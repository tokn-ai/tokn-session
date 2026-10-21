use super::*;
use crate::models::*;

pub(in crate::managed_pi) enum ModelOperation {
  Status,
  List,
}

pub(in crate::managed_pi) enum ModelResult {
  Status(SessionModelStatus),
  List(ListSessionModelsResponse),
}

#[derive(Clone, Copy)]
enum ModelPhase {
  Before,
  Catalog,
}

pub(super) struct ModelPending {
  operation: ModelOperation,
  phase: ModelPhase,
  reply: oneshot::Sender<Result<ModelResult, DeliveryError>>,
}

impl ModelPending {
  pub(super) fn new(operation: ModelOperation, reply: oneshot::Sender<Result<ModelResult, DeliveryError>>) -> Self {
    Self {
      operation,
      phase: ModelPhase::Before,
      reply,
    }
  }

  pub(super) fn reject(self, message: &str, possibly_written: bool) {
    let _ = possibly_written;
    let error = DeliveryError::not_sent(message);
    let _ = self.reply.send(Err(error));
  }
}

impl Actor {
  pub(super) fn current_model_status(&self) -> SessionModelStatus {
    let generation = self.inner.upgrade().and_then(|inner| {
      inner
        .records
        .lock()
        .unwrap()
        .get(&self.receipt.request_id)
        .filter(|record| record.generation == self.generation)
        .map(|record| record.instance_id.clone())
    });
    let idle = self.receipt.status == ManagedSessionStatus::Idle
      && self
        .state
        .as_ref()
        .is_some_and(|state| !state.is_streaming && !state.is_compacting && state.pending_message_count == 0)
      && self.receipt.dialog.is_none()
      && !self.awaiting_new_turn
      && !self.checking_idle
      && !self.stop_requested
      && !self.pending.values().any(|pending| {
        matches!(
          pending.kind,
          PendingKind::Prompt { .. } | PendingKind::Message(_) | PendingKind::Abort(_)
        )
      });
    let can_select = idle && generation.is_some() && !self.termination.is_cancelled();
    SessionModelStatus {
      state: SessionModelState::live(
        self.state.as_ref().and_then(|state| state.model.clone()),
        &format!("{}:{}", generation.as_deref().unwrap_or("stopped"), self.model_revision),
      ),
      runtime_generation: generation,
      can_list: true,
      supports_model_input: true,
      can_select,
      updates_pi_default: true,
      reason: (!can_select)
        .then(|| "Wait for Pi, queued messages, and dialogs to finish before changing models".into()),
    }
  }

  pub(super) async fn model_response(
    &mut self,
    mut pending: ModelPending,
    success: bool,
    data: Option<&Value>,
    error: &str,
  ) -> Result<(), String> {
    if !success {
      pending.reject(error, true);
      return Ok(());
    }
    match pending.phase {
      ModelPhase::Before => {
        let state = match data
          .ok_or_else(|| "Pi returned no model state".to_string())
          .and_then(parse_state)
        {
          Ok(state) => state,
          Err(error) => {
            pending.reject(&error, false);
            return Ok(());
          }
        };
        if let Err(error) = self.accept_state(state) {
          pending.reject(&error, false);
          return Err(error);
        }
        match pending.operation {
          ModelOperation::Status => {
            let _ = pending.reply.send(Ok(ModelResult::Status(self.current_model_status())));
          }
          ModelOperation::List => {
            pending.phase = ModelPhase::Catalog;
            self
              .send(
                "get_available_models",
                json!({"type":"get_available_models"}),
                PendingKind::Model(pending),
              )
              .await?;
          }
        }
      }
      ModelPhase::Catalog => {
        let models = match data
          .and_then(|data| data.get("models"))
          .ok_or_else(|| "Pi returned no model catalog".to_string())
          .and_then(pi_choices)
        {
          Ok(models) => models,
          Err(error) => {
            pending.reject(&error, false);
            return Ok(());
          }
        };
        let _ = pending.reply.send(Ok(ModelResult::List(ListSessionModelsResponse {
          status: self.current_model_status(),
          models,
        })));
      }
    }
    Ok(())
  }
}
