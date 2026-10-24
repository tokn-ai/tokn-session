//! Codex app-server interaction through its public bidirectional RPC boundary.
mod framing;
mod mapping;
mod state;
mod transport;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
  collections::{BTreeMap, BTreeSet},
  ffi::OsString,
  path::PathBuf,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};
use tokio::{
  io::{AsyncRead, AsyncWrite},
  sync::{broadcast, mpsc},
  task::JoinHandle,
};
use tokn_codex_rpc_protocol as wire;
use tokn_interaction_core::*;
use transport::{Incoming, Transport};

const MAX_SESSIONS: usize = 1024;
const MAX_INTERACTIONS: usize = 128;

#[derive(Clone, Debug)]
pub struct EndpointOptions {
  pub runtime_id: String,
  pub instance_id: String,
  pub request_timeout: Duration,
}
impl EndpointOptions {
  pub fn new(runtime_id: impl Into<String>) -> Self {
    Self {
      runtime_id: runtime_id.into(),
      instance_id: uuid::Uuid::new_v4().to_string(),
      request_timeout: Duration::from_secs(20),
    }
  }
}

/// Native launch settings and credentials remain host-owned.
#[derive(Clone, Debug)]
pub struct LaunchOptions {
  pub executable: PathBuf,
  pub working_directory: PathBuf,
  pub arguments: Vec<OsString>,
  pub environment: BTreeMap<OsString, OsString>,
}

/// Starting/resuming a thread is explicit; inspection never activates history.
#[derive(Clone, Debug, Default)]
pub struct ThreadOptions {
  pub cwd: Option<PathBuf>,
  pub model: Option<String>,
  pub model_provider: Option<String>,
  pub approval_policy: Option<String>,
  pub sandbox: Option<String>,
  pub config: BTreeMap<String, Value>,
  pub ephemeral: Option<bool>,
}

/// Native creation facts for host-side history verification. A returned path
/// may not exist until Codex persists the first user input.
#[derive(Clone, Debug)]
pub struct CreatedThread {
  pub target: SessionTarget,
  pub working_directory: PathBuf,
  pub history_path: Option<PathBuf>,
  pub ephemeral: bool,
}

#[derive(Clone)]
pub struct CodexAdapter {
  inner: Arc<Inner>,
}

struct Inner {
  descriptor: RuntimeDescriptor,
  transport: Arc<Transport>,
  events: broadcast::Sender<RuntimeEvent>,
  snapshot: Mutex<Snapshot>,
  sequence: Mutex<u64>,
  pump: Mutex<Option<JoinHandle<()>>>,
  disconnected: AtomicBool,
  initialize: Mutex<Value>,
}
struct Snapshot {
  epoch: u64,
  bound: BTreeSet<String>,
  sessions: BTreeMap<String, SessionRecord>,
  interactions: BTreeMap<String, PendingRecord>,
  receipts: ReceiptLedger<Result<Reply, InteractionError>>,
}
impl Default for Snapshot {
  fn default() -> Self {
    Self {
      epoch: 0,
      bound: BTreeSet::new(),
      sessions: BTreeMap::new(),
      interactions: BTreeMap::new(),
      receipts: ReceiptLedger::new(1024),
    }
  }
}
#[derive(Clone)]
struct SessionRecord {
  generation: u64,
  state: SessionState,
  name: Option<String>,
  turn_id: Option<String>,
}
#[derive(Clone)]
struct PendingRecord {
  responding: bool,
  id: wire::RpcId,
  method: String,
  params: Value,
  interaction: PendingInteraction,
}

impl CodexAdapter {
  /// Accept pipes intentionally supplied by their owner; never steal process IO.
  pub async fn attach<R, W>(options: EndpointOptions, reader: R, writer: W) -> Result<Self, InteractionError>
  where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
  {
    validate_options(&options)?;
    let (transport, incoming) = Transport::stdio(reader, writer, None, options.request_timeout);
    Self::initialize(options, Ownership::Attached, transport, incoming).await
  }

  /// Attach to an explicitly configured WebSocket app-server, with optional bearer auth.
  pub async fn connect(
    options: EndpointOptions,
    endpoint: &str,
    bearer: Option<&str>,
  ) -> Result<Self, InteractionError> {
    validate_options(&options)?;
    let (transport, incoming) = Transport::websocket(endpoint, bearer, options.request_timeout).await?;
    Self::initialize(options, Ownership::Attached, transport, incoming).await
  }

  pub async fn spawn(options: EndpointOptions, launch: LaunchOptions) -> Result<Self, InteractionError> {
    validate_options(&options)?;
    if launch.executable.as_os_str().is_empty() || !launch.working_directory.is_absolute() {
      return Err(InteractionError::invalid(
        "Codex needs an executable and an absolute working directory",
      ));
    }
    let mut command = tokio::process::Command::new(launch.executable);
    command
      .args(launch.arguments)
      .args(["app-server", "--listen", "stdio://"])
      .current_dir(launch.working_directory)
      .envs(launch.environment)
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped())
      .kill_on_drop(true);
    let mut child = command
      .spawn()
      .map_err(|error| InteractionError::unavailable(format!("Could not start Codex app-server: {error}")))?;
    let reader = child
      .stdout
      .take()
      .ok_or_else(|| InteractionError::unavailable("Codex stdout is unavailable"))?;
    let writer = child
      .stdin
      .take()
      .ok_or_else(|| InteractionError::unavailable("Codex stdin is unavailable"))?;
    let (transport, incoming) = Transport::stdio(reader, writer, Some(child), options.request_timeout);
    Self::initialize(options, Ownership::Managed, transport, incoming).await
  }

  async fn initialize(
    options: EndpointOptions,
    ownership: Ownership,
    transport: Arc<Transport>,
    mut incoming: mpsc::Receiver<Incoming>,
  ) -> Result<Self, InteractionError> {
    let inner = Arc::new(Inner {
      descriptor: RuntimeDescriptor {
        runtime_id: options.runtime_id,
        instance_id: options.instance_id,
        provider: Provider::Codex,
        ownership,
        capabilities: capabilities(),
      },
      transport,
      events: broadcast::channel(256).0,
      snapshot: Mutex::new(Snapshot::default()),
      sequence: Mutex::new(0),
      pump: Mutex::new(None),
      disconnected: AtomicBool::new(false),
      initialize: Mutex::new(Value::Null),
    });
    let weak = Arc::downgrade(&inner);
    let pump = tokio::spawn(async move {
      while let Some(message) = incoming.recv().await {
        let Some(inner) = weak.upgrade() else {
          return;
        };
        inner.receive(message).await;
      }
      if let Some(inner) = weak.upgrade() {
        inner.disconnect("Codex RPC event stream closed".into());
      }
    });
    *inner.pump.lock().unwrap_or_else(|error| error.into_inner()) = Some(pump);
    let adapter = Self { inner };
    let result = adapter
      .inner
      .transport
      .rpc(
        "initialize",
        json!({
          "clientInfo":{"name":"tokn_session","title":"Tokn Session","version":env!("CARGO_PKG_VERSION")},
          "capabilities":{"experimentalApi":true}
        }),
        false,
      )
      .await;
    match result {
      Ok(value) if serde_json::from_value::<wire::InitializeResponse>(value.clone()).is_ok() => {
        *adapter
          .inner
          .initialize
          .lock()
          .unwrap_or_else(|error| error.into_inner()) = value;
        if let Err(error) = adapter.inner.transport.notify("initialized", json!({})).await {
          let _ = adapter.inner.transport.close().await;
          return Err(error);
        }
        Ok(adapter)
      }
      result => {
        let _ = adapter.inner.transport.close().await;
        Err(
          result
            .err()
            .unwrap_or_else(|| InteractionError::unavailable("Malformed Codex initialization response")),
        )
      }
    }
  }

  pub fn stderr_tail(&self) -> String {
    self.inner.transport.stderr_tail()
  }

  pub async fn create_thread(&self, options: ThreadOptions) -> Result<SessionTarget, InteractionError> {
    let result = self.start_thread(options).await?;
    self.bind_thread(result, None)
  }

  /// Create once and retain the native history location. This does not send a
  /// prompt or claim that the returned history path is already durable.
  pub async fn create_thread_with_descriptor(&self, options: ThreadOptions) -> Result<CreatedThread, InteractionError> {
    let expected_cwd = options.cwd.clone();
    let expected_ephemeral = options.ephemeral;
    let result = self.start_thread(options).await?;
    let target = self.bind_thread(result.clone(), None)?;
    let parsed: wire::ThreadStartResponse = serde_json::from_value(result.clone()).map_err(|_| {
      InteractionError::uncertain("Codex may have created a thread, but returned incompatible creation metadata")
    })?;
    let cwd = parsed
      .thread
      .cwd
      .as_deref()
      .or_else(|| result.get("cwd").and_then(Value::as_str))
      .map(PathBuf::from)
      .filter(|path| path.is_absolute())
      .ok_or_else(|| {
        InteractionError::uncertain("Codex may have created a thread, but returned no absolute working directory")
      })?;
    if expected_cwd.as_ref().is_some_and(|expected| expected != &cwd)
      || result
        .get("cwd")
        .and_then(Value::as_str)
        .is_some_and(|value| PathBuf::from(value) != cwd)
    {
      return Err(InteractionError::uncertain(
        "Codex may have created a thread in an unexpected working directory",
      ));
    }
    let ephemeral = result["thread"]["ephemeral"].as_bool().ok_or_else(|| {
      InteractionError::uncertain("Codex may have created a thread, but returned no persistence identity")
    })?;
    if expected_ephemeral.is_some_and(|expected| expected != ephemeral) {
      return Err(InteractionError::uncertain(
        "Codex may have created a thread with unexpected persistence settings",
      ));
    }
    let history_path = parsed.thread.path.map(PathBuf::from);
    if history_path.as_ref().is_some_and(|path| !path.is_absolute()) || !ephemeral && history_path.is_none() {
      return Err(InteractionError::uncertain(
        "Codex may have created a thread, but returned no absolute history location",
      ));
    }
    Ok(CreatedThread {
      target,
      working_directory: cwd,
      history_path,
      ephemeral,
    })
  }

  async fn start_thread(&self, options: ThreadOptions) -> Result<Value, InteractionError> {
    self.inner.ensure_open()?;
    if options.cwd.as_ref().is_some_and(|path| !path.is_absolute()) {
      return Err(InteractionError::invalid("Codex thread cwd must be absolute"));
    }
    let mut params = json!({});
    for (key, value) in [
      ("cwd", options.cwd.map(|path| json!(path))),
      ("model", options.model.map(Value::String)),
      ("modelProvider", options.model_provider.map(Value::String)),
      ("approvalPolicy", options.approval_policy.map(Value::String)),
      ("sandbox", options.sandbox.map(Value::String)),
      ("ephemeral", options.ephemeral.map(Value::Bool)),
    ] {
      if let Some(value) = value {
        params[key] = value;
      }
    }
    if !options.config.is_empty() {
      params["config"] = json!(options.config);
    }
    let epoch = self.inner.epoch();
    self
      .inner
      .transport
      .rpc_with_context("thread/start", params, true, Some(epoch))
      .await
  }

  pub async fn resume_thread(&self, session_id: &str) -> Result<SessionTarget, InteractionError> {
    self.inner.ensure_open()?;
    if !mapping::valid_id(session_id) {
      return Err(InteractionError::invalid("Invalid Codex thread ID"));
    }
    let epoch = self.inner.epoch();
    let result = self
      .inner
      .transport
      .rpc_with_context(
        "thread/resume",
        json!({"threadId":session_id,"excludeTurns":true}),
        true,
        Some(epoch),
      )
      .await?;
    self.bind_thread(result, Some(session_id))
  }

  fn bind_thread(&self, result: Value, expected: Option<&str>) -> Result<SessionTarget, InteractionError> {
    let thread = result
      .get("thread")
      .ok_or_else(|| InteractionError::uncertain("Codex may have loaded a thread, but returned no identity"))?;
    let id = thread
      .get("id")
      .and_then(Value::as_str)
      .filter(|id| mapping::valid_id(id))
      .map(str::to_owned)
      .ok_or_else(|| InteractionError::uncertain("Codex returned an invalid thread identity"))?;
    if expected.is_some_and(|expected| expected != id) {
      return Err(InteractionError::uncertain("Codex resumed an unexpected thread"));
    }
    // Thread responses are applied by the ordered event pump before waking
    // their caller; applying the returned snapshot here could overwrite a
    // newer notification that has already been processed.
    Ok(SessionTarget {
      runtime_id: self.inner.descriptor.runtime_id.clone(),
      instance_id: self.inner.descriptor.instance_id.clone(),
      session_id: Some(id),
    })
  }

  async fn read_thread(&self, id: &str, include_turns: bool) -> Result<SessionRecord, InteractionError> {
    let epoch = self.inner.epoch();
    let result = self
      .inner
      .transport
      .rpc_with_context(
        "thread/read",
        json!({"threadId":id,"includeTurns":include_turns}),
        false,
        Some(epoch),
      )
      .await?;
    let parsed: wire::ThreadReadResponse = serde_json::from_value(result.clone())
      .map_err(|_| InteractionError::unavailable("Codex returned an incompatible thread response"))?;
    if parsed.thread.id != id {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "Codex returned a different thread",
      ));
    }
    self
      .inner
      .snapshot
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .sessions
      .get(id)
      .cloned()
      .ok_or_else(|| InteractionError::unavailable("Codex thread state is unavailable"))
  }

  async fn perform(&self, request: &InteractionRequest) -> Result<Reply, InteractionError> {
    self.inner.check_target(&request.target)?;
    let session = request.target.session_id.as_deref();
    if request.command.is_mutation() {
      let id = session.ok_or_else(|| InteractionError::invalid("Codex mutations require an explicit thread ID"))?;
      if !self
        .inner
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .bound
        .contains(id)
      {
        return Err(InteractionError::new(
          ErrorKind::StaleTarget,
          "Explicitly create or resume this thread before controlling it; inspection does not subscribe to live requests",
        ));
      }
    }
    match &request.command {
      Command::Models => {
        let mut result = Vec::new();
        let mut cursor: Option<String> = None;
        let mut cursors = std::collections::HashSet::new();
        for _ in 0..32 {
          let value = self
            .inner
            .transport
            .rpc("model/list", json!({"cursor":cursor,"limit":100}), false)
            .await?;
          let page: wire::ModelListResponse = serde_json::from_value(value)
            .map_err(|_| InteractionError::unavailable("Codex returned an incompatible model catalog"))?;
          if result.len() + page.data.len() > 2048 {
            return Err(InteractionError::unavailable("Codex model catalog exceeds its bound"));
          }
          for model in page.data.into_iter().filter(|model| !model.hidden) {
            result.push(ModelChoice {
              supports_images: model
                .input_modalities
                .as_ref()
                .is_some_and(|input| input.iter().any(|kind| kind == "image")),
              model: ModelIdentity {
                model_id: model.model,
                model_provider: None,
                display_name: Some(model.display_name),
              },
            });
          }
          cursor = page.next_cursor;
          if cursor.is_none() {
            return Ok(Reply::Models(result));
          }
          if !cursors.insert(cursor.clone()) {
            break;
          }
        }
        Err(InteractionError::unavailable(
          "Codex model pagination did not terminate",
        ))
      }
      Command::Metadata => {
        let record = if let Some(id) = session {
          Some(self.read_thread(id, false).await?)
        } else {
          None
        };
        Ok(Reply::Metadata(Metadata {
          name: record
            .as_ref()
            .and_then(|record| record.name.clone())
            .or_else(|| Some("Codex app-server".into())),
          version: self
            .inner
            .initialize
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get("userAgent")
            .and_then(Value::as_str)
            .map(str::to_owned),
          model: record.and_then(|record| record.state.model),
          ..Metadata::default()
        }))
      }
      Command::State => {
        let id = session.ok_or_else(|| InteractionError::invalid("Codex state requires a native thread ID"))?;
        Ok(Reply::State(self.read_thread(id, false).await?.state))
      }
      Command::PendingInteractions => {
        let snapshot = self.inner.snapshot.lock().unwrap_or_else(|error| error.into_inner());
        Ok(Reply::Interactions(
          snapshot
            .interactions
            .values()
            .filter(|record| {
              !record.responding && (session.is_none() || record.interaction.session_id.as_deref() == session)
            })
            .map(|record| record.interaction.clone())
            .collect(),
        ))
      }
      Command::Message { input, delivery } => {
        let id = session.ok_or_else(|| InteractionError::invalid("Codex messages require an explicit thread ID"))?;
        if input.model.is_some() {
          return Err(InteractionError::unsupported(
            "Codex turn/start applies model overrides while steering; atomic model-on-send is unavailable",
          ));
        }
        if matches!(delivery, DeliveryMode::Start | DeliveryMode::Queue) {
          return Err(InteractionError::unsupported(
            "Codex supports start_or_steer and strict steer; strict start and automatic follow-up queue are unavailable",
          ));
        }
        if input.expected_revision.is_some() && *delivery != DeliveryMode::Steer {
          return Err(InteractionError::unsupported(
            "Codex revision preconditions require strict steering",
          ));
        }
        let input_items = mapping::input(input)?;
        let (method, params) = if *delivery == DeliveryMode::Steer {
          let turn = self.active_turn(id).await?;
          if input
            .expected_revision
            .as_ref()
            .is_some_and(|expected| expected != &turn)
          {
            return Err(InteractionError::new(ErrorKind::Conflict, "Codex active turn changed"));
          }
          (
            "turn/steer",
            json!({"threadId":id,"expectedTurnId":turn,"input":input_items}),
          )
        } else {
          ("turn/start", json!({"threadId":id,"input":input_items}))
        };
        let value = self.inner.transport.rpc(method, params, true).await?;
        let turn_id = if *delivery == DeliveryMode::Steer {
          value.get("turnId")
        } else {
          value.get("turn").and_then(|turn| turn.get("id"))
        }
        .and_then(Value::as_str)
        .filter(|id| mapping::valid_id(id))
        .map(str::to_owned)
        .ok_or_else(|| InteractionError::uncertain("Codex may have accepted input, but returned no valid turn ID"))?;
        Ok(Reply::Admission(InputReceipt {
          disposition: if *delivery == DeliveryMode::Steer {
            InputDisposition::Steered
          } else {
            InputDisposition::Accepted
          },
          model: None,
          model_ignored: false,
          native_id: Some(turn_id),
        }))
      }
      Command::CancelTurn => {
        let id =
          session.ok_or_else(|| InteractionError::invalid("Codex cancellation requires an explicit thread ID"))?;
        let turn = self.active_turn(id).await?;
        self
          .inner
          .transport
          .rpc("turn/interrupt", json!({"threadId":id,"turnId":turn}), true)
          .await?;
        Ok(Reply::Acknowledged)
      }
      Command::Respond {
        interaction_id,
        response,
      } => {
        let id = session.ok_or_else(|| InteractionError::invalid("Codex replies require an explicit thread ID"))?;
        let (rpc_id, value) = {
          let mut snapshot = self.inner.snapshot.lock().unwrap_or_else(|error| error.into_inner());
          let pending = snapshot
            .interactions
            .get_mut(interaction_id)
            .filter(|pending| !pending.responding && pending.interaction.session_id.as_deref() == Some(id))
            .ok_or_else(|| {
              InteractionError::new(
                ErrorKind::StaleTarget,
                "This Codex request is no longer pending on this thread",
              )
            })?;
          let value = mapping::response(&pending.interaction, &pending.method, &pending.params, response)?;
          let rpc_id = pending.id.clone();
          pending.responding = true;
          (rpc_id, value)
        };
        // Native server-request replies have no acknowledgement: success means
        // the complete response was written, never that a later turn completed.
        let result = self.inner.transport.respond(&rpc_id, value).await;
        let retire = result.is_ok()
          || self.inner.transport.is_closed()
          || result
            .as_ref()
            .err()
            .is_some_and(|error| error.kind == ErrorKind::Uncertain);
        let removed = {
          let mut snapshot = self.inner.snapshot.lock().unwrap_or_else(|error| error.into_inner());
          if retire {
            snapshot.interactions.remove(interaction_id).is_some()
          } else {
            if let Some(pending) = snapshot.interactions.get_mut(interaction_id) {
              pending.responding = false;
            }
            false
          }
        };
        if removed {
          self.inner.publish(
            Some(id.to_owned()),
            RuntimeEventKind::InteractionResolved {
              interaction_id: interaction_id.clone(),
            },
          );
        }
        result?;
        Ok(Reply::Acknowledged)
      }
    }
  }

  async fn active_turn(&self, id: &str) -> Result<String, InteractionError> {
    let active = || {
      self
        .inner
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .sessions
        .get(id)
        .filter(|record| record.state.status == ExecutionState::Running)
        .and_then(|record| record.turn_id.clone())
    };
    if let Some(turn) = active() {
      return Ok(turn);
    }
    // Late attachments inspect one turn's metadata, never a full history.
    self.read_thread(id, false).await?;
    if let Some(turn) = active() {
      return Ok(turn);
    }
    let epoch = self.inner.epoch();
    self
      .inner
      .transport
      .rpc_with_context(
        "thread/turns/list",
        json!({"threadId":id,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
        false,
        Some(epoch),
      )
      .await?;
    active().ok_or_else(|| {
      InteractionError::new(
        ErrorKind::Conflict,
        "Codex has no known active turn to steer or interrupt",
      )
    })
  }

  async fn execute_request(&self, request: InteractionRequest) -> Result<Reply, InteractionError> {
    self.inner.check_identity(&request.target)?;
    if !mapping::valid_id(&request.request_id) {
      return Err(InteractionError::invalid("Invalid Codex request ID"));
    }
    if !request.command.is_mutation() {
      return self.perform(&request).await;
    }
    let fingerprint: [u8; 32] =
      Sha256::digest(serde_json::to_vec(&request).map_err(|_| InteractionError::invalid("Invalid Codex request"))?)
        .into();
    {
      let mut snapshot = self.inner.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      let admission = snapshot
        .receipts
        .admit(
          request.request_id.clone(),
          self.inner.descriptor.instance_id.clone(),
          fingerprint,
          Ok(Reply::Pending),
        )
        .map_err(|error| {
          InteractionError::new(
            ErrorKind::Conflict,
            format!("Codex admission receipt unavailable: {error:?}"),
          )
        })?;
      if let ReceiptAdmission::Replay(result) = admission {
        return result;
      }
    }
    let adapter = self.clone();
    tokio::spawn(async move {
      let result = adapter.perform(&request).await;
      let mut snapshot = adapter.inner.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      if result
        .as_ref()
        .err()
        .is_some_and(|error| error.kind == ErrorKind::Uncertain)
      {
        snapshot.receipts.retain_outcome(&request.request_id, result.clone());
      } else {
        snapshot.receipts.complete(&request.request_id, result.clone());
      }
      result
    })
    .await
    .map_err(|_| InteractionError::uncertain("Codex admission task was interrupted"))?
  }
}

fn params_session(native: &Value) -> Option<String> {
  native.get("params")?.get("threadId")?.as_str().map(str::to_owned)
}
fn validate_options(options: &EndpointOptions) -> Result<(), InteractionError> {
  if !mapping::valid_id(&options.runtime_id)
    || !mapping::valid_id(&options.instance_id)
    || options.request_timeout.is_zero()
    || options.request_timeout > Duration::from_secs(300)
  {
    Err(InteractionError::invalid(
      "Codex needs valid runtime identities and a bounded request timeout",
    ))
  } else {
    Ok(())
  }
}
fn capabilities() -> Capabilities {
  let mut result = Capabilities::default()
    .with(Operation::Metadata)
    .with(Operation::State)
    .with(Operation::Models)
    .with(Operation::Message)
    .with(Operation::Steer)
    .with(Operation::CancelTurn)
    .with(Operation::Respond)
    .with(Operation::PendingInteractions);
  result.operations.insert(
    Operation::Queue,
    Capability::unavailable("Codex does not expose automatic native follow-up delivery through turn/start"),
  );
  result
}
impl InteractionAdapter for CodexAdapter {
  fn descriptor(&self) -> RuntimeDescriptor {
    self.inner.descriptor.clone()
  }
  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Result<Reply, InteractionError>> {
    Box::pin(self.execute_request(request))
  }
  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.inner.events.subscribe()
  }
  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>> {
    Box::pin(async { self.inner.transport.close().await })
  }
}
impl Drop for Inner {
  fn drop(&mut self) {
    if let Some(pump) = self.pump.get_mut().unwrap_or_else(|error| error.into_inner()).take() {
      pump.abort();
    }
  }
}

#[cfg(test)]
mod tests;
