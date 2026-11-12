//! Managed interaction with the native ZCode Protocol app-server.
mod transport;
mod wire;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
  collections::{BTreeMap, HashSet},
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, broadcast, watch};
use tokn_interaction_core::*;
use transport::Transport;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SESSIONS: usize = 128;

pub struct LaunchOptions {
  pub runtime_id: String,
  pub executable: PathBuf,
  /// Launcher prefix, such as the path to the native JavaScript bundle for Node.
  pub arguments: Vec<String>,
  pub directory: PathBuf,
  /// Exact database identity expected from native startup before admitting any mutations.
  pub history_path: PathBuf,
  /// Explicit overrides for the child, inherited from the trusted host profile.
  pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct CreatedSession {
  pub target: SessionTarget,
  pub working_directory: PathBuf,
}

pub fn new_session_id() -> String {
  format!("sess_{}", uuid::Uuid::new_v4())
}

struct Inner {
  descriptor: RuntimeDescriptor,
  directory: PathBuf,
  history_path: PathBuf,
  transport: Transport,
  storage: watch::Sender<Option<bool>>,
  events: broadcast::Sender<RuntimeEvent>,
  sequence: Mutex<u64>,
  disconnected: AtomicBool,
  // Serialize residency transitions while allowing independent controls.
  binding: AsyncMutex<()>,
  sessions: Mutex<HashSet<String>>,
  active_prompts: Mutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct ZCodeAdapter {
  inner: Arc<Inner>,
}

impl ZCodeAdapter {
  pub async fn launch(options: LaunchOptions) -> Result<Self, InteractionError> {
    if options.runtime_id.trim().is_empty()
      || !options.directory.is_absolute()
      || !options.directory.is_dir()
      || options.directory.to_str().is_none()
      || !options.history_path.is_absolute()
      || options.history_path.to_str().is_none()
    {
      return Err(InteractionError::invalid(
        "ZCode requires an existing absolute working directory and an absolute database path",
      ));
    }
    let (transport, stdout) = Transport::launch(&options)?;
    let (events, _) = broadcast::channel(128);
    let (storage, mut verified) = watch::channel(None);
    let inner = Arc::new(Inner {
      descriptor: RuntimeDescriptor {
        runtime_id: options.runtime_id,
        instance_id: uuid::Uuid::new_v4().to_string(),
        provider: Provider::ZCode,
        ownership: Ownership::Managed,
        capabilities: Capabilities::default()
          .with(Operation::Metadata)
          .with(Operation::State)
          .with(Operation::Models)
          .with(Operation::Message)
          .with(Operation::CancelTurn)
          .with(Operation::PendingInteractions),
      },
      directory: options.directory,
      history_path: options.history_path,
      transport,
      storage,
      events,
      sequence: Mutex::new(0),
      disconnected: AtomicBool::new(false),
      binding: AsyncMutex::new(()),
      sessions: Mutex::new(HashSet::new()),
      active_prompts: Mutex::new(HashSet::new()),
    });
    inner.transport.start_reader(stdout, Arc::downgrade(&inner));
    let startup = tokio::time::timeout(STARTUP_TIMEOUT, async {
      let capabilities = inner
        .transport
        .call(
          "runtime/capabilities",
          json!({"hostCapabilities": {
            "sessionDirectorySource": true, "sessionWorkspaceRouting": true
          }}),
          false,
        )
        .await?;
      if capabilities["sessionLifecycleWorkspace"] != true || capabilities["sessionWorkspaceRouting"] != true {
        return Err(InteractionError::unavailable(
          "ZCode app-server does not support verified workspace activation",
        ));
      }
      if !*verified
        .wait_for(|value| value.is_some())
        .await
        .map_err(|_| InteractionError::unavailable("ZCode app-server disconnected before database verification"))?
        .as_ref()
        .expect("storage watch predicate")
      {
        return Err(InteractionError::unavailable(
          "ZCode app-server selected a different history database or could not initialize it",
        ));
      }
      if !inner.transport.is_live() {
        return Err(InteractionError::unavailable(
          "ZCode app-server disconnected during startup",
        ));
      }
      Ok(())
    })
    .await
    .unwrap_or_else(|_| {
      Err(InteractionError::unavailable(
        "ZCode app-server did not verify its history database before the startup deadline",
      ))
    });
    if let Err(error) = startup {
      let _ = inner.transport.close().await;
      return Err(error);
    }
    Ok(Self { inner })
  }

  /// Materialize an empty native session at the durable caller-reserved ID.
  /// ZCode writes the history row when a first turn starts.
  pub async fn create_session_with_id(
    &self,
    session_id: &str,
    model: Option<ModelIdentity>,
  ) -> Result<CreatedSession, InteractionError> {
    validate_id(session_id)?;
    let _guard = self.inner.binding.lock().await;
    self.inner.check_capacity(session_id)?;
    let mut params = json!({
      "sessionId": session_id,
      "workspace": self.inner.workspace(),
      "persistence": "immediate",
    });
    if let Some(model) = model {
      params["model"] = serde_json::to_value(wire::ModelSelection::from_model(model)?)
        .map_err(|_| InteractionError::invalid("Invalid ZCode model selection"))?;
    }
    let snapshot: wire::Snapshot =
      wire::decode(self.inner.transport.call("session/create", params, true).await?, true)?;
    self.inner.verify_snapshot(&snapshot, session_id, true)?;
    self.inner.bind(session_id).await.map_err(|_| {
      InteractionError::uncertain("ZCode created a session but its live event subscription could not be verified")
    })
  }

  pub async fn resume_session(&self, session_id: &str) -> Result<CreatedSession, InteractionError> {
    validate_id(session_id)?;
    let _guard = self.inner.binding.lock().await;
    self.inner.check_capacity(session_id)?;
    self.inner.lookup_session(session_id).await?;
    // This RPC avoids returning the entire restored transcript.
    let result = self
      .inner
      .transport
      .call(
        "session/lifecycle/ensureReady",
        json!({
          "sessionId": session_id, "workspace": self.inner.workspace(),
        }),
        true,
      )
      .await?;
    if result["completed"] != true {
      return Err(InteractionError::uncertain("ZCode did not confirm session activation"));
    }
    let snapshot = self.inner.snapshot(session_id).await?;
    let created = self.inner.bind(session_id).await?;
    self.inner.publish(
      Some(session_id.into()),
      RuntimeEventKind::State(self.inner.session_state(&snapshot)),
    );
    Ok(created)
  }
}

impl Inner {
  fn workspace(&self) -> Value {
    json!({"workspaceKey": self.directory, "workspacePath": self.directory})
  }

  fn check_capacity(&self, id: &str) -> Result<(), InteractionError> {
    let sessions = self.sessions.lock().unwrap();
    if !sessions.contains(id) && sessions.len() >= MAX_SESSIONS {
      return Err(InteractionError::unavailable(
        "ZCode app-server has reached its managed-session limit",
      ));
    }
    Ok(())
  }

  fn target(&self, id: &str) -> SessionTarget {
    SessionTarget {
      runtime_id: self.descriptor.runtime_id.clone(),
      instance_id: self.descriptor.instance_id.clone(),
      session_id: Some(id.to_owned()),
    }
  }

  fn verify_session(&self, session: &wire::NativeSession, id: &str) -> Result<(), InteractionError> {
    if session.session_id != id
      || !same_absolute_path(&self.directory, Path::new(&session.workspace.workspace_path))
      || !same_absolute_path(&self.directory, Path::new(&session.workspace.workspace_key))
      || session
        .workspace
        .workspace_identity
        .as_ref()
        .is_some_and(|value| !same_absolute_path(&self.directory, Path::new(value)))
      || matches!(
        session.session_kind.as_str(),
        "subagent_child" | "workflow_child" | "nested_workflow_child"
      )
    {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "ZCode returned a different session or a session outside this local working directory",
      ));
    }
    Ok(())
  }

  fn verify_snapshot(&self, snapshot: &wire::Snapshot, id: &str, mutation: bool) -> Result<(), InteractionError> {
    if snapshot.protocol.name != "ZCode Protocol"
      || snapshot.protocol.version != 1
      || !snapshot.projection.matches_session(id)
      || self.verify_session(&snapshot.session, id).is_err()
    {
      return Err(if mutation {
        InteractionError::uncertain("ZCode created a session but reported a different identity or protocol")
      } else {
        InteractionError::new(
          ErrorKind::StaleTarget,
          "ZCode returned a different session identity or protocol",
        )
      });
    }
    Ok(())
  }

  async fn lookup_session(&self, id: &str) -> Result<(), InteractionError> {
    let sessions: wire::SessionDirectory = wire::decode(
      self
        .transport
        .call("session/list", json!({"sessionIds": [id]}), false)
        .await?,
      false,
    )?;
    let [session] = sessions.sessions.as_slice() else {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "ZCode session is absent from this local history database and working directory",
      ));
    };
    self.verify_session(session, id)
  }

  async fn snapshot(&self, id: &str) -> Result<wire::Snapshot, InteractionError> {
    let snapshot: wire::Snapshot = wire::decode(
      self
        .transport
        .call(
          "session/read",
          json!({
            "sessionId": id, "messageLimit": 1, "contentProfile": "index"
          }),
          false,
        )
        .await?,
      false,
    )?;
    self.verify_snapshot(&snapshot, id, false)?;
    Ok(snapshot)
  }

  fn session_state(&self, snapshot: &wire::Snapshot) -> SessionState {
    let mut state = snapshot.state();
    if self
      .active_prompts
      .lock()
      .unwrap()
      .contains(&snapshot.session.session_id)
    {
      state.status = ExecutionState::Running;
    }
    state
  }

  async fn bind(&self, id: &str) -> Result<CreatedSession, InteractionError> {
    let session_is_new = self.sessions.lock().unwrap().insert(id.to_owned());
    if session_is_new {
      let subscribe = self
        .transport
        .call(
          "session/subscribe",
          json!({
            "sessionId": id, "deliveryKind": "desktop-continuous", "includeSnapshot": false
          }),
          false,
        )
        .await;
      let subscribed = match subscribe {
        Ok(result) if result["sessionId"].as_str() == Some(id) => Ok(()),
        Ok(_) => Err(InteractionError::unavailable(
          "ZCode returned a different event subscription identity",
        )),
        Err(error) => Err(error),
      };
      if let Err(error) = subscribed {
        self.sessions.lock().unwrap().remove(id);
        return Err(error);
      }
    }
    Ok(CreatedSession {
      target: self.target(id),
      working_directory: self.directory.clone(),
    })
  }

  fn check_target<'a>(&self, target: &'a SessionTarget) -> Result<&'a str, InteractionError> {
    if target.runtime_id != self.descriptor.runtime_id || target.instance_id != self.descriptor.instance_id {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "The ZCode runtime instance changed",
      ));
    }
    if !self.transport.is_live() {
      return Err(InteractionError::unavailable("ZCode app-server is disconnected"));
    }
    target
      .session_id
      .as_deref()
      .filter(|id| self.sessions.lock().unwrap().contains(*id))
      .ok_or_else(|| InteractionError::new(ErrorKind::StaleTarget, "ZCode is not bound to this native session"))
  }

  fn publish(&self, session_id: Option<String>, kind: RuntimeEventKind) {
    let mut sequence = self.sequence.lock().unwrap();
    if self.disconnected.load(Ordering::Acquire) && !matches!(kind, RuntimeEventKind::Disconnected { .. }) {
      return;
    }
    *sequence += 1;
    let _ = self.events.send(RuntimeEvent {
      runtime_id: self.descriptor.runtime_id.clone(),
      instance_id: self.descriptor.instance_id.clone(),
      session_id,
      sequence: *sequence,
      kind,
    });
  }

  fn disconnect(&self, reason: &str) {
    self.transport.terminate();
    self.active_prompts.lock().unwrap().clear();
    self.storage.send_if_modified(|storage| {
      if storage.is_none() {
        *storage = Some(false);
        true
      } else {
        false
      }
    });
    if !self.disconnected.swap(true, Ordering::AcqRel) {
      self.publish(None, RuntimeEventKind::Disconnected { reason: reason.into() });
    }
  }

  async fn receive(&self, frame: Value) -> Result<(), InteractionError> {
    let Some(method) = frame["method"].as_str() else {
      if let Some(id) = frame.get("id").cloned() {
        self.transport.accept_reply(&id, frame);
        return Ok(());
      }
      return Err(InteractionError::unavailable(
        "ZCode returned an invalid app-server envelope",
      ));
    };
    let params = &frame["params"];
    if let Some(id) = frame.get("id") {
      return self.transport.send(decline_callback(id, method)?, false).await;
    }
    if method == "startup/storageState" && params["databaseKind"] == "session" {
      if params["phase"] == "ready" {
        let actual = params["databaseId"].as_str();
        let expected = database_hash(&self.history_path);
        let canonical = self
          .history_path
          .canonicalize()
          .ok()
          .and_then(|path| database_hash(&path));
        let matches = actual.is_some() && (actual == expected.as_deref() || actual == canonical.as_deref());
        self.storage.send_replace(Some(matches));
      } else if params["phase"] == "failed" {
        self.storage.send_replace(Some(false));
      }
    }
    if let Some(id) = params["sessionId"]
      .as_str()
      .filter(|id| self.sessions.lock().unwrap().contains(*id))
    {
      match (method, params["type"].as_str()) {
        ("session/event", Some("turn.started")) => self.publish_state(id, ExecutionState::Running),
        ("state.updated", _) => match params["reason"].as_str() {
          Some("prompt_started") => {
            self.active_prompts.lock().unwrap().insert(id.into());
            self.publish_state(id, ExecutionState::Running);
          }
          // Core completion precedes bootstrap releasing its active prompt lock.
          // Only these native ready notifications allow another session/send.
          Some("prompt_completed" | "prompt_failed") => {
            self.active_prompts.lock().unwrap().remove(id);
            self.publish_state(id, ExecutionState::Idle);
          }
          _ => {}
        },
        ("session/event", Some("session.closed")) => {
          self.sessions.lock().unwrap().remove(id);
          self.active_prompts.lock().unwrap().remove(id);
          self.publish_state(id, ExecutionState::Stopped);
        }
        _ => {}
      }
    }
    Ok(())
  }

  fn publish_state(&self, id: &str, status: ExecutionState) {
    self.publish(
      Some(id.into()),
      RuntimeEventKind::State(SessionState {
        session_id: Some(id.into()),
        status,
        model: None,
        pending_inputs: None,
        revision: None,
      }),
    );
  }

  async fn message(
    &self,
    id: &str,
    request_id: &str,
    input: MessageInput,
    delivery: DeliveryMode,
  ) -> Result<Reply, InteractionError> {
    if !input.images.is_empty() {
      return Err(InteractionError::unsupported("ZCode image upload is unavailable"));
    }
    if input.text.trim().is_empty()
      || input.text.chars().count() > tokn_interaction_core::admission::MAX_INPUT_LENGTH
      || input
        .text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
      return Err(InteractionError::invalid("Invalid ZCode message text"));
    }
    if request_id.trim().is_empty() || request_id.len() > 128 {
      return Err(InteractionError::invalid(
        "ZCode message request requires a bounded request ID",
      ));
    }
    if delivery != DeliveryMode::Start {
      return Err(InteractionError::unsupported(
        "ZCode only supports starting a turn while idle",
      ));
    }
    let mut params = json!({"sessionId": id, "inputId": request_id, "content": input.text});
    if let Some(revision) = input.expected_revision {
      let revision = revision
        .parse::<u64>()
        .map_err(|_| InteractionError::invalid("Invalid ZCode session revision"))?;
      params["expectedRevision"] = json!(revision);
    }
    if let Some(model) = input.model.clone() {
      params["modelSelection"] = json!(wire::ModelSelection::from_model(model)?);
    }
    let admission: wire::Admission = wire::decode(self.transport.call("session/send", params, true).await?, true)?;
    if admission.session_id != id || !admission.accepted {
      return Err(InteractionError::uncertain(
        "ZCode returned an unverified message admission",
      ));
    }
    Ok(Reply::Admission(InputReceipt {
      disposition: InputDisposition::Started,
      model: input.model,
      model_ignored: false,
      native_id: Some(request_id.to_owned()),
    }))
  }
}

impl InteractionAdapter for ZCodeAdapter {
  fn descriptor(&self) -> RuntimeDescriptor {
    self.inner.descriptor.clone()
  }

  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Result<Reply, InteractionError>> {
    Box::pin(async move {
      if request.target.session_id.is_none()
        && request.target.runtime_id == self.inner.descriptor.runtime_id
        && request.target.instance_id == self.inner.descriptor.instance_id
        && self.inner.transport.is_live()
        && matches!(request.command, Command::Metadata)
      {
        return Ok(Reply::Metadata(Metadata {
          name: Some("ZCode".into()),
          ..Metadata::default()
        }));
      }
      let id = self.inner.check_target(&request.target)?;
      match request.command {
        Command::Metadata => {
          let snapshot = self.inner.snapshot(id).await?;
          Ok(Reply::Metadata(Metadata {
            name: Some("ZCode".into()),
            model: snapshot.state().model,
            commands: snapshot.commands.into_iter().map(|command| command.name).collect(),
            ..Metadata::default()
          }))
        }
        Command::State => Ok(Reply::State(self.inner.session_state(&self.inner.snapshot(id).await?))),
        Command::Models => Ok(Reply::Models(self.inner.snapshot(id).await?.models())),
        Command::Message { input, delivery } => self.inner.message(id, &request.request_id, input, delivery).await,
        Command::CancelTurn => {
          let result = self
            .inner
            .transport
            .call("session/stop", json!({"sessionId": id}), true)
            .await?;
          if result.as_object().is_none_or(|object| !object.is_empty()) {
            return Err(InteractionError::uncertain("ZCode did not confirm the stop request"));
          }
          Ok(Reply::Acknowledged)
        }
        Command::PendingInteractions => Ok(Reply::Interactions(Vec::new())),
        Command::Respond { .. } => Err(InteractionError::unsupported(
          "ZCode interaction responses are unavailable",
        )),
      }
    })
  }

  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.inner.events.subscribe()
  }

  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>> {
    Box::pin(async move {
      self.inner.disconnect("The managed ZCode runtime was closed");
      self.inner.transport.close().await
    })
  }
}

fn decline_callback(id: &Value, method: &str) -> Result<Value, InteractionError> {
  if !id.is_string() && !id.is_number() {
    return Err(InteractionError::unavailable("ZCode returned an invalid callback ID"));
  }
  // The viewer has no ZCode permission or question surface. Deny or decline
  // these reverse requests so native turns do not wait for an invisible dialog.
  Ok(match method {
    "interaction/requestPermission" => json!({"id": id, "result": {"decision": "deny"}}),
    "interaction/requestUserInput" => json!({"id": id, "result": {"action": "decline"}}),
    _ => json!({"id": id, "error": {"code": -32601, "message": "Host operation unavailable"}}),
  })
}

fn validate_id(id: &str) -> Result<(), InteractionError> {
  if !id.starts_with("sess_")
    || id.len() == "sess_".len()
    || id.len() > 128
    || !id
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
  {
    return Err(InteractionError::invalid("ZCode requires a valid native session ID"));
  }
  Ok(())
}

fn database_hash(path: &Path) -> Option<String> {
  Some(format!("{:x}", Sha256::digest(path.to_str()?.as_bytes())))
}

fn same_absolute_path(expected: &Path, actual: &Path) -> bool {
  actual.is_absolute()
    && (expected == actual
      || actual
        .canonicalize()
        .ok()
        .zip(expected.canonicalize().ok())
        .is_some_and(|(actual, expected)| actual == expected))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[cfg(unix)]
  fn fixture_options(database_id: Option<&str>) -> LaunchOptions {
    let directory = std::env::current_dir().unwrap();
    let history_path = directory.join("example.sqlite");
    let script = r#"
      [ "$0" = app-server ] && [ "$1" = --stdio ] || exit 2
      printf '{"method":"startup/storageState","params":{"databaseKind":"session","phase":"ready","databaseId":"%s"}}\n' "$FIXTURE_DATABASE_ID"
      IFS= read -r request || exit 3
      case "$request" in *'"method":"runtime/capabilities"'*) ;; *) exit 4 ;; esac
      printf '%s\n' '{"id":1,"result":{"sessionLifecycleWorkspace":true,"sessionWorkspaceRouting":true}}'
      while IFS= read -r request; do :; done
    "#;
    LaunchOptions {
      runtime_id: "fixture-runtime".into(),
      executable: "/bin/sh".into(),
      arguments: vec!["-c".into(), script.into()],
      directory: directory.clone(),
      history_path: history_path.clone(),
      environment: BTreeMap::from([(
        "FIXTURE_DATABASE_ID".into(),
        database_id
          .map(str::to_owned)
          .unwrap_or_else(|| database_hash(&history_path).unwrap()),
      )]),
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn verifies_the_native_storage_identity_before_exposing_the_runtime() {
    let adapter = ZCodeAdapter::launch(fixture_options(None)).await.unwrap();
    assert_eq!(adapter.descriptor().provider, Provider::ZCode);
    adapter.close().await.unwrap();
    let mismatched = ZCodeAdapter::launch(fixture_options(Some("unexpected-native-database"))).await;
    let Err(error) = mismatched else {
      panic!("unexpected database must fail runtime activation");
    };
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert!(error.message.contains("history database"));
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn waits_for_the_native_ready_boundary_after_turn_completion_and_failure() {
    let adapter = ZCodeAdapter::launch(fixture_options(None)).await.unwrap();
    adapter.inner.sessions.lock().unwrap().insert("sess_fixture".into());
    let snapshot: wire::Snapshot = wire::decode(
      json!({
        "protocol": {"name":"ZCode Protocol","version":1},
        "session": {
          "sessionId":"sess_fixture", "sessionKind":"interactive",
          "workspace":{"workspacePath":"/fixture","workspaceKey":"/fixture"}
        },
        "projection":{"sessionId":"sess_fixture","status":"idle","turnCount":1},
        "runtime":{"stateRevision":1}, "settings":{"model":{"available":[]}}
      }),
      false,
    )
    .unwrap();
    let mut events = adapter.subscribe();
    for terminal in ["turn.completed", "turn.failed"] {
      adapter
        .inner
        .receive(json!({"method":"state.updated","params":{
          "sessionId":"sess_fixture", "reason":"prompt_started"
        }}))
        .await
        .unwrap();
      assert!(matches!(
        events.recv().await.unwrap().kind,
        RuntimeEventKind::State(SessionState {
          status: ExecutionState::Running,
          ..
        })
      ));
      adapter
        .inner
        .receive(json!({"method":"session/event","params":{
          "sessionId":"sess_fixture", "type":terminal
        }}))
        .await
        .unwrap();
      assert!(matches!(events.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
      assert_eq!(adapter.inner.session_state(&snapshot).status, ExecutionState::Running);
      let reason = if terminal == "turn.completed" {
        "prompt_completed"
      } else {
        "prompt_failed"
      };
      adapter
        .inner
        .receive(json!({"method":"state.updated","params":{
          "sessionId":"sess_fixture", "reason":reason
        }}))
        .await
        .unwrap();
      assert!(matches!(
        events.recv().await.unwrap().kind,
        RuntimeEventKind::State(SessionState {
          status: ExecutionState::Idle,
          ..
        })
      ));
      assert_eq!(adapter.inner.session_state(&snapshot).status, ExecutionState::Idle);
    }
    // A bootstrap failure can finish without producing any core terminal event.
    adapter
      .inner
      .receive(json!({"method":"state.updated","params":{
        "sessionId":"sess_fixture", "reason":"prompt_failed"
      }}))
      .await
      .unwrap();
    assert!(matches!(
      events.recv().await.unwrap().kind,
      RuntimeEventKind::State(SessionState {
        status: ExecutionState::Idle,
        ..
      })
    ));
    adapter
      .inner
      .active_prompts
      .lock()
      .unwrap()
      .insert("sess_fixture".into());
    adapter
      .inner
      .receive(json!({"method":"session/event","params":{
        "sessionId":"sess_fixture", "type":"session.closed"
      }}))
      .await
      .unwrap();
    assert!(adapter.inner.active_prompts.lock().unwrap().is_empty());
    adapter.close().await.unwrap();
  }

  #[test]
  fn callback_rejections_keep_native_correlation_without_authorizing_host_actions() {
    let permission = decline_callback(&json!("callback-7"), "interaction/requestPermission").unwrap();
    assert_eq!(permission, json!({"id":"callback-7","result":{"decision":"deny"}}));
    let input = decline_callback(&json!(42), "interaction/requestUserInput").unwrap();
    assert_eq!(input, json!({"id":42,"result":{"action":"decline"}}));
    let browser = decline_callback(&json!("another"), "interaction/browserExecute").unwrap();
    assert_eq!(browser["error"]["code"], -32601);
    assert!(decline_callback(&Value::Null, "interaction/requestUserInput").is_err());
  }

  #[test]
  fn reserves_native_ids_and_rejects_path_like_ids() {
    assert!(validate_id(&new_session_id()).is_ok());
    assert!(validate_id("sess_../../outside").is_err());
    assert!(validate_id("session-1111").is_err());
  }
}
