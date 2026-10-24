//! One fresh native admission transaction for input and optional model intent.
use super::*;

#[derive(Clone, Copy)]
enum MessagePhase {
  State,
  Catalog,
  BusyState,
  Setting,
  Readback,
}

pub(super) struct PreparedMessage {
  pub(super) result: MessageModelResult,
  pub(super) busy: bool,
}

pub(super) struct MessagePending {
  pub(super) request_id: String,
  text: String,
  images: Vec<ResolvedImage>,
  selection: Option<MessageModelSelection>,
  phase: MessagePhase,
  result: Option<MessageModelResult>,
  reply: oneshot::Sender<Result<Admission, DeliveryError>>,
}

impl MessagePending {
  pub(super) fn new(
    request_id: String,
    text: String,
    images: Vec<ResolvedImage>,
    selection: Option<MessageModelSelection>,
    reply: oneshot::Sender<Result<Admission, DeliveryError>>,
  ) -> Self {
    Self {
      request_id,
      text,
      images,
      selection,
      phase: MessagePhase::State,
      result: None,
      reply,
    }
  }

  fn fingerprint(&self) -> Result<String, String> {
    managed_message_fingerprint(&self.text, &self.images, self.selection.as_ref())
  }

  fn error(&self, message: &str, possibly_written: bool) -> DeliveryError {
    let uncertain = (matches!(self.phase, MessagePhase::Readback) && self.result.is_none())
      || (possibly_written && matches!(self.phase, MessagePhase::Setting));
    let error = if uncertain {
      DeliveryError::uncertain(message)
    } else {
      DeliveryError::not_sent(message)
    };
    self
      .result
      .clone()
      .map_or(error.clone(), |result| error.with_model_result(result))
  }

  pub(super) fn reject(self, message: &str, possibly_written: bool) {
    let error = self.error(message, possibly_written);
    let _ = self.reply.send(Err(error));
  }
}

impl Actor {
  pub(super) async fn begin_message(&mut self, pending: MessagePending) -> Result<(), String> {
    let fingerprint = pending.fingerprint()?;
    match self.recent.admit(&pending.request_id, fingerprint) {
      Ok(None) => {}
      Ok(Some(result)) => {
        let _ = pending.reply.send(result);
        return Ok(());
      }
      Err(error) => {
        let _ = pending.reply.send(Err(error));
        return Ok(());
      }
    }
    self.send(wire::Command::GetState, PendingKind::Message(pending)).await
  }

  fn observed_busy(&self) -> bool {
    self.receipt.status == ManagedSessionStatus::Running
      || self
        .state
        .as_ref()
        .is_some_and(|state| state.is_streaming || state.is_compacting || state.pending_message_count > 0)
      || self.receipt.dialog.is_some()
      || self.awaiting_new_turn
      || self.checking_idle
  }

  pub(super) fn reject_message(&mut self, pending: MessagePending, message: &str, possibly_written: bool) {
    let error = pending.error(message, possibly_written);
    self.reject_message_error(pending, error);
  }

  fn reject_message_error(&mut self, pending: MessagePending, error: DeliveryError) {
    self.recent.complete(&pending.request_id, Err(error.clone()));
    let _ = pending.reply.send(Err(error));
  }

  fn message_result(&self, outcome: MessageModelOutcome) -> MessageModelResult {
    MessageModelResult {
      outcome,
      state: self.current_model_status().state,
    }
  }

  async fn finish_message(&mut self, mut pending: MessagePending) -> Result<(), String> {
    if self.termination.is_cancelled() || self.stop_requested {
      self.reject_message(pending, "Message was cancelled before submission", false);
      return Ok(());
    }
    if !pending.images.is_empty() && self.state.as_ref().is_none_or(|state| !state.images_available) {
      self.reject_message(pending, "The effective Pi model does not support image input", false);
      return Ok(());
    }
    let busy = self.observed_busy();
    let result = pending
      .result
      .take()
      .unwrap_or_else(|| self.message_result(MessageModelOutcome::Unchanged));
    self
      .prompt(
        pending.request_id,
        pending.text,
        pending.images,
        Some(pending.reply),
        Some(PreparedMessage { result, busy }),
      )
      .await
  }

  pub(super) async fn message_response(
    &mut self,
    mut pending: MessagePending,
    success: bool,
    data: Option<&Value>,
    error: &str,
  ) -> Result<(), String> {
    if !success {
      self.reject_message(pending, error, true);
      return Ok(());
    }
    if self.stop_requested || self.termination.is_cancelled() {
      self.reject_message(pending, "Message was cancelled before submission", true);
      return Ok(());
    }
    match pending.phase {
      MessagePhase::State | MessagePhase::BusyState | MessagePhase::Readback => {
        let state = match data
          .ok_or_else(|| "Pi returned no fresh state".to_string())
          .and_then(parse_state)
        {
          Ok(state) => state,
          Err(error) => {
            self.reject_message(pending, &error, true);
            return Err(error);
          }
        };
        if let Err(error) = self.accept_state(state).await {
          self.reject_message(pending, &error, true);
          return Err(error);
        }
        if matches!(pending.phase, MessagePhase::Readback) {
          let selection = pending.selection.as_ref().expect("model mutation has intent");
          if !self
            .state
            .as_ref()
            .and_then(|state| state.model.as_ref())
            .is_some_and(|model| {
              model.model_id == selection.model_id && model.model_provider == selection.model_provider
            })
          {
            self.reject_message(
              pending,
              "Pi did not confirm the selected model; refresh before retrying",
              true,
            );
            return Ok(());
          }
          pending.result = Some(self.message_result(MessageModelOutcome::Applied));
          if self.observed_busy() {
            let error = DeliveryError::uncertain(
              "Pi started work while applying the model; refresh its state before resending the message",
            )
            .with_model_result(pending.result.clone().expect("model readback was confirmed"));
            self.reject_message_error(pending, error);
            return Ok(());
          }
          return self.finish_message(pending).await;
        }
        if matches!(pending.phase, MessagePhase::BusyState) || self.observed_busy() {
          pending.result = Some(self.message_result(if pending.selection.is_some() {
            MessageModelOutcome::IgnoredWhileRunning
          } else {
            MessageModelOutcome::Unchanged
          }));
          return self.finish_message(pending).await;
        }
        let Some(selection) = &pending.selection else {
          pending.result = Some(self.message_result(MessageModelOutcome::Unchanged));
          return self.finish_message(pending).await;
        };
        if let Err(error) = validate_message_selection(selection) {
          self.reject_message(pending, &error, false);
          return Ok(());
        }
        let status = self.current_model_status();
        if !status.can_select
          || selection.runtime_generation != status.runtime_generation
          || selection.expected_revision != status.state.revision
        {
          self.reject_message(
            pending,
            "The Pi model or runtime changed; refresh the draft model before sending",
            false,
          );
          return Ok(());
        }
        pending.phase = MessagePhase::Catalog;
        self
          .send(wire::Command::GetAvailableModels, PendingKind::Message(pending))
          .await?;
      }
      MessagePhase::Catalog => {
        // A native work event observed since preflight always wins over draft
        // model intent. Confirm the effective model and steer without a setter.
        if self.observed_busy() {
          pending.phase = MessagePhase::BusyState;
          self
            .send(wire::Command::GetState, PendingKind::Message(pending))
            .await?;
          return Ok(());
        }
        let models = match data
          .and_then(|data| data.get("models"))
          .ok_or_else(|| "Pi returned no model catalog".to_string())
          .and_then(pi_choices)
        {
          Ok(models) => models,
          Err(error) => {
            self.reject_message(pending, &error, false);
            return Ok(());
          }
        };
        let selection = pending.selection.as_ref().expect("catalog admission has model intent");
        let status = self.current_model_status();
        if !status.can_select
          || selection.runtime_generation != status.runtime_generation
          || selection.expected_revision != status.state.revision
        {
          self.reject_message(
            pending,
            "The Pi model or runtime changed; refresh the draft model before sending",
            false,
          );
          return Ok(());
        }
        let Some(choice) = models.iter().find(|choice| {
          choice.model.model_id == selection.model_id && choice.model.model_provider == selection.model_provider
        }) else {
          self.reject_message(
            pending,
            "The selected model is no longer available in this Pi runtime",
            false,
          );
          return Ok(());
        };
        if !pending.images.is_empty() && !choice.supports_images {
          self.reject_message(pending, "The selected Pi model does not support image input", false);
          return Ok(());
        }
        if status
          .state
          .model
          .as_ref()
          .is_some_and(|model| model.model_id == selection.model_id && model.model_provider == selection.model_provider)
        {
          pending.result = Some(self.message_result(MessageModelOutcome::Unchanged));
          return self.finish_message(pending).await;
        }
        let command = wire::Command::SetModel {
          provider: selection
            .model_provider
            .clone()
            .expect("validated Pi model selection has a provider"),
          model_id: selection.model_id.clone(),
        };
        pending.phase = MessagePhase::Setting;
        self.send(command, PendingKind::Message(pending)).await?;
      }
      MessagePhase::Setting => {
        pending.phase = MessagePhase::Readback;
        self
          .send(wire::Command::GetState, PendingKind::Message(pending))
          .await?;
      }
    }
    Ok(())
  }
}
