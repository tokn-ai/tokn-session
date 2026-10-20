use super::{
  Inner, WorkerState,
  persistence::{Receipt, validate_session},
  protocol::{RpcReader, RpcState, StderrTail, parse_extension_dialog, parse_state},
};
use crate::{
  input::{Admission, DeliveryError},
  workspace::{ManagedSessionDialogResponse, ManagedSessionStatus},
};
use serde_json::{Value, json};
use std::{
  collections::HashMap,
  ffi::{OsStr, OsString},
  fs::File,
  path::PathBuf,
  process::Stdio,
  sync::{Arc, Mutex, Weak},
  time::Duration,
};
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  process::{Child, ChildStdin},
  sync::{OwnedMutexGuard, mpsc, oneshot, watch},
  time::{Instant, MissedTickBehavior},
};

const RPC_DEADLINE: Duration = Duration::from_secs(15);
// Prompt preflight may legitimately await extension input or compaction before
// acknowledgement. The actor continues reading events and accepting Stop.
const PROMPT_DEADLINE: Duration = Duration::from_secs(300);
const WRITE_DEADLINE: Duration = Duration::from_secs(3);
const DIALOG_DEADLINE: Duration = Duration::from_secs(120);
const MAX_TURN_ERROR_BYTES: usize = 2048;

pub(super) enum Command {
  Prompt {
    request_id: String,
    text: String,
    reply: oneshot::Sender<Result<Admission, DeliveryError>>,
  },
  Abort(oneshot::Sender<Result<(), String>>),
  Dialog(ManagedSessionDialogResponse, oneshot::Sender<Result<(), String>>),
}

enum PendingKind {
  State {
    initial: bool,
  },
  Prompt {
    request_id: String,
    reply: Option<oneshot::Sender<Result<Admission, DeliveryError>>>,
    busy: bool,
  },
  Abort(oneshot::Sender<Result<(), String>>),
}

struct Pending {
  command: &'static str,
  deadline: Instant,
  kind: PendingKind,
}

pub(super) async fn run(
  inner: Weak<Inner>,
  lease: File,
  executable: PathBuf,
  receipt: Receipt,
  generation: u64,
  prompt: Option<String>,
  resume: bool,
  mut commands: mpsc::Receiver<Command>,
  worker_state: watch::Sender<WorkerState>,
) {
  let id = receipt.request_id.clone();
  let mut actor = match Actor::spawn(
    inner.clone(),
    executable,
    receipt,
    generation,
    prompt,
    resume,
    worker_state.clone(),
  )
  .await
  {
    Ok(actor) => actor,
    Err(error) => {
      commands.close();
      worker_state.send_replace(WorkerState::Closing(Some(error.clone())));
      let _retirement = retirement_gate(&inner, &id).await;
      reject_queued_commands(&mut commands, &error);
      let finished = finish(&inner, &id, generation, Some(error.clone())).await;
      drop(lease);
      worker_state.send_replace(WorkerState::Stopped(Some(error)));
      publish_finished(&inner, finished);
      return;
    }
  };
  let result = actor.run(&mut commands).await;
  commands.close();
  worker_state.send_replace(WorkerState::Closing(result.as_ref().err().cloned()));
  // Wake startup waiters before acquiring their admission gate. All exits hold
  // it through cleanup, including failures and viewer shutdown.
  if actor.retirement.is_none() {
    actor.retirement = retirement_gate(&inner, &id).await;
  }
  reject_queued_commands(&mut commands, "Pi stopped before receiving the message");
  // Pending deliveries are uncertain after writing, even when the subprocess
  // exits before its correlated response arrives.
  for (_, pending) in actor.pending.drain() {
    match pending.kind {
      PendingKind::Prompt { reply: Some(reply), .. } => {
        let _ = reply.send(Err(DeliveryError::uncertain(
          "Pi exited before acknowledging the message; check its history before retrying",
        )));
      }
      PendingKind::Abort(reply) => {
        let _ = reply.send(Err("Pi stopped before acknowledging Stop".into()));
      }
      _ => {}
    }
  }
  actor.reader.abort();
  // Tokio's Unix ChildStdin::shutdown does not close the pipe. Dropping the
  // handle delivers EOF and lets Pi dispose extensions before the kill bound.
  drop(actor.stdin.take());
  if result.is_ok() {
    let _ = tokio::time::timeout(Duration::from_secs(2), actor.child.wait()).await;
  }
  let _ = actor.child.start_kill();
  let _ = tokio::time::timeout(Duration::from_secs(3), actor.child.wait()).await;
  let _ = tokio::time::timeout(Duration::from_millis(100), &mut actor.stderr_reader).await;
  actor.stderr_reader.abort();
  let error = result.err().map(|error| actor.exit_message(error));
  let stopped = actor.stop_reply.take();
  // Keep the per-session lease until its final durable revision is visible.
  // Otherwise a new owner could resume from the previous revision while this
  // actor is still publishing its terminal metadata.
  let finished = finish(&inner, &id, generation, error.clone()).await;
  let retirement = actor.retirement.take();
  drop(actor);
  drop(lease);
  worker_state.send_replace(WorkerState::Stopped(error));
  publish_finished(&inner, finished);
  drop(retirement);
  if let Some(reply) = stopped {
    let _ = reply.send(Ok(()));
  }
}

struct Actor {
  inner: Weak<Inner>,
  receipt: Receipt,
  generation: u64,
  child: Child,
  stdin: Option<ChildStdin>,
  frames: mpsc::Receiver<Result<Value, String>>,
  reader: tokio::task::JoinHandle<()>,
  stderr_reader: tokio::task::JoinHandle<()>,
  stderr: Arc<Mutex<StderrTail>>,
  pending: HashMap<String, Pending>,
  recent: HashMap<String, (String, Option<Result<Admission, DeliveryError>>)>,
  initial_prompt: Option<String>,
  state: Option<RpcState>,
  saw_settled: bool,
  checking_idle: bool,
  dialog_deadline: Option<Instant>,
  stop_reply: Option<oneshot::Sender<Result<(), String>>>,
  exit_requested: bool,
  pending_outcome: Option<Option<String>>,
  awaiting_new_turn: bool,
  stop_requested: bool,
  worker_state: watch::Sender<WorkerState>,
  idle_since: Option<Instant>,
  idle_probe: bool,
  retirement: Option<OwnedMutexGuard<()>>,
}

impl Actor {
  async fn spawn(
    inner: Weak<Inner>,
    executable: PathBuf,
    receipt: Receipt,
    generation: u64,
    prompt: Option<String>,
    resume: bool,
    worker_state: watch::Sender<WorkerState>,
  ) -> Result<Self, String> {
    if inner
      .upgrade()
      .is_none_or(|inner| inner.shutting_down.load(std::sync::atomic::Ordering::Acquire))
    {
      return Err("Managed Pi runtime is shutting down".into());
    }
    let mut command = tokio::process::Command::new(executable);
    command
      .args(["--mode", "rpc"])
      .current_dir(&receipt.folder)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true);
    let node_extra_ca_certs = std::env::var_os("NODE_EXTRA_CA_CERTS");
    let ssl_cert_file = std::env::var_os("SSL_CERT_FILE");
    if let Some(path) = inherited_node_ca_fallback(node_extra_ca_certs.as_deref(), ssl_cert_file.as_deref()) {
      command.env("NODE_EXTRA_CA_CERTS", path);
    }
    if resume {
      command.arg("--session").arg(receipt.session_file.as_ref().unwrap());
    }
    let mut child = command.spawn().map_err(|error| {
      format!("Could not start Pi: {error}. Install Pi or set TOKN_SESSION_PI_EXECUTABLE to its executable path.")
    })?;
    let stdin = child.stdin.take().ok_or("Pi stdin is unavailable")?;
    let stdout = child.stdout.take().ok_or("Pi stdout is unavailable")?;
    let mut stderr_stream = child.stderr.take().ok_or("Pi stderr is unavailable")?;
    let (send, frames) = mpsc::channel(8);
    let reader = tokio::spawn(async move {
      let mut reader = RpcReader::new(stdout);
      loop {
        match reader.read_frame().await {
          Ok(Some(value)) => {
            if let Some(metadata) = project_frame(value) {
              if send.send(Ok(metadata)).await.is_err() {
                break;
              }
            }
          }
          Ok(None) => {
            let _ = send.send(Err("Pi closed its RPC output".into())).await;
            break;
          }
          Err(error) => {
            let _ = send.send(Err(error)).await;
            break;
          }
        }
      }
    });
    let stderr = Arc::new(Mutex::new(StderrTail::default()));
    let tail = stderr.clone();
    let stderr_reader = tokio::spawn(async move {
      let mut buffer = [0; 4096];
      while let Ok(count) = stderr_stream.read(&mut buffer).await {
        if count == 0 {
          break;
        }
        tail.lock().unwrap().push(&buffer[..count]);
      }
    });
    Ok(Self {
      inner,
      receipt,
      generation,
      child,
      stdin: Some(stdin),
      frames,
      reader,
      stderr_reader,
      stderr,
      pending: HashMap::new(),
      recent: HashMap::new(),
      initial_prompt: prompt,
      state: None,
      saw_settled: false,
      checking_idle: false,
      dialog_deadline: None,
      stop_reply: None,
      exit_requested: false,
      pending_outcome: None,
      awaiting_new_turn: false,
      stop_requested: false,
      worker_state,
      idle_since: None,
      idle_probe: false,
      retirement: None,
    })
  }

  async fn run(&mut self, commands: &mut mpsc::Receiver<Command>) -> Result<(), String> {
    if self
      .inner
      .upgrade()
      .is_none_or(|inner| inner.shutting_down.load(std::sync::atomic::Ordering::Acquire))
    {
      return Ok(());
    }
    self.request_state(true).await?;
    let mut shutdown = self
      .inner
      .upgrade()
      .ok_or("Managed Pi runtime has stopped")?
      .shutdown
      .subscribe();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
      let stopping = *shutdown.borrow_and_update();
      if stopping {
        self.begin_shutdown().await;
        return Ok(());
      }
      tokio::select! {
        _ = shutdown.changed() => {
          self.begin_shutdown().await;
          return Ok(());
        },
        frame = self.frames.recv() => match frame {
          Some(Ok(value)) => {self.frame(value).await?; if self.exit_requested {return Ok(());}},
          Some(Err(error)) => return Err(error),
          None => return Err("Pi RPC reader stopped".into()),
        },
        command = commands.recv() => {
          self.idle_since = None;
          self.idle_probe = false;
          match command {
          Some(Command::Prompt {request_id, text, reply}) => {
            if self.inner.upgrade().is_none_or(|inner| inner.shutting_down.load(std::sync::atomic::Ordering::Acquire)) {
              let _ = reply.send(Err(DeliveryError::not_sent("Managed Pi runtime is shutting down")));
            } else {
              self.prompt(request_id, text, Some(reply)).await?;
            }
          },
          Some(Command::Abort(reply)) => {
            self.stop_requested = true;
            self.awaiting_new_turn = false;
            self.initial_prompt = None;
            self.cancel_dialog("Pi dialog was cancelled by Stop").await?;
            self.update(|receipt| { receipt.status = ManagedSessionStatus::Stopping; receipt.message = Some("Stopping the current Pi turn".into()); }).await?;
            self.send("abort", json!({"type":"abort"}), PendingKind::Abort(reply)).await?;
          }
          Some(Command::Dialog(response, reply)) => { let result = self.respond(response).await; let _ = reply.send(result); }
          None => return Ok(()),
        }},
        _ = tick.tick() => self.tick().await?,
        exit = self.child.wait() => {
          return match exit { Ok(status) if status.success() => Ok(()), Ok(status) => Err(format!("Pi exited with {status}")), Err(error) => Err(error.to_string()) };
        }
      }
      if self.retire_if_idle(commands).await? {
        return Ok(());
      }
    }
  }

  async fn begin_shutdown(&mut self) {
    // Shutdown has its own signal and a shared write budget, so full command
    // queues or an extension dialog cannot postpone closing the worker's stdin.
    let _ = tokio::time::timeout(WRITE_DEADLINE, async {
      let _ = self.cancel_dialog("Pi runtime is stopping").await;
      let _ = self
        .write(json!({"type":"abort","id":uuid::Uuid::new_v4().to_string()}))
        .await;
    })
    .await;
    drop(self.stdin.take());
  }

  async fn frame(&mut self, value: Value) -> Result<(), String> {
    match value.get("type").and_then(Value::as_str).unwrap_or_default() {
      "response" => {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
          return Err("Pi RPC response has no correlation ID".into());
        };
        let Some(pending) = self.pending.remove(id) else {
          return Ok(());
        };
        if value.get("command").and_then(Value::as_str) != Some(pending.command) {
          return Err("Pi RPC response command did not match its request".into());
        }
        let success = value
          .get("success")
          .and_then(Value::as_bool)
          .ok_or("Pi RPC response has no success flag")?;
        let error = value
          .get("error")
          .and_then(Value::as_str)
          .unwrap_or("Pi rejected the request")
          .chars()
          .take(2048)
          .collect::<String>();
        match pending.kind {
          PendingKind::State { initial } => {
            if !success {
              return Err(error);
            }
            let state = parse_state(value.get("data").ok_or("Pi get_state response has no data")?)?;
            if let Some(expected) = self
              .state
              .as_ref()
              .map(|state| state.session_id.as_str())
              .or(self.receipt.session_id.as_deref())
            {
              if state.session_id != expected {
                return Err("Pi resumed a different session identity".into());
              }
            }
            self.state = Some(state);
            self.probe_file().await?;
            if initial {
              if let Some(prompt) = self.initial_prompt.take() {
                self.prompt(self.receipt.request_id.clone(), prompt, None).await?;
              } else {
                self.set_phase_from_state().await?;
              }
              self.worker_state.send_replace(WorkerState::Ready);
            } else if self.checking_idle || self.idle_probe {
              self.set_phase_from_state().await?;
            }
          }
          PendingKind::Prompt {
            request_id,
            reply,
            busy,
          } => {
            let initial = reply.is_none();
            let result = if success {
              Ok(if busy {
                Admission::QueuedFollowUp
              } else {
                Admission::Started
              })
            } else {
              Err(DeliveryError::not_sent(error.clone()))
            };
            if let Some(recent) = self.recent.get_mut(&request_id) {
              recent.1 = Some(result.clone());
            }
            if let Some(reply) = reply {
              let _ = reply.send(result);
            }
            if !success && initial {
              return Err(error);
            }
            if success && !busy {
              self.confirm_new_turn().await?;
            }
            if !success && !busy {
              self.awaiting_new_turn = false;
            }
            if !success {
              self
                .update(|receipt| {
                  receipt.message = Some(error);
                })
                .await?;
            }
            // Extensions can consume a prompt without starting an agent. The
            // correlated acknowledgement alone does not prove a running turn.
            self.checking_idle = true;
            self.request_state(false).await?;
          }
          PendingKind::Abort(reply) => {
            if !success {
              self.stop_requested = false;
            }
            self.probe_file().await?;
            if success && self.receipt.session_key.is_none() {
              self.stop_reply = Some(reply);
              self.exit_requested = true;
              return Ok(());
            }
            if success {
              self.checking_idle = true;
              self.request_state(false).await?;
            }
            let _ = reply.send(if success { Ok(()) } else { Err(error) });
          }
        }
      }
      "agent_start" => {
        self.confirm_new_turn().await?;
        self.checking_idle = false;
        if let Some(state) = &mut self.state {
          state.is_streaming = true;
        }
        self.phase(ManagedSessionStatus::Running).await?;
      }
      "agent_settled" => {
        self
          .cancel_dialog("Pi turn settled and its dialog was cancelled")
          .await?;
        self.saw_settled = true;
        self.checking_idle = false;
        if let Some(state) = &mut self.state {
          state.is_streaming = false;
          state.is_compacting = false;
          state.pending_message_count = 0;
        }
        self.settle_turn().await?;
        self.request_state(false).await?;
      }
      "agent_end" if !self.saw_settled => {
        self.checking_idle = true;
      }
      "message_end" => {
        self.confirm_new_turn().await?;
        match value.get("stop_reason").and_then(Value::as_str) {
          Some("aborted") if self.stop_requested => {
            self.pending_outcome = None;
          }
          Some("aborted") => {}
          Some("error") => {
            self.pending_outcome = Some(Some(
              turn_error(value.get("error_message")).unwrap_or_else(|| "Pi could not complete this response.".into()),
            ));
          }
          _ => {
            self.pending_outcome = Some(turn_error(value.get("error_message")));
          }
        }
      }
      "auto_retry_end" => {
        if self.stop_requested {
          return Ok(());
        }
        match value.get("success").and_then(Value::as_bool) {
          Some(true) => {
            self.pending_outcome = Some(None);
          }
          Some(false) => {
            if let Some(error) = turn_error(value.get("final_error")) {
              self.pending_outcome = Some(Some(error));
            }
          }
          None => {}
        }
        if !self.saw_settled {
          self.checking_idle = true;
        }
      }
      "compaction_start" | "auto_compaction_start" | "auto_retry_start" => {
        self.checking_idle = false;
        self.phase(ManagedSessionStatus::Running).await?;
      }
      "compaction_end" | "auto_compaction_end" => {
        if !self.stop_requested && value.get("aborted").and_then(Value::as_bool) != Some(true) {
          if let Some(error) = turn_error(value.get("error_message")) {
            self.pending_outcome = Some(Some(error));
          }
        }
        if !self.saw_settled {
          self.checking_idle = true;
        }
      }
      "extension_ui_request" => self.dialog(value).await?,
      _ => {}
    }
    Ok(())
  }

  async fn prompt(
    &mut self,
    request_id: String,
    text: String,
    reply: Option<oneshot::Sender<Result<Admission, DeliveryError>>>,
  ) -> Result<(), String> {
    if self
      .inner
      .upgrade()
      .is_none_or(|inner| inner.shutting_down.load(std::sync::atomic::Ordering::Acquire))
    {
      if let Some(reply) = reply {
        let _ = reply.send(Err(DeliveryError::not_sent("Managed Pi runtime is shutting down")));
      }
      return Ok(());
    }
    use sha2::{Digest, Sha256};
    let fingerprint = format!("{:x}", Sha256::digest(text.as_bytes()));
    if let Some((existing, result)) = self.recent.get(&request_id) {
      let result = if existing != &fingerprint {
        Err(DeliveryError::not_sent(
          "Message request ID was already used with different content",
        ))
      } else {
        result.clone().unwrap_or_else(|| {
          Err(DeliveryError::uncertain(
            "The original message is still awaiting Pi acknowledgement",
          ))
        })
      };
      if let Some(reply) = reply {
        let _ = reply.send(result);
      }
      return Ok(());
    }
    if self.state.is_none() {
      if let Some(reply) = reply {
        let _ = reply.send(Err(DeliveryError::not_sent("Pi is still starting")));
      }
      return Ok(());
    }
    if self.recent.len() >= 1024 {
      if let Some(reply) = reply {
        let _ = reply.send(Err(DeliveryError::not_sent(
          "Managed runtime message receipt limit reached; resume this session to continue",
        )));
      }
      return Ok(());
    }
    let busy = matches!(
      self.receipt.status,
      ManagedSessionStatus::Running | ManagedSessionStatus::Stopping
    ) || self
      .state
      .as_ref()
      .is_some_and(|state| state.is_streaming || state.is_compacting || state.pending_message_count > 0);
    if !busy {
      self.awaiting_new_turn = true;
      self.stop_requested = false;
    }
    self.recent.insert(request_id.clone(), (fingerprint, None));
    let mut command = json!({"type":"prompt","message":text});
    if busy {
      command["streamingBehavior"] = json!("followUp");
    }
    self
      .send(
        "prompt",
        command,
        PendingKind::Prompt {
          request_id,
          reply,
          busy,
        },
      )
      .await?;
    self.phase(ManagedSessionStatus::Running).await
  }

  async fn send(&mut self, command: &'static str, mut value: Value, kind: PendingKind) -> Result<(), String> {
    if self.pending.len() >= 32 {
      return Err("Pi RPC pending request limit reached".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    value["id"] = json!(id);
    // Insert before writing: a partial pipe write is uncertain and must never
    // be automatically repeated by the host.
    let timeout = if command == "prompt" {
      PROMPT_DEADLINE
    } else {
      RPC_DEADLINE
    };
    self.pending.insert(
      id,
      Pending {
        command,
        deadline: Instant::now() + timeout,
        kind,
      },
    );
    self.write(value).await
  }

  async fn write(&mut self, value: Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let stdin = self.stdin.as_mut().ok_or("Pi RPC stdin is closed")?;
    tokio::time::timeout(WRITE_DEADLINE, stdin.write_all(&bytes))
      .await
      .map_err(|_| "Pi RPC write timed out".to_string())?
      .map_err(|error| format!("Pi RPC write failed: {error}"))
  }

  async fn request_state(&mut self, initial: bool) -> Result<(), String> {
    if self
      .pending
      .values()
      .any(|pending| matches!(pending.kind, PendingKind::State { .. }))
    {
      return Ok(());
    }
    self
      .send("get_state", json!({"type":"get_state"}), PendingKind::State { initial })
      .await
  }

  async fn tick(&mut self) -> Result<(), String> {
    if self.receipt.dialog.is_none() && self.pending.values().any(|pending| pending.deadline <= Instant::now()) {
      return Err("Pi RPC acknowledgement timed out; check persisted history before retrying".into());
    }
    if self.dialog_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
      self.cancel_dialog("Pi dialog expired and was cancelled").await?;
    }
    if self.receipt.session_key.is_none() {
      self.probe_file().await?;
      self.request_state(false).await?;
    } else if self.checking_idle {
      self.request_state(false).await?;
    }
    Ok(())
  }

  async fn retire_if_idle(&mut self, commands: &mut mpsc::Receiver<Command>) -> Result<bool, String> {
    let eligible = self.receipt.status == ManagedSessionStatus::Idle
      && self.receipt.session_key.is_some()
      && self
        .state
        .as_ref()
        .is_some_and(|state| !state.is_streaming && !state.is_compacting && state.pending_message_count == 0)
      && self.receipt.dialog.is_none()
      && self.initial_prompt.is_none()
      && !self.awaiting_new_turn
      && !self.checking_idle
      && self.pending_outcome.is_none()
      && !self.stop_requested;
    if !eligible || !commands.is_empty() || (!self.pending.is_empty() && !self.idle_probe) {
      self.idle_since = None;
      self.idle_probe = false;
      return Ok(false);
    }
    // Preserve the deadline while checking native state once more. This also
    // catches work queued by an extension without an incoming viewer command.
    if !self.pending.is_empty() {
      return Ok(false);
    }
    if !self.frames.is_empty() {
      return Ok(false);
    }
    let Some(inner) = self.inner.upgrade() else {
      return Ok(true);
    };
    let since = self.idle_since.get_or_insert_with(Instant::now);
    if since.elapsed() < inner.idle_timeout {
      return Ok(false);
    }
    if !self.idle_probe {
      self.idle_probe = true;
      self.request_state(false).await?;
      return Ok(false);
    }
    let gate = inner
      .records
      .lock()
      .unwrap()
      .get(&self.receipt.request_id)
      .map(|record| record.start_gate.clone());
    let Some(gate) = gate else {
      return Ok(true);
    };
    let Ok(guard) = gate.try_lock_owned() else {
      return Ok(false);
    };
    // Send holds this same gate through queue admission. Closing an empty
    // receiver under it cannot strand a message that has already been admitted.
    if !commands.is_empty() || !self.frames.is_empty() {
      return Ok(false);
    }
    commands.close();
    self.retirement = Some(guard);
    Ok(true)
  }

  async fn probe_file(&mut self) -> Result<(), String> {
    let Some(state) = &self.state else {
      return Ok(());
    };
    let Some(path) = state.session_file.clone() else {
      return Ok(());
    };
    if !path.is_absolute() {
      return Err("Pi returned a relative session file path".into());
    }
    let id = state.session_id.clone();
    let folder = self.receipt.folder.clone();
    let validated = tokio::task::spawn_blocking(move || validate_session(&path, &id, &folder))
      .await
      .map_err(|error| error.to_string())??;
    if let Some((path, key)) = validated {
      if self.receipt.session_key.as_ref() != Some(&key) {
        let session_id = state.session_id.clone();
        self
          .update(|receipt| {
            receipt.session_id = Some(session_id);
            receipt.session_file = Some(path);
            receipt.session_key = Some(key);
          })
          .await?;
      }
    }
    Ok(())
  }

  async fn set_phase_from_state(&mut self) -> Result<(), String> {
    let idle = self
      .state
      .as_ref()
      .is_some_and(|state| !state.is_streaming && !state.is_compacting && state.pending_message_count == 0);
    if idle {
      self.checking_idle = false;
      self.settle_turn().await
    } else {
      self.phase(ManagedSessionStatus::Running).await
    }
  }

  async fn confirm_new_turn(&mut self) -> Result<(), String> {
    if !self.awaiting_new_turn {
      return Ok(());
    }
    self.awaiting_new_turn = false;
    if self.receipt.last_turn_error.is_some() {
      self
        .update(|receipt| {
          receipt.last_turn_error = None;
        })
        .await?;
    }
    Ok(())
  }

  async fn settle_turn(&mut self) -> Result<(), String> {
    let pending = self.pending_outcome.take();
    let error = if self.stop_requested {
      self.receipt.last_turn_error.clone()
    } else {
      pending.unwrap_or_else(|| self.receipt.last_turn_error.clone())
    };
    self.stop_requested = false;
    if self.receipt.status != ManagedSessionStatus::Idle || self.receipt.last_turn_error != error {
      self
        .update(|receipt| {
          receipt.status = ManagedSessionStatus::Idle;
          receipt.message = None;
          receipt.last_turn_error = error;
        })
        .await?;
    }
    Ok(())
  }

  async fn phase(&mut self, status: ManagedSessionStatus) -> Result<(), String> {
    if self.receipt.status != status {
      self
        .update(|receipt| {
          receipt.status = status;
          receipt.message = None;
        })
        .await?;
    }
    Ok(())
  }

  async fn dialog(&mut self, value: Value) -> Result<(), String> {
    let native_id = value
      .get("id")
      .and_then(Value::as_str)
      .filter(|id| id.len() <= 1024)
      .map(str::to_owned);
    match parse_extension_dialog(&value) {
      Ok(None) => {}
      Ok(Some(dialog)) => {
        if let Some(previous) = &self.receipt.dialog {
          self
            .write(json!({"type":"extension_ui_response","id":previous.dialog_id,"cancelled":true}))
            .await?;
        }
        let timeout = value
          .get("timeout")
          .and_then(Value::as_u64)
          .filter(|timeout| *timeout > 0)
          .map(Duration::from_millis)
          .unwrap_or(DIALOG_DEADLINE)
          .min(DIALOG_DEADLINE);
        self.dialog_deadline = Some(Instant::now() + timeout);
        self
          .update(|receipt| {
            receipt.dialog = Some(dialog);
            receipt.message = None;
          })
          .await?;
      }
      Err(reason) => {
        if let Some(id) = native_id {
          self
            .write(json!({"type":"extension_ui_response","id":id,"cancelled":true}))
            .await?;
        }
        self
          .update(|receipt| {
            receipt.message = Some(reason);
          })
          .await?;
      }
    }
    Ok(())
  }

  async fn respond(&mut self, response: ManagedSessionDialogResponse) -> Result<(), String> {
    let dialog = self
      .receipt
      .dialog
      .as_ref()
      .filter(|dialog| dialog.dialog_id == response.dialog_id)
      .ok_or("This Pi dialog is no longer pending")?;
    let mut value = json!({"type":"extension_ui_response","id":dialog.dialog_id});
    if response.cancelled {
      value["cancelled"] = json!(true);
    } else if dialog.kind == "confirm" {
      value["confirmed"] = json!(response.confirmed.ok_or("Confirm requires a yes or no response")?);
    } else {
      let answer = response.value.ok_or("This Pi dialog requires a value")?;
      if answer.len() > 16 * 1024 {
        return Err("Pi dialog response exceeds its size limit".into());
      }
      if dialog.kind == "select" && !dialog.options.contains(&answer) {
        return Err("Pi dialog response is not an available option".into());
      }
      value["value"] = json!(answer);
    }
    self.write(value).await?;
    self.dialog_deadline = None;
    self.refresh_deadlines();
    self
      .update(|receipt| {
        receipt.dialog = None;
      })
      .await
  }

  fn refresh_deadlines(&mut self) {
    for pending in self.pending.values_mut() {
      pending.deadline = Instant::now()
        + if pending.command == "prompt" {
          PROMPT_DEADLINE
        } else {
          RPC_DEADLINE
        };
    }
  }

  async fn cancel_dialog(&mut self, reason: &str) -> Result<(), String> {
    if let Some(dialog) = &self.receipt.dialog {
      self
        .write(json!({"type":"extension_ui_response","id":dialog.dialog_id,"cancelled":true}))
        .await?;
      self.dialog_deadline = None;
      self.refresh_deadlines();
      self
        .update(|receipt| {
          receipt.dialog = None;
          receipt.message = Some(reason.into());
        })
        .await?;
    }
    Ok(())
  }

  async fn update(&mut self, update: impl FnOnce(&mut Receipt)) -> Result<(), String> {
    update(&mut self.receipt);
    let receipt = self.receipt.clone();
    let id = receipt.request_id.clone();
    if let Some(inner) = self.inner.upgrade() {
      inner
        .mutate(&id, self.generation, |current| {
          // The actor is the only writer of lifecycle metadata in this generation.
          let revision = current.revision;
          *current = receipt;
          current.revision = revision;
        })
        .await?;
    }
    Ok(())
  }

  fn exit_message(&self, message: String) -> String {
    let stderr = self.stderr.lock().unwrap().text();
    if stderr.trim().is_empty() {
      message
    } else {
      format!("{message}: {}", stderr.trim())
    }
  }
}

async fn finish(
  inner: &Weak<Inner>,
  id: &str,
  generation: u64,
  error: Option<String>,
) -> Option<crate::workspace::ManagedSessionSnapshot> {
  let Some(inner) = inner.upgrade() else {
    return None;
  };
  let receipt = {
    let records = inner.records.lock().unwrap();
    let record = records.get(id).filter(|record| record.generation == generation)?;
    let mut receipt = record.receipt.clone();
    receipt.status = if error.is_some() {
      ManagedSessionStatus::Failed
    } else {
      ManagedSessionStatus::Stopped
    };
    receipt.message = error;
    receipt.dialog = None;
    receipt.revision = receipt.revision.saturating_add(1);
    receipt
  };
  let store = inner.store.clone();
  let saved = receipt.clone();
  let _ = tokio::task::spawn_blocking(move || store.save(saved)).await;
  let mut records = inner.records.lock().unwrap();
  if let Some(record) = records.get_mut(id).filter(|record| record.generation == generation) {
    record.receipt = receipt.clone();
    record.sender = None;
  }
  Some(receipt.snapshot())
}

async fn retirement_gate(inner: &Weak<Inner>, id: &str) -> Option<OwnedMutexGuard<()>> {
  let gate = inner.upgrade()?.records.lock().unwrap().get(id)?.start_gate.clone();
  Some(gate.lock_owned().await)
}

fn reject_queued_commands(commands: &mut mpsc::Receiver<Command>, message: &str) {
  while let Ok(command) = commands.try_recv() {
    match command {
      Command::Prompt { reply, .. } => {
        let _ = reply.send(Err(DeliveryError::not_sent(message)));
      }
      Command::Abort(reply) | Command::Dialog(_, reply) => {
        let _ = reply.send(Err("Pi runtime has stopped".into()));
      }
    }
  }
}

fn publish_finished(inner: &Weak<Inner>, snapshot: Option<crate::workspace::ManagedSessionSnapshot>) {
  // A stopped event can immediately trigger an input-capability check. Publish
  // only after releasing the process lease so that check can safely resume.
  if let (Some(inner), Some(snapshot)) = (inner.upgrade(), snapshot) {
    let _ = inner.events.send(snapshot);
  }
}

/// Retain only coarse lifecycle metadata across the bounded actor channel.
/// Token events and completed assistant/tool bodies belong to native history.
fn project_frame(value: Value) -> Option<Value> {
  let kind = value.get("type").and_then(Value::as_str)?;
  match kind {
    "response" | "extension_ui_request" => Some(value),
    "message_end" => {
      let message = value.get("message")?;
      if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
      }
      Some(
        json!({"type":"message_end", "stop_reason":message.get("stopReason").and_then(Value::as_str).map(|reason| reason.chars().take(128).collect::<String>()), "error_message":turn_error(message.get("errorMessage"))}),
      )
    }
    "auto_retry_end" => Some(
      json!({"type":"auto_retry_end", "success":value.get("success").and_then(Value::as_bool), "final_error":turn_error(value.get("finalError"))}),
    ),
    "compaction_end" | "auto_compaction_end" => Some(
      json!({"type":kind, "aborted":value.get("aborted").and_then(Value::as_bool), "error_message":turn_error(value.get("errorMessage"))}),
    ),
    "agent_start"
    | "agent_end"
    | "agent_settled"
    | "compaction_start"
    | "auto_compaction_start"
    | "auto_retry_start" => Some(json!({"type":kind})),
    _ => None,
  }
}

fn turn_error(value: Option<&Value>) -> Option<String> {
  let text = value.and_then(Value::as_str)?.trim();
  if text.is_empty() {
    return None;
  }
  let mut end = text.len().min(MAX_TURN_ERROR_BYTES);
  while !text.is_char_boundary(end) {
    end -= 1;
  }
  Some(text[..end].to_owned())
}

/// Node does not automatically use SSL_CERT_FILE. Preserve an explicit Node
/// setting, including an empty one, while honoring the host's configured CA
/// bundle when Node has no setting of its own.
fn inherited_node_ca_fallback(node_extra_ca_certs: Option<&OsStr>, ssl_cert_file: Option<&OsStr>) -> Option<OsString> {
  if node_extra_ca_certs.is_some() {
    return None;
  }
  ssl_cert_file.filter(|path| !path.is_empty()).map(OsStr::to_os_string)
}

#[cfg(test)]
mod certificate_tests {
  use super::*;

  #[test]
  fn only_an_absent_node_setting_inherits_a_nonempty_host_bundle() {
    let host_bundle = OsStr::new("/configured CA bundle.pem");
    assert_eq!(
      inherited_node_ca_fallback(None, Some(host_bundle)),
      Some(host_bundle.to_owned())
    );
    assert_eq!(
      inherited_node_ca_fallback(Some(OsStr::new("/node-specific.pem")), Some(host_bundle)),
      None
    );
    assert_eq!(
      inherited_node_ca_fallback(Some(OsStr::new("")), Some(host_bundle)),
      None
    );
    assert_eq!(inherited_node_ca_fallback(None, Some(OsStr::new(""))), None);
    assert_eq!(inherited_node_ca_fallback(None, None), None);
    assert_eq!(
      inherited_node_ca_fallback(Some(OsStr::new("/node-specific.pem")), None),
      None
    );
  }
}

#[cfg(test)]
mod projection_tests {
  use super::*;

  #[test]
  fn projection_discards_tokens_and_bodies_but_keeps_bounded_turn_failure() {
    for kind in ["message_start", "message_update", "tool_execution_update", "turn_end"] {
      assert!(
        project_frame(
          json!({"type":kind,"message":{"role":"assistant","content":[{"type":"text","text":"private body"}]}})
        )
        .is_none()
      );
    }
    let projected = project_frame(json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"private body"}],"stopReason":"error","errorMessage":"x".repeat(MAX_TURN_ERROR_BYTES + 10)}})).unwrap();
    assert_eq!(projected["stop_reason"], "error");
    assert_eq!(projected["error_message"].as_str().unwrap().len(), MAX_TURN_ERROR_BYTES);
    assert!(!projected.to_string().contains("private body"));
    assert_eq!(
      project_frame(json!({"type":"agent_end","messages":[{"text":"private body"}]})).unwrap(),
      json!({"type":"agent_end"})
    );
    assert!(
      project_frame(json!({"type":"message_end","message":{"role":"user","errorMessage":"not an assistant failure"}}))
        .is_none()
    );
  }

  #[test]
  fn projection_keeps_retry_and_compaction_failure_evidence_only() {
    assert_eq!(
      project_frame(
        json!({"type":"auto_retry_end","success":false,"finalError":"quota exceeded","messages":["private body"]})
      )
      .unwrap(),
      json!({"type":"auto_retry_end","success":false,"final_error":"quota exceeded"})
    );
    assert_eq!(
      project_frame(
        json!({"type":"compaction_end","aborted":true,"errorMessage":"cancelled","result":{"summary":"private body"}})
      )
      .unwrap(),
      json!({"type":"compaction_end","aborted":true,"error_message":"cancelled"})
    );
  }
}
