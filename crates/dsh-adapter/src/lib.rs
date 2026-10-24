//! Attached DSH ApiProxy interactions. Closing detaches; it never stops the host.
mod events;
mod managed;
mod responses;
mod transport;

pub use managed::ManagedOptions;
use serde_json::{Value, json};
use std::{
  collections::BTreeMap,
  path::PathBuf,
  sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  time::Duration,
};
use tokio::{
  sync::{Mutex, broadcast, watch},
  task::JoinHandle,
};
use tokn_dsh_rpc_protocol::api;
use tokn_interaction_core::*;
use transport::Transport;

#[derive(Clone)]
pub struct ApiProxyConfig {
  pub endpoint: String,
  pub runtime_id: String,
  /// Optional endpoint authentication. Never included in descriptors or errors.
  pub bearer_token: Option<String>,
  pub connect_timeout: Duration,
  pub request_timeout: Duration,
  pub max_body_bytes: usize,
  pub max_frame_bytes: usize,
  /// RFC WebSocket control-frame liveness; native downlinks accept no application messages.
  pub heartbeat_interval: Duration,
  pub heartbeat_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct CreateSessionOptions {
  pub session_id: String,
  pub working_directory: PathBuf,
  /// Omitted uses the native host's effective default composition.
  pub agent_preset: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CreatedSession {
  pub target: SessionTarget,
  pub working_directory: PathBuf,
  pub agent_preset: Option<String>,
}

pub fn new_session_id() -> String {
  format!("session-{}", uuid::Uuid::new_v4())
}

impl ApiProxyConfig {
  pub fn new(endpoint: impl Into<String>) -> Self {
    Self {
      endpoint: endpoint.into(),
      runtime_id: format!("dsh:{}", uuid::Uuid::new_v4()),
      bearer_token: None,
      connect_timeout: Duration::from_secs(5),
      request_timeout: Duration::from_secs(15),
      max_body_bytes: 16 * 1024 * 1024,
      max_frame_bytes: 2 * 1024 * 1024,
      heartbeat_interval: Duration::from_secs(15),
      heartbeat_timeout: Duration::from_secs(15),
    }
  }
}

#[derive(Default)]
struct StreamState {
  pending: BTreeMap<String, PendingInteraction>,
  sessions: BTreeMap<String, SessionState>,
}

struct Inner {
  descriptor: RuntimeDescriptor,
  transport: Transport,
  state: Mutex<StreamState>,
  events: broadcast::Sender<RuntimeEvent>,
  sequence: AtomicU64,
  publication: StdMutex<()>,
  disconnected: AtomicBool,
  closed: AtomicBool,
  shutdown: watch::Sender<bool>,
  tasks: StdMutex<Vec<JoinHandle<()>>>,
  managed: StdMutex<Option<managed::OwnedRuntime>>,
}

pub struct DshAdapter {
  inner: Arc<Inner>,
}

impl DshAdapter {
  /// Create a real idle agent at a preallocated identity. The adapter never
  /// retries this mutation and never sends an initial prompt implicitly.
  pub async fn create_session(&self, options: CreateSessionOptions) -> Result<CreatedSession, InteractionError> {
    self.inner.ensure_live()?;
    if options.session_id.is_empty()
      || options.session_id.len() > 512
      || !options
        .session_id
        .bytes()
        .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
      || !options.working_directory.is_absolute()
      || options
        .working_directory
        .to_str()
        .is_none_or(|path| path.contains('\0'))
      || options
        .agent_preset
        .as_ref()
        .is_some_and(|preset| preset.trim().is_empty() || preset.len() > 512 || preset.chars().any(char::is_control))
    {
      return Err(InteractionError::invalid(
        "DSH creation requires a safe native identity, absolute working directory, and valid preset",
      ));
    }
    let payload = serde_json::to_value(api::SessionCreateRequest {
      session_id: options.session_id.clone(),
      cwd: options.working_directory.to_string_lossy().into_owned(),
      agent_preset: options.agent_preset.clone(),
    })
    .map_err(|_| InteractionError::invalid("Invalid DSH creation request"))?;
    let created: api::SessionCreateResponse = self
      .inner
      .transport
      .call(&uuid::Uuid::new_v4().to_string(), "session.create", payload, true)
      .await?;
    if created.session_id != options.session_id
      || created
        .agent_preset
        .as_ref()
        .is_some_and(|preset| preset.trim().is_empty() || preset.len() > 512 || preset.chars().any(char::is_control))
      || options
        .agent_preset
        .as_ref()
        .is_some_and(|preset| created.agent_preset.as_ref() != Some(preset))
    {
      return Err(InteractionError::uncertain(
        "DSH may have created a session, but returned an unexpected identity or preset",
      ));
    }
    self
      .inner
      .ensure_live()
      .map_err(|_| InteractionError::uncertain("DSH created a session, but its live connection was lost"))?;
    // session.create returns no cwd or history path. Verify location through
    // the non-activating directory; never guess a history filename.
    let sessions: api::SessionList = self
      .inner
      .transport
      .call(&uuid::Uuid::new_v4().to_string(), "session.list", json!({}), false)
      .await
      .map_err(|_| {
        InteractionError::uncertain("DSH created a session, but its working directory could not be verified")
      })?;
    let mut matches = sessions
      .items
      .into_iter()
      .filter(|entry| entry.session_id == options.session_id);
    let matched = matches.next().ok_or_else(|| {
      InteractionError::uncertain("DSH created a session, but it is absent from the native directory")
    })?;
    if matches.next().is_some()
      || matched.cwd.as_deref().map(PathBuf::from).as_ref() != Some(&options.working_directory)
    {
      return Err(InteractionError::uncertain(
        "DSH created a session, but its native working directory does not match",
      ));
    }
    self
      .inner
      .ensure_live()
      .map_err(|_| InteractionError::uncertain("DSH created a session, but its live connection was lost"))?;
    Ok(CreatedSession {
      target: SessionTarget {
        runtime_id: self.inner.descriptor.runtime_id.clone(),
        instance_id: self.inner.descriptor.instance_id.clone(),
        session_id: Some(created.session_id),
      },
      working_directory: options.working_directory,
      agent_preset: created.agent_preset,
    })
  }

  /// Attach an explicitly configured ApiProxy endpoint. No sessions are resumed
  /// by connection, metadata/state/model queries, or pending-interaction reads.
  pub async fn attach(config: ApiProxyConfig) -> Result<Self, InteractionError> {
    Self::connect(config, Ownership::Attached).await
  }

  /// Start a fresh native Web profile on an OS-assigned loopback port. Readiness
  /// comes only from this owned child's native URL line, never port probing.
  pub async fn launch(options: ManagedOptions) -> Result<Self, InteractionError> {
    let mut launch = managed::Launch::start(&options).await?;
    let mut config = ApiProxyConfig::new(launch.endpoint.clone());
    config.runtime_id = options.runtime_id.clone();
    config.request_timeout = options.request_timeout;
    config.connect_timeout = options.connect_timeout;
    let adapter = Self::connect(config, Ownership::Managed).await?;
    let runtime = launch.supervise(adapter.inner.clone(), options.shutdown_timeout);
    *adapter.inner.managed.lock().expect("DSH managed lock") = Some(runtime);
    Ok(adapter)
  }

  async fn connect(config: ApiProxyConfig, ownership: Ownership) -> Result<Self, InteractionError> {
    if config.runtime_id.trim().is_empty() {
      return Err(InteractionError::invalid("DSH runtime_id is required"));
    }
    let max_frame_bytes = config.max_frame_bytes;
    let heartbeat_interval = config.heartbeat_interval;
    let heartbeat_timeout = config.heartbeat_timeout;
    let runtime_id = config.runtime_id.clone();
    let transport = Transport::new(config)?;
    let mux = transport.stream("api/events.mux").await?;
    let host = transport.stream("api/events.host").await?;
    let _: api::HostDescription = transport
      .call(&uuid::Uuid::new_v4().to_string(), "host.describe", json!({}), false)
      .await?;
    let (events, _) = broadcast::channel(256);
    let (shutdown, _) = watch::channel(false);
    let mut capabilities = Capabilities::default()
      .with(Operation::Metadata)
      .with(Operation::State)
      .with(Operation::Models)
      .with(Operation::Message)
      .with(Operation::Queue)
      .with(Operation::CancelTurn)
      .with(Operation::Respond)
      .with(Operation::PendingInteractions);
    capabilities.operations.insert(
      Operation::Steer,
      Capability::unavailable("native steer also starts idle agents; use start_or_steer"),
    );
    let inner = Arc::new(Inner {
      descriptor: RuntimeDescriptor {
        runtime_id,
        instance_id: uuid::Uuid::new_v4().to_string(),
        provider: Provider::Dsh,
        ownership,
        capabilities,
      },
      transport,
      state: Mutex::new(StreamState::default()),
      events,
      sequence: AtomicU64::new(0),
      publication: StdMutex::new(()),
      disconnected: AtomicBool::new(false),
      closed: AtomicBool::new(false),
      shutdown,
      tasks: StdMutex::new(Vec::new()),
      managed: StdMutex::new(None),
    });
    let tasks = vec![
      events::spawn(
        inner.clone(),
        mux,
        max_frame_bytes,
        heartbeat_interval,
        heartbeat_timeout,
      ),
      events::spawn(
        inner.clone(),
        host,
        max_frame_bytes,
        heartbeat_interval,
        heartbeat_timeout,
      ),
    ];
    inner.tasks.lock().expect("DSH task lock").extend(tasks);
    Ok(Self { inner })
  }

  /// Session-independent native directory, including advisory failures and
  /// unknown provider metadata. Does not invoke activating session.models.
  pub async fn model_catalog(&self) -> Result<api::ModelCatalog, InteractionError> {
    self.inner.ensure_open()?;
    self
      .inner
      .transport
      .call(&uuid::Uuid::new_v4().to_string(), "llm.models", json!({}), false)
      .await
  }

  async fn execute_inner(&self, request: InteractionRequest) -> Result<Reply, InteractionError> {
    let inner = &self.inner;
    inner.ensure_open()?;
    if request.target.runtime_id != inner.descriptor.runtime_id
      || request.target.instance_id != inner.descriptor.instance_id
    {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "DSH attachment identity changed",
      ));
    }
    if request.request_id.is_empty()
      || request.request_id.len() > 512
      || request.request_id.chars().any(char::is_control)
    {
      return Err(InteractionError::invalid("invalid DSH request_id"));
    }
    let rpc_id = &request.request_id;
    let session_id = request.target.session_id.as_deref();
    match request.command {
      Command::Metadata => {
        let host: api::HostDescription = inner.transport.call(rpc_id, "host.describe", json!({}), false).await?;
        Ok(Reply::Metadata(Metadata {
          name: Some("DeepSeek Harness ApiProxy".into()),
          version: Some(host.version),
          model: None,
          commands: Vec::new(),
          agents: Vec::new(),
          providers: Vec::new(),
        }))
      }
      Command::State => {
        let session_id = require_session(session_id)?;
        inner.ensure_live()?;
        let list: api::SessionList = inner.transport.call(rpc_id, "session.list", json!({}), false).await?;
        inner.ensure_live()?;
        let found = list
          .items
          .into_iter()
          .find(|entry| entry.session_id == session_id)
          .ok_or_else(|| InteractionError::unavailable("DSH session is not present on this endpoint"))?;
        let cached = if inner.disconnected.load(Ordering::Acquire) {
          None
        } else {
          inner.state.lock().await.sessions.get(session_id).cloned()
        };
        let state = SessionState {
          session_id: Some(session_id.into()),
          status: if found.running {
            ExecutionState::Running
          } else {
            ExecutionState::Idle
          },
          model: cached.as_ref().and_then(|state| state.model.clone()),
          pending_inputs: cached.as_ref().and_then(|state| state.pending_inputs),
          revision: None,
        };
        inner.ensure_live()?;
        Ok(Reply::State(state))
      }
      Command::Models => {
        let catalog: api::ModelCatalog = inner.transport.call(rpc_id, "llm.models", json!({}), false).await?;
        inner.emit(
          None,
          RuntimeEventKind::Native(json!({"type":"model_catalog","catalog":catalog})),
        );
        let choices = catalog
          .groups
          .into_iter()
          .flat_map(|group| {
            group.models.into_iter().map(move |model| ModelChoice {
              model: ModelIdentity {
                model_provider: Some(group.id.clone()),
                model_id: model.id,
                display_name: Some(model.name),
              },
              supports_images: false,
            })
          })
          .collect();
        Ok(Reply::Models(choices))
      }
      Command::Message { input, delivery } => {
        let session_id = require_session(session_id)?;
        inner.ensure_live()?;
        if input.model.is_some() || input.expected_revision.is_some() {
          return Err(InteractionError::unsupported(
            "DSH ApiProxy cannot atomically apply model intent on Send",
          ));
        }
        // Native steer wakes idle agents too; neither strict mode is fenced
        // against other endpoint clients. StartOrSteer is the honest mapping.
        let mode = match delivery {
          DeliveryMode::StartOrSteer => "steer",
          DeliveryMode::Queue => "queue",
          DeliveryMode::Start | DeliveryMode::Steer => {
            return Err(InteractionError::unsupported(
              "DSH supports start-or-steer and queue, without strict idle/active admission",
            ));
          }
        };
        let mut content = Vec::new();
        if !input.text.trim().is_empty() {
          content.push(json!({"type":"text","text":input.text}));
        }
        if input.images.len() > 16 {
          return Err(InteractionError::invalid("too many DSH image inputs"));
        }
        for image in input.images {
          if !matches!(
            image.mime_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
          ) || image.data.is_empty()
          {
            return Err(InteractionError::invalid("unsupported or empty DSH image"));
          }
          content.push(json!({"type":"image","mediaType":image.mime_type,"data":image.data}));
        }
        if content.is_empty() {
          return Err(InteractionError::invalid("DSH message must contain text or images"));
        }
        let result: Value = inner
          .transport
          .call(
            rpc_id,
            "session.prompt",
            json!({"sessionId":session_id,"mode":mode,"content":content}),
            true,
          )
          .await?;
        require_accepted(&result)?;
        Ok(Reply::Admission(InputReceipt {
          disposition: InputDisposition::Accepted,
          model: None,
          model_ignored: false,
          native_id: Some(rpc_id.clone()),
        }))
      }
      Command::CancelTurn => {
        let session_id = require_session(session_id)?;
        inner.ensure_live()?;
        let result: Value = inner
          .transport
          .call(rpc_id, "session.cancel", json!({"sessionId":session_id}), true)
          .await?;
        require_accepted(&result)?;
        Ok(Reply::Acknowledged)
      }
      Command::PendingInteractions => {
        inner.ensure_live()?;
        let pending = inner
          .state
          .lock()
          .await
          .pending
          .values()
          .filter(|pending| session_id.is_none() || pending.session_id.as_deref() == session_id)
          .cloned()
          .collect();
        Ok(Reply::Interactions(pending))
      }
      Command::Respond {
        interaction_id,
        response,
      } => inner.respond(session_id, &interaction_id, response).await,
    }
  }
}

impl Inner {
  fn ensure_open(&self) -> Result<(), InteractionError> {
    if self.closed.load(Ordering::Acquire) {
      Err(InteractionError::unavailable("DSH attachment is closed"))
    } else {
      Ok(())
    }
  }

  fn ensure_live(&self) -> Result<(), InteractionError> {
    self.ensure_open()?;
    if self.disconnected.load(Ordering::Acquire) {
      Err(InteractionError::unavailable(
        "DSH event connection was lost; attach again and refresh the baseline",
      ))
    } else {
      Ok(())
    }
  }

  fn emit(&self, session_id: Option<String>, kind: RuntimeEventKind) {
    // Sequence allocation and publication form one ordered operation across
    // both native downlinks and asynchronous response acknowledgements.
    let _publication = self.publication.lock().expect("DSH publication lock");
    if self.disconnected.load(Ordering::Acquire) && !matches!(&kind, RuntimeEventKind::Disconnected { .. }) {
      return;
    }
    let _ = self.events.send(RuntimeEvent {
      runtime_id: self.descriptor.runtime_id.clone(),
      instance_id: self.descriptor.instance_id.clone(),
      session_id,
      sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
      kind,
    });
  }
}

impl InteractionAdapter for DshAdapter {
  fn descriptor(&self) -> RuntimeDescriptor {
    self.inner.descriptor.clone()
  }
  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Result<Reply, InteractionError>> {
    Box::pin(self.execute_inner(request))
  }
  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.inner.events.subscribe()
  }
  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>> {
    Box::pin(async move {
      self.inner.closed.store(true, Ordering::Release);
      let _ = self.inner.shutdown.send(true);
      let managed = self.inner.managed.lock().expect("DSH managed lock").take();
      let result = match managed {
        Some(managed) => managed.close().await,
        None => Ok(()),
      };
      let tasks = std::mem::take(&mut *self.inner.tasks.lock().expect("DSH task lock"));
      for task in tasks {
        task.abort();
        let _ = task.await;
      }
      result
    })
  }
}

impl Drop for DshAdapter {
  fn drop(&mut self) {
    self.inner.closed.store(true, Ordering::Release);
    let _ = self.inner.shutdown.send(true);
    self.inner.managed.lock().expect("DSH managed lock").take();
    for task in self.inner.tasks.lock().expect("DSH task lock").drain(..) {
      task.abort();
    }
  }
}

fn require_session(session_id: Option<&str>) -> Result<&str, InteractionError> {
  session_id
    .filter(|id| !id.trim().is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
    .ok_or_else(|| InteractionError::invalid("a valid DSH session_id is required"))
}

fn require_accepted(value: &Value) -> Result<(), InteractionError> {
  if value.get("accepted") == Some(&Value::Bool(true)) {
    Ok(())
  } else {
    Err(InteractionError::uncertain(
      "DSH admission response is missing accepted:true",
    ))
  }
}

fn error(mutation: bool, message: impl Into<String>) -> InteractionError {
  if mutation {
    InteractionError::uncertain(message)
  } else {
    InteractionError::unavailable(message)
  }
}

#[cfg(test)]
mod tests;
