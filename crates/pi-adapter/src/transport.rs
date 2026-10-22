use super::*;
use std::sync::{
  Weak,
  atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  sync::oneshot,
  task::JoinHandle,
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PENDING: usize = 128;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

struct Pending {
  command: &'static str,
  mutation: bool,
  reply: oneshot::Sender<Result<Value, InteractionError>>,
}

pub(super) struct Inner {
  pub(super) descriptor: RuntimeDescriptor,
  pub(super) events: broadcast::Sender<RuntimeEvent>,
  pub(super) snapshot: Mutex<Snapshot>,
  pub(super) admission: AsyncMutex<()>,
  pub(super) stderr: Mutex<framing::StderrTail>,
  options: EndpointOptions,
  writer: AsyncMutex<Option<Writer>>,
  pending: Mutex<HashMap<String, Pending>>,
  child: AsyncMutex<Option<Child>>,
  tasks: Mutex<Vec<JoinHandle<()>>>,
  closed: AtomicBool,
  sequence: Mutex<u64>,
  request_sequence: AtomicU64,
}

impl Inner {
  pub(super) fn new<W>(options: EndpointOptions, ownership: Ownership, writer: W, child: Option<Child>) -> Arc<Self>
  where
    W: AsyncWrite + Unpin + Send + 'static,
  {
    let (events, _) = broadcast::channel(256);
    let inner = Arc::new(Self {
      descriptor: RuntimeDescriptor {
        runtime_id: options.runtime_id.clone(),
        instance_id: options.instance_id.clone(),
        provider: Provider::Pi,
        ownership,
        capabilities: capabilities(),
      },
      events,
      snapshot: Mutex::new(Snapshot::default()),
      admission: AsyncMutex::new(()),
      stderr: Mutex::new(framing::StderrTail::default()),
      options,
      writer: AsyncMutex::new(Some(Box::new(writer))),
      pending: Mutex::new(HashMap::new()),
      child: AsyncMutex::new(child),
      tasks: Mutex::new(Vec::new()),
      closed: AtomicBool::new(false),
      sequence: Mutex::new(0),
      request_sequence: AtomicU64::new(0),
    });
    if let Some(mut stderr) = inner
      .child
      .try_lock()
      .ok()
      .and_then(|mut child| child.as_mut().and_then(|child| child.stderr.take()))
    {
      let weak = Arc::downgrade(&inner);
      let task = tokio::spawn(async move {
        let mut bytes = [0; 4096];
        loop {
          let Ok(length) = stderr.read(&mut bytes).await else {
            break;
          };
          if length == 0 {
            break;
          }
          let Some(inner) = weak.upgrade() else { break };
          inner
            .stderr
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(&bytes[..length]);
        }
      });
      inner.tasks.lock().unwrap_or_else(|error| error.into_inner()).push(task);
    }
    inner
  }

  pub(super) fn check_target(&self, target: &SessionTarget) -> Result<(), InteractionError> {
    self.check_identity(target)?;
    self.check_available()
  }

  pub(super) fn check_identity(&self, target: &SessionTarget) -> Result<(), InteractionError> {
    if target.runtime_id != self.descriptor.runtime_id || target.instance_id != self.descriptor.instance_id {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "The Pi runtime instance changed",
      ));
    }
    Ok(())
  }

  pub(super) fn check_available(&self) -> Result<(), InteractionError> {
    if self.closed.load(Ordering::Acquire) {
      return Err(InteractionError::unavailable("The Pi RPC connection is closed"));
    }
    Ok(())
  }

  pub(super) fn check_session(&self, target: &SessionTarget) -> Result<(), InteractionError> {
    self.check_target(target)?;
    let snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
    let id = snapshot.native_state.as_ref().map(|state| state.session_id.as_str());
    if [
      self.options.expected_session_id.as_deref(),
      target.session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|expected| Some(expected) != id)
    {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "Pi is bound to a different native session",
      ));
    }
    Ok(())
  }

  pub(super) async fn fresh_state(&self, target: &SessionTarget) -> Result<wire::State, InteractionError> {
    self.check_target(target)?;
    let value = self.rpc(wire::Command::GetState, false).await?;
    let state = wire::parse_state(&value).map_err(InteractionError::unavailable)?;
    // The ordered reader already accepted this snapshot before waking us.
    // Reapplying here could overwrite an agent_start received after its reply.
    self.check_session(target)?;
    Ok(state)
  }

  fn accept_state(&self, state: wire::State) {
    let resolved = {
      let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      let first_state = snapshot.native_state.is_none();
      let changed_session = snapshot
        .native_state
        .as_ref()
        .is_some_and(|previous| previous.session_id != state.session_id);
      let resolved = if changed_session {
        std::mem::take(&mut snapshot.interactions)
          .into_iter()
          .map(|(id, record)| (id, record.interaction.session_id))
          .collect::<Vec<_>>()
      } else {
        Vec::new()
      };
      if first_state {
        for record in snapshot.interactions.values_mut() {
          record.interaction.session_id = Some(state.session_id.clone());
        }
      }
      if snapshot.native_state.as_ref() != Some(&state) {
        snapshot.revision = snapshot.revision.saturating_add(1);
      }
      snapshot.execution = Some(
        if state.is_streaming || state.is_compacting || state.pending_message_count > 0 {
          ExecutionState::Running
        } else {
          ExecutionState::Idle
        },
      );
      snapshot.native_state = Some(state.clone());
      resolved
    };
    for (interaction_id, session_id) in resolved {
      self.publish_for_session(session_id, RuntimeEventKind::InteractionResolved { interaction_id });
    }
    // Query-derived state belongs to its explicit reply. Emitting another State
    // event here would race host baseline recovery and duplicate every read.
  }

  pub(super) fn session_state(&self) -> SessionState {
    let snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
    SessionState {
      session_id: snapshot.native_state.as_ref().map(|state| state.session_id.clone()),
      status: snapshot.execution.unwrap_or(ExecutionState::Unknown),
      model: snapshot
        .native_state
        .as_ref()
        .and_then(|state| state.model.as_ref())
        .and_then(mapping::model),
      pending_inputs: snapshot.native_state.as_ref().map(|state| state.pending_message_count),
      revision: Some(format!("{}:{}", self.descriptor.instance_id, snapshot.revision)),
    }
  }

  pub(super) fn publish(&self, kind: RuntimeEventKind) {
    let session_id = match &kind {
      RuntimeEventKind::State(state) => state.session_id.clone(),
      RuntimeEventKind::InteractionRequested(interaction) => interaction.session_id.clone(),
      _ => self
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .native_state
        .as_ref()
        .map(|state| state.session_id.clone()),
    };
    self.publish_for_session(session_id, kind);
  }

  pub(super) fn publish_for_session(&self, session_id: Option<String>, kind: RuntimeEventKind) {
    // Reader, expiry, and response tasks can publish concurrently. Allocate the
    // sequence and send under one short synchronous lock so subscribers cannot
    // observe sequence 2 before sequence 1. No state lock or await is held here.
    let mut sequence = self.sequence.lock().unwrap_or_else(|error| error.into_inner());
    if self.closed.load(Ordering::Acquire) && !matches!(&kind, RuntimeEventKind::Disconnected { .. }) {
      return;
    }
    *sequence = sequence.checked_add(1).expect("Pi runtime event sequence exhausted");
    let _ = self.events.send(RuntimeEvent {
      runtime_id: self.descriptor.runtime_id.clone(),
      instance_id: self.descriptor.instance_id.clone(),
      session_id,
      sequence: *sequence,
      kind,
    });
  }

  pub(super) async fn rpc(&self, command: wire::Command, mutation: bool) -> Result<Value, InteractionError> {
    if self.closed.load(Ordering::Acquire) {
      return Err(InteractionError::unavailable("Pi RPC is disconnected"));
    }
    let name = command.name();
    let id = format!(
      "{}:{}",
      self.descriptor.instance_id,
      self.request_sequence.fetch_add(1, Ordering::Relaxed)
    );
    let request = wire::Request {
      id: Some(id.clone()),
      command,
    };
    let value = serde_json::to_value(request).map_err(|error| InteractionError::invalid(error.to_string()))?;
    let (reply, receive) = oneshot::channel();
    {
      let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());
      if pending.len() >= MAX_PENDING {
        return Err(InteractionError::unavailable("Pi RPC pending request limit reached"));
      }
      pending.insert(
        id.clone(),
        Pending {
          command: name,
          mutation,
          reply,
        },
      );
    }
    if let Err(error) = self.write_value(value, mutation).await {
      self
        .pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&id);
      return Err(error);
    }
    let deadline = if name == "prompt" {
      self.options.prompt_timeout
    } else {
      self.options.request_timeout
    };
    match tokio::time::timeout(deadline, receive).await {
      Ok(Ok(result)) => result,
      _ => {
        self
          .pending
          .lock()
          .unwrap_or_else(|error| error.into_inner())
          .remove(&id);
        Err(if mutation {
          InteractionError::uncertain("Pi did not acknowledge the command; check runtime state before retrying")
        } else {
          InteractionError::unavailable("Pi metadata request timed out")
        })
      }
    }
  }

  pub(super) async fn write_value(&self, value: Value, mutation: bool) -> Result<(), InteractionError> {
    let mut bytes = serde_json::to_vec(&value).map_err(|error| InteractionError::invalid(error.to_string()))?;
    if bytes.len() > framing::MAX_FRAME_BYTES {
      return Err(InteractionError::invalid(
        "Pi RPC command exceeds the 34 MiB frame limit",
      ));
    }
    bytes.push(b'\n');
    let mut writer = self.writer.lock().await;
    if self.closed.load(Ordering::Acquire) {
      return Err(InteractionError::unavailable("Pi RPC is disconnected"));
    }
    let writer = writer
      .as_mut()
      .ok_or_else(|| InteractionError::unavailable("Pi RPC input is closed"))?;
    match tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(&bytes)).await {
      Ok(Ok(())) => Ok(()),
      _ => {
        // write_all may have emitted only a JSON prefix. No later request can
        // safely use this LF stream without appending to that damaged frame.
        self.disconnected("Pi RPC write failed; the transport is no longer usable".into());
        Err(if mutation {
          InteractionError::uncertain("Pi RPC write failed; the command may have been accepted")
        } else {
          InteractionError::unavailable("Pi RPC write failed")
        })
      }
    }
  }

  async fn receive(self: &Arc<Self>, value: Value) -> Result<(), String> {
    match wire::ServerMessage::decode(value) {
      wire::ServerMessage::Response { response, native } => {
        let id = response.id.as_deref().ok_or("Pi RPC response has no correlation ID")?;
        let pending = {
          let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());
          if pending
            .get(id)
            .is_some_and(|pending| pending.command != response.command)
          {
            return Err("Pi RPC response command does not match its request".into());
          }
          pending.remove(id)
        };
        if let Some(pending) = pending {
          if response.success && pending.command == "get_state" {
            match response
              .data
              .as_ref()
              .ok_or_else(|| "Pi returned no state data".to_owned())
              .and_then(wire::parse_state)
            {
              Ok(state) => self.accept_state(state),
              Err(reason) => {
                let _ = pending.reply.send(Err(InteractionError::unavailable(reason.clone())));
                return Err(reason);
              }
            }
          }
          let result = if response.success {
            Ok(response.data.unwrap_or(Value::Null))
          } else {
            Err(InteractionError::new(
              ErrorKind::Rejected,
              response
                .error
                .unwrap_or_else(|| "Pi rejected the command".into())
                .chars()
                .take(2048)
                .collect::<String>(),
            ))
          };
          let _ = pending.reply.send(result);
        } else {
          self.publish(RuntimeEventKind::Native(native));
        }
      }
      wire::ServerMessage::ExtensionUi {
        dialog: Some(dialog),
        native,
      } => self.receive_dialog(dialog, native).await?,
      wire::ServerMessage::Notification { kind, native } => {
        let phase = match kind.as_str() {
          "agent_start" | "auto_retry_start" | "auto_compaction_start" | "compaction_start" => {
            Some(ExecutionState::Running)
          }
          "agent_settled" => Some(ExecutionState::Idle),
          _ => None,
        };
        if let Some(phase) = phase {
          self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .execution = Some(phase);
          self.publish(RuntimeEventKind::State(self.session_state()));
        }
        // Preserve every native notification, including token/tool events that
        // the existing viewer-owned process manager intentionally filters out.
        self.publish(RuntimeEventKind::Native(native));
      }
      wire::ServerMessage::Unknown(native) if native.get("type").and_then(Value::as_str) == Some("response") => {
        return Err("Malformed Pi RPC response".into());
      }
      wire::ServerMessage::Unknown(native)
        if native.get("type").and_then(Value::as_str) == Some("extension_ui_request") =>
      {
        if let Some(id) = native
          .get("id")
          .and_then(Value::as_str)
          .filter(|id| valid_identity(id) && id.len() <= wire::MAX_ID_BYTES)
        {
          self
            .write_value(json!({"type":"extension_ui_response","id":id,"cancelled":true}), true)
            .await
            .map_err(|error| error.to_string())?;
        }
        self.publish(RuntimeEventKind::Native(native));
      }
      message => self.publish(RuntimeEventKind::Native(message.native().clone())),
    }
    Ok(())
  }

  async fn receive_dialog(self: &Arc<Self>, dialog: wire::ExtensionDialog, native: Value) -> Result<(), String> {
    let expires = Instant::now()
      + dialog
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(MAX_DIALOG_AGE)
        .min(MAX_DIALOG_AGE);
    let interaction = {
      let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      if snapshot.interactions.contains_key(&dialog.dialog_id) {
        return Ok(());
      }
      if snapshot.interactions.len() >= MAX_INTERACTIONS {
        None
      } else {
        let session_id = snapshot
          .native_state
          .as_ref()
          .map(|state| state.session_id.clone())
          .or_else(|| self.options.expected_session_id.clone());
        let interaction = mapping::interaction(&dialog, session_id, native);
        snapshot.interactions.insert(
          dialog.dialog_id.clone(),
          DialogRecord {
            dialog: dialog.clone(),
            interaction: interaction.clone(),
            expires,
          },
        );
        Some(interaction)
      }
    };
    let Some(interaction) = interaction else {
      let _ = self
        .write_value(
          json!({"type":"extension_ui_response","id":dialog.dialog_id,"cancelled":true}),
          true,
        )
        .await;
      return Ok(());
    };
    self.publish(RuntimeEventKind::InteractionRequested(interaction));
    Ok(())
  }

  async fn expire_dialogs(&self) {
    let ids = {
      let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      let ids = snapshot
        .interactions
        .iter()
        .filter(|(_, record)| record.expires <= Instant::now())
        .map(|(id, record)| (id.clone(), record.interaction.session_id.clone()))
        .collect::<Vec<_>>();
      for (id, _) in &ids {
        snapshot.interactions.remove(id);
      }
      ids
    };
    for (id, session_id) in &ids {
      self.publish_for_session(
        session_id.clone(),
        RuntimeEventKind::InteractionResolved {
          interaction_id: id.clone(),
        },
      );
    }
    for (id, _) in ids {
      let _ = self
        .write_value(json!({"type":"extension_ui_response","id":id,"cancelled":true}), true)
        .await;
    }
  }

  pub(super) async fn cancel_interactions(&self) -> Result<(), InteractionError> {
    let ids = std::mem::take(
      &mut self
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .interactions,
    )
    .into_iter()
    .map(|(id, record)| (id, record.interaction.session_id))
    .collect::<Vec<_>>();
    for (id, session_id) in &ids {
      self.publish_for_session(
        session_id.clone(),
        RuntimeEventKind::InteractionResolved {
          interaction_id: id.clone(),
        },
      );
    }
    for (id, _) in ids {
      self
        .write_value(json!({"type":"extension_ui_response","id":id,"cancelled":true}), true)
        .await?;
    }
    Ok(())
  }

  fn disconnected(&self, reason: String) {
    if self.closed.swap(true, Ordering::AcqRel) {
      return;
    }
    for (_, pending) in self.pending.lock().unwrap_or_else(|error| error.into_inner()).drain() {
      let error = if pending.mutation {
        InteractionError::uncertain(reason.clone())
      } else {
        InteractionError::unavailable(reason.clone())
      };
      let _ = pending.reply.send(Err(error));
    }
    let ids = {
      let mut snapshot = self.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      snapshot.execution = Some(ExecutionState::Stopped);
      std::mem::take(&mut snapshot.interactions)
        .into_iter()
        .map(|(id, record)| (id, record.interaction.session_id))
        .collect::<Vec<_>>()
    };
    for (interaction_id, session_id) in ids {
      self.publish_for_session(session_id, RuntimeEventKind::InteractionResolved { interaction_id });
    }
    self.publish(RuntimeEventKind::Disconnected { reason });
  }

  pub(super) async fn close(&self) -> Result<(), InteractionError> {
    // Attached handles belong to a connector client: no abort, signal, or child
    // termination is issued. The connector must keep its native process alive
    // independently of this client transport's lifetime.
    self.disconnected("Pi RPC adapter closed".into());
    if let Some(mut writer) = self.writer.lock().await.take() {
      let _ = tokio::time::timeout(WRITE_TIMEOUT, writer.shutdown()).await;
    }
    for task in self.tasks.lock().unwrap_or_else(|error| error.into_inner()).drain(..) {
      task.abort();
    }
    if let Some(mut child) = self.child.lock().await.take() {
      if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
      {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(3), child.wait())
          .await
          .map_err(|_| InteractionError::unavailable("Pi child cleanup timed out"))?
          .map_err(|error| InteractionError::unavailable(format!("Pi child cleanup failed: {error}")))?;
      }
    }
    Ok(())
  }
}

impl Drop for Inner {
  fn drop(&mut self) {
    for task in self
      .tasks
      .get_mut()
      .unwrap_or_else(|error| error.into_inner())
      .drain(..)
    {
      task.abort();
    }
    // Managed children also use kill_on_drop. Attached endpoints never store a child.
    if let Some(child) = self.child.get_mut().as_mut() {
      let _ = child.start_kill();
    }
  }
}

pub(super) fn start_reader<R>(inner: &Arc<Inner>, reader: R)
where
  R: AsyncRead + Unpin + Send + 'static,
{
  let weak: Weak<Inner> = Arc::downgrade(inner);
  let task = tokio::spawn(async move {
    let mut reader = framing::RpcReader::new(reader);
    loop {
      let frame = reader.read_frame().await;
      let Some(inner) = weak.upgrade() else { return };
      let result = match frame {
        Ok(Some(value)) => inner.receive(value).await,
        Ok(None) => Err("Pi closed its RPC output".into()),
        Err(error) => Err(error),
      };
      if let Err(reason) = result {
        inner.disconnected(reason);
        return;
      }
    }
  });
  inner.tasks.lock().unwrap_or_else(|error| error.into_inner()).push(task);
  let weak = Arc::downgrade(inner);
  let expiry = tokio::spawn(async move {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
      tick.tick().await;
      let Some(inner) = weak.upgrade() else { break };
      if inner.closed.load(Ordering::Acquire) {
        break;
      }
      // A single tracked task bounds timer resources even if many questions are
      // answered quickly. It never cancels the LF reader mid-frame.
      inner.expire_dialogs().await;
    }
  });
  inner
    .tasks
    .lock()
    .unwrap_or_else(|error| error.into_inner())
    .push(expiry);
}
