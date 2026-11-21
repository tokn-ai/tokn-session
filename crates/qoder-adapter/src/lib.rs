//! Managed Qoder CLI interaction through ACP v1.
mod permissions;
mod session;
mod transport;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
  collections::BTreeMap,
  path::PathBuf,
  sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
};
use tokio::sync::{Mutex as AsyncMutex, broadcast, watch};
use tokn_acp_protocol as acp;
use tokn_interaction_core::*;

pub struct LaunchOptions {
  pub runtime_id: String,
  pub executable: PathBuf,
  pub arguments: Vec<String>,
  pub directory: PathBuf,
  pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct CreatedSession {
  pub target: SessionTarget,
  pub working_directory: PathBuf,
}

#[derive(Clone)]
pub struct QoderAdapter {
  inner: Arc<Inner>,
  _lease: Arc<Lease>,
}
struct Lease(Weak<Inner>);
impl Drop for Lease {
  fn drop(&mut self) {
    if let Some(inner) = self.0.upgrade() {
      inner.disconnect("Qoder adapter was dropped");
    }
  }
}

struct Prompt {
  token: String,
  admission: watch::Sender<Option<Result<Option<String>, InteractionError>>>,
}
struct Session {
  state: SessionState,
  models: Vec<ModelChoice>,
  prompt: Option<Prompt>,
}
struct Inner {
  descriptor: Mutex<RuntimeDescriptor>,
  directory: PathBuf,
  transport: transport::Transport,
  native: Mutex<acp::AgentCapabilities>,
  agent: Mutex<Option<acp::AgentInfo>>,
  sessions: Mutex<BTreeMap<String, Session>>,
  binding: AsyncMutex<()>,
  dispatch: Arc<AsyncMutex<()>>,
  permissions: Mutex<permissions::Permissions>,
  events: broadcast::Sender<RuntimeEvent>,
  sequence: AtomicU64,
  disconnected: AtomicBool,
}

impl QoderAdapter {
  pub async fn launch(options: LaunchOptions) -> Result<Self, InteractionError> {
    if options.runtime_id.is_empty() || !options.directory.is_absolute() || !options.directory.is_dir() {
      return Err(InteractionError::invalid(
        "Qoder requires a runtime ID and absolute project directory",
      ));
    }
    validate_arguments(&options.arguments)?;
    let directory = std::fs::canonicalize(&options.directory)
      .map_err(|_| InteractionError::invalid("Qoder project directory is unavailable"))?;
    let (transport, stdout) = transport::Transport::launch(&options)?;
    let (events, _) = broadcast::channel(256);
    let inner = Arc::new(Inner {
      descriptor: Mutex::new(RuntimeDescriptor {
        runtime_id: options.runtime_id,
        instance_id: uuid::Uuid::new_v4().to_string(),
        provider: Provider::Qoder,
        ownership: Ownership::Managed,
        capabilities: Capabilities::default(),
      }),
      directory,
      transport,
      native: Mutex::new(Default::default()),
      agent: Mutex::new(None),
      sessions: Mutex::new(BTreeMap::new()),
      binding: AsyncMutex::new(()),
      dispatch: Arc::new(AsyncMutex::new(())),
      permissions: Mutex::new(Default::default()),
      events,
      sequence: AtomicU64::new(1),
      disconnected: AtomicBool::new(false),
    });
    inner.transport.start_reader(stdout, Arc::downgrade(&inner));
    let initialized = inner
      .transport
      .call(
        "initialize",
        json!({
          "protocolVersion":acp::PROTOCOL_VERSION,
          "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},
          "clientInfo":{"name":"tokn-session","version":env!("CARGO_PKG_VERSION")}
        }),
        false,
      )
      .await;
    let initialization = initialized.and_then(|value| decode::<acp::InitializeResult>(value, false));
    let initialization = match initialization {
      Ok(result) if result.protocol_version == acp::PROTOCOL_VERSION => result,
      Ok(_) => {
        inner.transport.close().await?;
        return Err(InteractionError::unsupported(
          "Qoder requires an unsupported ACP version",
        ));
      }
      Err(error) => {
        let _ = inner.transport.close().await;
        return Err(error);
      }
    };
    let mut capabilities = Capabilities::default()
      .with(Operation::Metadata)
      .with(Operation::State)
      .with(Operation::Models)
      .with(Operation::Message)
      .with(Operation::CancelTurn)
      .with(Operation::PendingInteractions)
      .with(Operation::Respond);
    capabilities.image_input = initialization.agent_capabilities.prompt_capabilities.image;
    inner.descriptor.lock().unwrap().capabilities = capabilities;
    *inner.native.lock().unwrap() = initialization.agent_capabilities;
    *inner.agent.lock().unwrap() = initialization.agent_info;
    let lease = Arc::new(Lease(Arc::downgrade(&inner)));
    Ok(Self { inner, _lease: lease })
  }

  pub async fn create_session(&self, model: Option<ModelIdentity>) -> Result<CreatedSession, InteractionError> {
    self.inner.bind(None, model).await
  }
  pub async fn resume_session(&self, native_id: &str) -> Result<CreatedSession, InteractionError> {
    validate_id(native_id)?;
    self.inner.bind(Some(native_id), None).await
  }
  pub async fn close(&self) -> Result<(), InteractionError> {
    self.inner.disconnect("Qoder runtime was closed");
    self.inner.transport.close().await
  }
}

impl Inner {
  fn check_target(&self, target: &SessionTarget) -> Result<(), InteractionError> {
    let descriptor = self.descriptor.lock().unwrap();
    if descriptor.runtime_id != target.runtime_id || descriptor.instance_id != target.instance_id {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "Qoder runtime instance changed",
      ));
    }
    if !self.transport.is_live() {
      return Err(InteractionError::unavailable("Qoder runtime is disconnected"));
    }
    if target
      .session_id
      .as_ref()
      .is_some_and(|id| !self.sessions.lock().unwrap().contains_key(id))
    {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "Qoder session is not bound to this runtime",
      ));
    }
    Ok(())
  }
  fn target(&self, id: &str) -> SessionTarget {
    let descriptor = self.descriptor.lock().unwrap();
    SessionTarget {
      runtime_id: descriptor.runtime_id.clone(),
      instance_id: descriptor.instance_id.clone(),
      session_id: Some(id.into()),
    }
  }
  fn publish(&self, session_id: Option<String>, kind: RuntimeEventKind) {
    let descriptor = self.descriptor.lock().unwrap();
    let _ = self.events.send(RuntimeEvent {
      runtime_id: descriptor.runtime_id.clone(),
      instance_id: descriptor.instance_id.clone(),
      session_id,
      sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
      kind,
    });
  }
  fn publish_state(&self, id: &str) {
    let state = self
      .sessions
      .lock()
      .unwrap()
      .get(id)
      .map(|session| session.state.clone());
    if let Some(state) = state {
      self.publish(Some(id.into()), RuntimeEventKind::State(state));
    }
  }
  fn disconnect(&self, reason: &str) {
    self.transport.terminate();
    if self.disconnected.swap(true, Ordering::SeqCst) {
      return;
    }
    let mut sessions = self.sessions.lock().unwrap();
    for session in sessions.values_mut() {
      session.state.status = ExecutionState::Stopped;
      if let Some(prompt) = session.prompt.take() {
        prompt.admission.send_replace(Some(Err(InteractionError::uncertain(
          "Qoder disconnected before prompt completion",
        ))));
      }
    }
    drop(sessions);
    *self.permissions.lock().unwrap() = Default::default();
    self.publish(None, RuntimeEventKind::Disconnected { reason: reason.into() });
  }
  async fn receive(&self, frame: Value) -> Result<(), InteractionError> {
    if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
      return Err(InteractionError::unavailable("Qoder sent an invalid ACP envelope"));
    }
    if (frame.get("result").is_some() && frame.get("error").is_some())
      || (frame.get("method").is_some() && (frame.get("result").is_some() || frame.get("error").is_some()))
    {
      return Err(InteractionError::unavailable("Qoder sent an ambiguous ACP envelope"));
    }
    if let Some(id) = frame.get("id") {
      if frame.get("result").is_some() || frame.get("error").is_some() {
        self.transport.accept_reply(id, frame.clone());
        return Ok(());
      }
      if let Some(method) = frame.get("method").and_then(Value::as_str) {
        return self
          .receive_request(id.clone(), method, frame.get("params").cloned().unwrap_or(Value::Null))
          .await;
      }
    }
    if frame.get("method").and_then(Value::as_str) != Some("session/update") {
      return Ok(());
    }
    let update: acp::SessionUpdate = decode(frame.get("params").cloned().unwrap_or(Value::Null), false)?;
    self.observe_model_update(&update.session_id, &update.update)?;
    let token = self
      .sessions
      .lock()
      .unwrap()
      .get(&update.session_id)
      .and_then(|session| session.prompt.as_ref().map(|prompt| prompt.token.clone()));
    let kind = update.update.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
    if token.is_none() {
      // An owned process may start autonomous work. Do not keep claiming idle.
      if matches!(
        kind,
        "agent_message_chunk" | "agent_thought_chunk" | "tool_call" | "tool_call_update"
      ) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(&update.session_id) {
          if session.state.status == ExecutionState::Idle {
            session.state.status = ExecutionState::Unknown;
          }
        }
        drop(sessions);
        self.publish_state(&update.session_id);
      }
      return Ok(());
    }
    self.observe_permission_update(&update.session_id, &update.update);
    // Host-only live data includes a generation fence; public consumers must project safe fields.
    self.publish(
      Some(update.session_id),
      RuntimeEventKind::Native(json!({
        "protocol":"acp", "prompt_id":token, "update":update.update
      })),
    );
    Ok(())
  }
}

impl InteractionAdapter for QoderAdapter {
  fn descriptor(&self) -> RuntimeDescriptor {
    self.inner.descriptor.lock().unwrap().clone()
  }
  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.inner.events.subscribe()
  }
  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>> {
    Box::pin(QoderAdapter::close(self))
  }
  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Result<Reply, InteractionError>> {
    Box::pin(async move {
      self.inner.check_target(&request.target)?;
      let id = request.target.session_id.as_deref();
      match request.command {
        Command::Metadata => {
          let agent = self.inner.agent.lock().unwrap();
          Ok(Reply::Metadata(Metadata {
            name: agent.as_ref().map(|a| a.name.clone()),
            version: agent.as_ref().and_then(|a| a.version.clone()),
            model: id.and_then(|id| {
              self
                .inner
                .sessions
                .lock()
                .unwrap()
                .get(id)
                .and_then(|s| s.state.model.clone())
            }),
            ..Default::default()
          }))
        }
        Command::Models => {
          let sessions = self.inner.sessions.lock().unwrap();
          let session = if let Some(id) = id {
            sessions.get(id)
          } else {
            sessions.values().next()
          };
          Ok(Reply::Models(
            session
              .ok_or_else(|| {
                InteractionError::unsupported("Connect a Qoder session before inspecting its model catalog")
              })?
              .models
              .clone(),
          ))
        }
        Command::State => Ok(Reply::State(
          self
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(required_session(id)?)
            .unwrap()
            .state
            .clone(),
        )),
        Command::Message { input, delivery } => {
          self
            .inner
            .message(required_session(id)?, input, delivery, request.request_id)
            .await
        }
        Command::CancelTurn => self.inner.cancel(required_session(id)?).await,
        Command::PendingInteractions => Ok(Reply::Interactions(
          self.inner.pending_permissions(required_session(id)?),
        )),
        Command::Respond {
          interaction_id,
          response,
        } => {
          self
            .inner
            .respond_permission(required_session(id)?, &interaction_id, response)
            .await
        }
      }
    })
  }
}

fn required_session(id: Option<&str>) -> Result<&str, InteractionError> {
  id.ok_or_else(|| InteractionError::invalid("Qoder operation requires a bound session"))
}
fn validate_id(id: &str) -> Result<(), InteractionError> {
  if id.is_empty()
    || id.len() > 256
    || id.chars().any(|c| c.is_control() || c == '/' || c == '\\')
    || matches!(id, "." | "..")
  {
    Err(InteractionError::invalid("Invalid Qoder session identity"))
  } else {
    Ok(())
  }
}
fn decode<T: DeserializeOwned>(value: Value, mutation: bool) -> Result<T, InteractionError> {
  serde_json::from_value(value).map_err(|_| {
    if mutation {
      InteractionError::uncertain("Qoder returned an invalid mutation response")
    } else {
      InteractionError::unavailable("Qoder returned an invalid ACP response")
    }
  })
}

fn validate_arguments(arguments: &[String]) -> Result<(), InteractionError> {
  const FLAGS: &[&str] = &[
    "--acp",
    "--cwd",
    "--workspace",
    "--config-dir",
    "--config",
    "--settings",
    "--settings-file",
    "--print",
    "--prompt",
    "--prompt-interactive",
    "--resume",
    "--continue",
    "--session-id",
    "--fork-session",
    "--worktree",
    "--remote",
    "--remote-session",
    "--attach",
    "--input-format",
    "--output-format",
  ];
  if arguments.iter().any(|argument| {
    let flag = argument.split('=').next().unwrap_or(argument);
    argument == "--"
      || FLAGS.contains(&flag)
      || ["-w", "-c", "-r", "-p", "-i"]
        .iter()
        .any(|prefix| argument.starts_with(prefix) && !argument.starts_with("--"))
  }) {
    Err(InteractionError::invalid(
      "Qoder managed ACP arguments cannot override protocol, project, storage, or session startup",
    ))
  } else {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn rejects_native_storage_and_session_overrides_including_attached_short_flags() {
    for flag in [
      "--acp=false",
      "--cwd=other",
      "--workspace",
      "--config-dir",
      "--resume",
      "--fork-session",
      "-wother",
      "-pTASK",
      "-rID",
      "-c",
      "--",
    ] {
      assert!(validate_arguments(&[flag.into()]).is_err(), "{flag}");
    }
    assert!(validate_arguments(&["fixture.js".into(), "--verbose".into()]).is_ok());
  }
}
