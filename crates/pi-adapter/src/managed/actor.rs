//! Pi-native managed workflow driver. Generic ownership, receipts, and
//! lifecycle persistence live in `tokn_interaction_core::managed_sessions`.
use super::protocol::{RpcState, parse_extension_dialog, parse_state, pi_choices};
use crate::{EndpointOptions, LaunchOptions, PiAdapter, SupervisedSession};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, time::Duration};
use tokio::{
  sync::{OwnedMutexGuard, mpsc, oneshot, watch},
  time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use tokn_interaction_core::creation::FirstInputState;
use tokn_interaction_core::managed_sessions::*;
use tokn_pi_rpc_protocol as wire;

mod message_input;
mod model_control;
use message_input::{MessagePending, PreparedMessage};
use model_control::ModelPending;

const RPC_DEADLINE: Duration = Duration::from_secs(15);
// Prompt preflight may legitimately await extension input or compaction before
// acknowledgement. The actor continues reading events and accepting Stop.
const PROMPT_DEADLINE: Duration = Duration::from_secs(300);
const WRITE_DEADLINE: Duration = Duration::from_secs(3);
const DIALOG_DEADLINE: Duration = Duration::from_secs(120);
const MAX_TURN_ERROR_BYTES: usize = 2048;

enum PendingKind {
  InitialModels,
  Model(ModelPending),
  State {
    initial: bool,
  },
  Message(MessagePending),
  Prompt {
    request_id: String,
    reply: Option<oneshot::Sender<Result<Admission, DeliveryError>>>,
    busy: bool,
    model_result: Option<MessageModelResult>,
  },
  Abort(oneshot::Sender<Result<(), String>>),
}

struct Pending {
  command: &'static str,
  deadline: Instant,
  kind: PendingKind,
}

pub(super) async fn run(executable: PathBuf, worker: ManagedWorker) {
  let ManagedWorker {
    host,
    lease,
    native_lease,
    receipt,
    prompt,
    resume,
    mut commands,
    worker_state,
    termination,
  } = worker;
  let lifecycle = ManagedWorkerLifecycle::new(host.clone(), lease, native_lease.clone(), worker_state.clone());
  let mut actor = match Actor::spawn(
    host.clone(),
    executable,
    receipt,
    prompt,
    resume,
    worker_state.clone(),
    termination,
    native_lease.clone(),
  )
  .await
  {
    Ok(actor) => actor,
    Err(error) => {
      lifecycle.close_admission(&mut commands, Some(error.clone()), &error);
      drop(native_lease);
      lifecycle.complete(Some(error), true, None, None).await;
      return;
    }
  };
  let result = actor.run(&mut commands).await;
  lifecycle.close_admission(
    &mut commands,
    result.as_ref().err().cloned(),
    "Pi stopped before receiving the message",
  );
  for (_, pending) in actor.pending.drain() {
    match pending.kind {
      PendingKind::Prompt {
        reply: Some(reply),
        model_result,
        ..
      } => {
        let error =
          DeliveryError::uncertain("Pi exited before acknowledging the message; check its history before retrying");
        let error = model_result.map_or(error.clone(), |result| error.with_model_result(result));
        let _ = reply.send(Err(error));
      }
      PendingKind::Message(pending) => pending.reject(
        "Pi stopped before confirming the message; check its history before retrying",
        true,
      ),
      PendingKind::Model(pending) => pending.reject("Pi stopped before confirming the model request", true),
      PendingKind::Abort(reply) => {
        let _ = reply.send(Err("Pi stopped before acknowledging Stop".into()));
      }
      _ => {}
    }
  }
  if actor.retirement.is_none() {
    actor.retirement = host.retirement_gate().await;
  }
  // Retain both ownership leases until transport cleanup and durable final
  // metadata are complete, then publish the stopped observation.
  let cleanup = actor.session.close().await;
  let cleanup_confirmed = cleanup.is_ok();
  let error = result.err().or_else(|| cleanup.err().map(|error| error.to_string()));
  let error = error.map(|error| actor.exit_message(error));
  let stopped = actor.stop_reply.take();
  let retirement = actor.retirement.take();
  drop(actor);
  drop(native_lease);
  lifecycle.complete(error, cleanup_confirmed, retirement, stopped).await;
}

struct Actor {
  host: ManagedHostContext,
  receipt: Receipt,
  session: SupervisedSession,
  native_lease: Option<tokn_interaction_core::session_ownership::SessionLease>,
  pending: HashMap<String, Pending>,
  recent: ManagedMessageReceipts,
  initial_prompt: Option<(String, Vec<ResolvedImage>)>,
  initial_model: Option<tokn_interaction_core::ModelIdentity>,
  initial_model_verified: bool,
  state: Option<RpcState>,
  model_revision: u64,
  saw_settled: bool,
  checking_idle: bool,
  dialog_deadline: Option<Instant>,
  stop_reply: Option<oneshot::Sender<Result<(), String>>>,
  exit_requested: bool,
  pending_outcome: Option<Option<String>>,
  awaiting_new_turn: bool,
  stop_requested: bool,
  worker_state: watch::Sender<WorkerState>,
  idle_policy: ManagedIdlePolicy,
  idle_probe: bool,
  retirement: Option<OwnedMutexGuard<()>>,
  termination: CancellationToken,
}

impl Actor {
  async fn spawn(
    host: ManagedHostContext,
    executable: PathBuf,
    receipt: Receipt,
    prompt: Option<(String, Vec<ResolvedImage>)>,
    resume: bool,
    worker_state: watch::Sender<WorkerState>,
    termination: CancellationToken,
    native_lease: Option<tokn_interaction_core::session_ownership::SessionLease>,
  ) -> Result<Self, String> {
    if host.is_shutting_down() {
      return Err("Managed Pi runtime is shutting down".into());
    }
    let instance_id = host.instance_id().ok_or("Managed Pi runtime has stopped")?;
    let mut options = EndpointOptions::new(format!("pi:{}", receipt.request_id));
    options.instance_id = instance_id;
    options.expected_session_id = receipt.session_id.clone();
    options.request_timeout = RPC_DEADLINE;
    options.prompt_timeout = PROMPT_DEADLINE;
    let initial_model = (!resume).then(|| receipt.model.clone()).flatten();
    let mut arguments = Vec::new();
    if let Some(model) = &initial_model {
      validate_creation_model(model)?;
      let provider = model
        .model_provider
        .as_ref()
        .ok_or("Pi creation model requires its provider")?;
      arguments.extend([
        "--provider".into(),
        provider.into(),
        "--model".into(),
        model.model_id.clone().into(),
      ]);
    }
    let session = PiAdapter::spawn_supervised(
      options,
      LaunchOptions {
        executable,
        working_directory: receipt.folder.clone(),
        arguments,
        environment: Default::default(),
        session_file: resume.then(|| receipt.session_file.clone()).flatten(),
      },
    )
    .await
    .map_err(|error| {
      format!("Could not start Pi: {error}. Install Pi or set TOKN_SESSION_PI_EXECUTABLE to its executable path.")
    })?;
    host.worker_started(session.pid());
    let idle_policy = ManagedIdlePolicy::new(host.idle_timeout());
    Ok(Self {
      host,
      receipt,
      session,
      native_lease,
      pending: HashMap::new(),
      recent: ManagedMessageReceipts::default(),
      initial_prompt: prompt,
      initial_model,
      initial_model_verified: false,
      state: None,
      model_revision: 0,
      saw_settled: false,
      checking_idle: false,
      dialog_deadline: None,
      stop_reply: None,
      exit_requested: false,
      pending_outcome: None,
      awaiting_new_turn: false,
      stop_requested: false,
      worker_state,
      idle_policy,
      idle_probe: false,
      retirement: None,
      termination,
    })
  }

  async fn run(&mut self, commands: &mut mpsc::Receiver<Command>) -> Result<(), String> {
    if self.host.is_shutting_down() {
      return Ok(());
    }
    self.request_state(true).await?;
    let mut shutdown = self.host.shutdown()?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
      let stopping = *shutdown.borrow_and_update();
      if stopping || self.termination.is_cancelled() {
        self.begin_shutdown().await;
        return Ok(());
      }
      tokio::select! {
        _ = self.termination.cancelled() => {
          self.begin_shutdown().await;
          return Ok(());
        },
        _ = shutdown.changed() => {
          self.begin_shutdown().await;
          return Ok(());
        },
        frame = self.session.recv() => match frame {
          Some(Ok(value)) => {self.frame(value).await?; if self.exit_requested {return Ok(());}},
          Some(Err(_)) | None if self.session.exited_successfully() => return Ok(()),
          Some(Err(error)) => return Err(error),
          None => return Err("Pi RPC reader stopped".into()),
        },
        command = commands.recv() => {
          self.idle_policy.reset();
          self.idle_probe = false;
          match command {
          Some(Command::Model(operation, reply)) => {
            if self.termination.is_cancelled() { let _ = reply.send(Err(DeliveryError::not_sent("Pi runtime is stopping"))); }
            else { self.send(wire::Command::GetState, PendingKind::Model(ModelPending::new(operation, reply))).await?; }
          }
          Some(Command::Prompt {request_id, text, images, selection, reply}) => {
            if self.termination.is_cancelled() || self.stop_requested || self.host.is_shutting_down() {
              let _ = reply.send(Err(DeliveryError::not_sent("Managed Pi runtime is stopping")));
            } else {
              self.begin_message(MessagePending::new(request_id, text, images, selection, reply)).await?;
            }
          },
          Some(Command::Abort(reply)) => {
            self.stop_requested = true;
            self.awaiting_new_turn = false;
            self.initial_prompt = None;
            let input_checks: Vec<_> = self.pending.iter().filter_map(|(id, pending)| matches!(pending.kind, PendingKind::Message(_)).then(|| id.clone())).collect();
            for id in input_checks {
              if let Some(Pending { kind: PendingKind::Message(pending), .. }) = self.pending.remove(&id) {
                self.reject_message(pending, "Message was cancelled by Stop before submission", true);
              }
            }
            self.cancel_dialog("Pi dialog was cancelled by Stop").await?;
            self.update(|receipt| { receipt.status = ManagedSessionStatus::Stopping; receipt.message = Some("Stopping the current Pi turn".into()); }).await?;
            self.send(wire::Command::Abort, PendingKind::Abort(reply)).await?;
          }
          Some(Command::Dialog(response, reply)) => { let result = self.respond(response).await; let _ = reply.send(result); }
          None => return Ok(()),
        }},
        _ = tick.tick() => self.tick().await?,
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
        .session
        .send(uuid::Uuid::new_v4().to_string(), wire::Command::Abort)
        .await;
    })
    .await;
    self.session.close_input().await;
  }

  async fn frame(&mut self, value: Value) -> Result<(), String> {
    match value.get("type").and_then(Value::as_str).unwrap_or_default() {
      "response" => {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
          return Err("Pi RPC response has no correlation ID".into());
        };
        // PiAdapter validated native correlation and the response envelope.
        // This map holds only the supervisor's workflow continuation.
        if !self.pending.contains_key(id) {
          return Ok(());
        }
        let success = value["success"].as_bool().expect("adapter-validated response");
        let error = value
          .get("error")
          .and_then(Value::as_str)
          .unwrap_or("Pi rejected the request")
          .to_owned();
        let pending = self.pending.remove(id).expect("validated pending response");
        match pending.kind {
          PendingKind::InitialModels => {
            if !success {
              return Err(error);
            }
            let choices = pi_choices(
              value
                .get("data")
                .and_then(|data| data.get("models"))
                .ok_or("Pi model catalog is missing")?,
            )?;
            let selected = self
              .initial_model
              .as_ref()
              .ok_or("Pi creation model selection disappeared")?;
            let choice = choices
              .iter()
              .find(|choice| same_model(&choice.model, selected))
              .ok_or("The selected Pi model is no longer available; refresh the model list")?;
            if !choice.supports_images
              && self
                .initial_prompt
                .as_ref()
                .is_some_and(|(_, images)| !images.is_empty())
            {
              return Err("The selected Pi model does not support image input".into());
            }
            self.initial_model_verified = true;
            // Re-read after catalog discovery: extension startup or native
            // defaults may have changed the launch selection in the meantime.
            self.request_state(true).await?;
          }
          PendingKind::Model(pending) => self.model_response(pending, success, value.get("data"), &error).await?,
          PendingKind::Message(pending) => {
            self
              .message_response(pending, success, value.get("data"), &error)
              .await?
          }
          PendingKind::State { initial } => {
            if !success {
              return Err(error);
            }
            let state = parse_state(value.get("data").ok_or("Pi get_state response has no data")?)?;
            self.accept_state(state).await?;
            self.probe_file().await?;
            if initial {
              if let Some(selected) = &self.initial_model {
                if !self.initial_model_verified {
                  self
                    .send(wire::Command::GetAvailableModels, PendingKind::InitialModels)
                    .await?;
                  return Ok(());
                }
                if self
                  .state
                  .as_ref()
                  .and_then(|state| state.model.as_ref())
                  .is_none_or(|current| !same_model(current, selected))
                {
                  return Err("Pi did not activate the selected creation model; no prompt was sent".into());
                }
                if matches!(
                  self.receipt.status,
                  ManagedSessionStatus::Running | ManagedSessionStatus::Stopping
                ) || self.checking_idle
                  || self
                    .state
                    .as_ref()
                    .is_some_and(|state| state.is_streaming || state.is_compacting || state.pending_message_count > 0)
                {
                  return Err("Pi began work before the initial prompt; no prompt was sent".into());
                }
              }
              if let Some((prompt, images)) = self.initial_prompt.take() {
                self
                  .prompt(self.receipt.request_id.clone(), prompt, images, None, None)
                  .await?;
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
            model_result,
          } => {
            let initial = reply.is_none();
            if initial {
              self
                .update(|receipt| {
                  receipt.creation_input_state = Some(if success {
                    FirstInputState::Accepted
                  } else {
                    FirstInputState::Rejected
                  });
                })
                .await?;
            }
            let result = if success {
              Ok(if let Some(result) = model_result {
                Admission::WithModel {
                  disposition: if busy {
                    ModelInputDisposition::Steered
                  } else {
                    ModelInputDisposition::Started
                  },
                  result,
                }
              } else if busy {
                Admission::Steered
              } else {
                Admission::Started
              })
            } else {
              let failure = DeliveryError::not_sent(error.clone());
              Err(model_result.map_or(failure.clone(), |result| failure.with_model_result(result)))
            };
            self.recent.complete(&request_id, result.clone());
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

  async fn accept_state(&mut self, state: RpcState) -> Result<(), String> {
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
    if self.native_lease.is_none() {
      self.native_lease = Some(self.host.acquire_native_ownership(&state.session_id).await?);
    }
    if self.state.as_ref().is_none_or(|previous| previous.model != state.model) {
      self.model_revision = self.model_revision.saturating_add(1);
    }
    self.host.set_images_available(state.images_available);
    self.state = Some(state);
    Ok(())
  }

  async fn prompt(
    &mut self,
    request_id: String,
    text: String,
    images: Vec<ResolvedImage>,
    reply: Option<oneshot::Sender<Result<Admission, DeliveryError>>>,
    prepared: Option<PreparedMessage>,
  ) -> Result<(), String> {
    let model_result = prepared.as_ref().map(|prepared| prepared.result.clone());
    if self.termination.is_cancelled() || self.host.is_shutting_down() {
      if let Some(reply) = reply {
        let _ = reply.send(Err(with_model_result(
          DeliveryError::not_sent("Managed Pi runtime is shutting down"),
          model_result,
        )));
      }
      return Ok(());
    }
    if prepared.is_none() {
      let fingerprint = managed_message_fingerprint(&text, &images, None)?;
      match self.recent.admit(&request_id, fingerprint) {
        Ok(None) => {}
        Ok(Some(result)) => {
          if let Some(reply) = reply {
            let _ = reply.send(result);
          }
          return Ok(());
        }
        Err(error) => {
          if let Some(reply) = reply {
            let _ = reply.send(Err(error));
          }
          return Ok(());
        }
      }
    }
    if self.state.is_none() {
      if let Some(reply) = reply {
        let _ = reply.send(Err(with_model_result(
          DeliveryError::not_sent("Pi is still starting"),
          model_result,
        )));
      }
      return Ok(());
    }
    if !images.is_empty() && self.state.as_ref().is_none_or(|state| !state.images_available) {
      let message = "The active Pi model does not support image input. Choose an image-capable model and retry.";
      if let Some(reply) = reply {
        let _ = reply.send(Err(with_model_result(DeliveryError::not_sent(message), model_result)));
        return Ok(());
      }
      return Err(message.into());
    }
    let busy = prepared.as_ref().map(|prepared| prepared.busy).unwrap_or_else(|| {
      matches!(
        self.receipt.status,
        ManagedSessionStatus::Running | ManagedSessionStatus::Stopping
      ) || self
        .state
        .as_ref()
        .is_some_and(|state| state.is_streaming || state.is_compacting || state.pending_message_count > 0)
    });
    if !busy {
      self.awaiting_new_turn = true;
      self.stop_requested = false;
    }
    let mut command = wire::Command::Prompt {
      message: text,
      images: Vec::new(),
      streaming_behavior: (busy || prepared.is_some()).then_some(wire::StreamingBehavior::Steer),
    };
    if let wire::Command::Prompt {
      images: native_images, ..
    } = &mut command
    {
      use base64::Engine;
      *native_images = images
        .iter()
        .map(|image| wire::ImageContent {
          kind: "image".into(),
          data: base64::engine::general_purpose::STANDARD.encode(image.bytes.as_slice()),
          mime_type: image.mime_type.clone(),
        })
        .collect();
    }
    if reply.is_none() {
      // Persist before native I/O: a lost acknowledgement must never become
      // inferred acceptance merely because Pi created a history file.
      self
        .update(|receipt| receipt.creation_input_state = Some(FirstInputState::Uncertain))
        .await?;
    }
    self
      .send(
        command,
        PendingKind::Prompt {
          request_id,
          reply,
          busy,
          model_result,
        },
      )
      .await?;
    self.phase(ManagedSessionStatus::Running).await
  }

  async fn send(&mut self, command: wire::Command, kind: PendingKind) -> Result<(), String> {
    if self.pending.len() >= 32 {
      let error = "Pi RPC pending request limit reached";
      reject_before_send(kind, error);
      return Err(error.into());
    }
    let name = command.name();
    let id = uuid::Uuid::new_v4().to_string();
    let deadline = Instant::now()
      + if name == "prompt" {
        PROMPT_DEADLINE
      } else {
        RPC_DEADLINE
      };
    // Workflow ownership is recorded before the write, preserving uncertain
    // outcomes if only part of the native request reaches Pi.
    self.pending.insert(
      id.clone(),
      Pending {
        command: name,
        deadline,
        kind,
      },
    );
    if let Err(error) = self.session.send(id.clone(), command).await {
      if error.kind != tokn_interaction_core::ErrorKind::Uncertain {
        if let Some(pending) = self.pending.remove(&id) {
          reject_before_send(pending.kind, &error.message);
        }
      }
      return Err(error.to_string());
    }
    Ok(())
  }

  async fn write(&self, response: wire::ExtensionUiResponse) -> Result<(), String> {
    self.session.respond(response).await.map_err(|error| error.to_string())
  }

  async fn request_state(&mut self, initial: bool) -> Result<(), String> {
    // Catalog verification must retain its initial-state continuation. A
    // background tick must not reserve a plain state query in its place.
    if !initial
      && self
        .pending
        .values()
        .any(|pending| matches!(pending.kind, PendingKind::InitialModels))
    {
      return Ok(());
    }
    if self
      .pending
      .values()
      .any(|pending| matches!(pending.kind, PendingKind::State { .. }))
    {
      return Ok(());
    }
    self.send(wire::Command::GetState, PendingKind::State { initial }).await
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
      self.idle_policy.reset();
      self.idle_probe = false;
      return Ok(false);
    }
    // Preserve the deadline while checking native state once more. This also
    // catches work queued by an extension without an incoming viewer command.
    if !self.pending.is_empty() {
      return Ok(false);
    }
    if !self.session.is_empty() {
      return Ok(false);
    }
    if !self.idle_policy.observe(true) {
      return Ok(false);
    }
    if !self.idle_probe {
      self.idle_probe = true;
      self.request_state(false).await?;
      return Ok(false);
    }
    let Some(guard) = self.host.try_retirement_gate() else {
      return Ok(false);
    };
    // Send holds this same gate through queue admission. Closing an empty
    // receiver under it cannot strand a message that has already been admitted.
    if !commands.is_empty() || !self.session.is_empty() {
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
    let validated = self.host.validate_history(&path, &id, &folder).await?;
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
          self.write(cancel_response(&previous.dialog_id)).await?;
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
          self.write(cancel_response(&id)).await?;
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
    let native = wire::ExtensionDialog {
      dialog_id: dialog.dialog_id.clone(),
      kind: dialog.kind.clone(),
      title: dialog.title.clone(),
      message: dialog.message.clone(),
      options: dialog.options.clone(),
      initial_value: dialog.initial_value.clone(),
      timeout_ms: None,
    };
    let answer = native.response(
      (dialog.kind != "confirm").then_some(response.value).flatten(),
      (dialog.kind == "confirm").then_some(response.confirmed).flatten(),
      response.cancelled,
    )?;
    self.write(answer).await?;
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
      self.write(cancel_response(&dialog.dialog_id)).await?;
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
    self.host.update(receipt).await?;
    Ok(())
  }

  fn exit_message(&self, message: String) -> String {
    let stderr = self.session.stderr_tail();
    if stderr.trim().is_empty() {
      message
    } else {
      format!("{message}: {}", stderr.trim())
    }
  }
}

fn cancel_response(id: &str) -> wire::ExtensionUiResponse {
  wire::ExtensionUiResponse::ExtensionUiResponse {
    id: id.to_owned(),
    value: None,
    confirmed: None,
    cancelled: Some(true),
  }
}

/// These failures occur before a provider prompt can be written. Reporting
/// uncertainty would unnecessarily lock an editable draft after a failed check.
fn with_model_result(error: DeliveryError, result: Option<MessageModelResult>) -> DeliveryError {
  result.map_or(error.clone(), |result| error.with_model_result(result))
}

fn reject_before_send(kind: PendingKind, message: &str) {
  match kind {
    PendingKind::Model(pending) => pending.reject(message, false),
    PendingKind::Message(pending) => pending.reject(message, false),
    PendingKind::Prompt {
      reply: Some(reply),
      model_result,
      ..
    } => {
      let error = DeliveryError::not_sent(message);
      let error = model_result.map_or(error.clone(), |result| error.with_model_result(result));
      let _ = reply.send(Err(error));
    }
    PendingKind::Abort(reply) => {
      let _ = reply.send(Err(message.into()));
    }
    PendingKind::InitialModels | PendingKind::State { .. } | PendingKind::Prompt { reply: None, .. } => {}
  }
}

fn same_model(left: &tokn_interaction_core::ModelIdentity, right: &tokn_interaction_core::ModelIdentity) -> bool {
  left.model_provider == right.model_provider && left.model_id == right.model_id
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

#[cfg(test)]
mod delivery_preflight_tests {
  use super::*;

  #[test]
  fn rejected_image_checks_and_unwritten_prompts_are_definite_non_sends() {
    for check_only in [true, false] {
      let (reply, mut receive) = oneshot::channel();
      let pending = if check_only {
        PendingKind::Message(MessagePending::new(
          "request".into(),
          "caption".into(),
          Vec::new(),
          None,
          reply,
        ))
      } else {
        PendingKind::Prompt {
          request_id: "request".into(),
          reply: Some(reply),
          busy: false,
          model_result: None,
        }
      };
      reject_before_send(pending, "Pi RPC pending request limit reached");
      let error = receive.try_recv().unwrap().unwrap_err();
      assert!(!error.may_have_been_sent);
      assert!(error.message.contains("pending request limit"));
    }
  }
}
