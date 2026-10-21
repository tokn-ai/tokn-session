use super::*;
use crate::models::*;

pub(in crate::managed_pi) enum ModelOperation {
  Status,
  List,
  Select(SelectSessionModelRequest),
}

pub(in crate::managed_pi) enum ModelResult {
  Status(SessionModelStatus),
  List(ListSessionModelsResponse),
  Selected(SessionModelState),
}

#[derive(Clone, Copy)]
enum ModelPhase {
  Before,
  Catalog,
  Set,
  After,
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
    let error =
      if matches!(self.phase, ModelPhase::After) || (possibly_written && matches!(self.phase, ModelPhase::Set)) {
        DeliveryError::uncertain(message)
      } else {
        DeliveryError::not_sent(message)
      };
    let _ = self.reply.send(Err(error));
  }
}

impl Actor {
  fn current_model_status(&self) -> SessionModelStatus {
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
          PendingKind::Prompt { .. } | PendingKind::ImagePreflight { .. } | PendingKind::Abort(_)
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
      ModelPhase::Before | ModelPhase::After => {
        let state = match data
          .ok_or_else(|| "Pi returned no model state".to_string())
          .and_then(parse_state)
        {
          Ok(state) => state,
          Err(error) => {
            pending.reject(&error, true);
            return Ok(());
          }
        };
        if let Err(error) = self.accept_state(state) {
          pending.reject(&error, true);
          return Err(error);
        }
        let status = self.current_model_status();
        if matches!(pending.phase, ModelPhase::After) {
          let ModelOperation::Select(selection) = &pending.operation else {
            unreachable!()
          };
          if !status.state.model.as_ref().is_some_and(|model| {
            model.model_provider == selection.model_provider && model.model_id == selection.model_id
          }) {
            pending.reject(
              "Pi's model changed before selection could be confirmed; refresh its current model",
              true,
            );
          } else {
            let _ = pending.reply.send(Ok(ModelResult::Selected(status.state)));
          }
          return Ok(());
        }
        match &pending.operation {
          ModelOperation::Status => {
            let _ = pending.reply.send(Ok(ModelResult::Status(status)));
            return Ok(());
          }
          ModelOperation::Select(selection) => {
            if !status.can_select {
              pending.reject(status.reason.as_deref().unwrap_or("Pi is busy"), false);
              return Ok(());
            }
            if selection.runtime_generation != status.runtime_generation
              || selection.expected_revision != status.state.revision
            {
              pending.reject(
                "The Pi model or runtime changed; refresh before choosing a model",
                false,
              );
              return Ok(());
            }
          }
          ModelOperation::List => {}
        }
        pending.phase = ModelPhase::Catalog;
        self
          .send(
            "get_available_models",
            json!({"type":"get_available_models"}),
            PendingKind::Model(pending),
          )
          .await?;
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
        match &pending.operation {
          ModelOperation::List => {
            let _ = pending.reply.send(Ok(ModelResult::List(ListSessionModelsResponse {
              status: self.current_model_status(),
              models,
            })));
          }
          ModelOperation::Select(selection) => {
            if !self.current_model_status().can_select {
              pending.reject("Pi became busy before the model change", false);
              return Ok(());
            }
            if !models.iter().any(|choice| {
              choice.model.model_provider == selection.model_provider && choice.model.model_id == selection.model_id
            }) {
              pending.reject("The selected model is no longer available in this Pi runtime", false);
              return Ok(());
            }
            let request =
              json!({"type":"set_model", "provider":selection.model_provider, "modelId":selection.model_id});
            pending.phase = ModelPhase::Set;
            self.send("set_model", request, PendingKind::Model(pending)).await?;
          }
          ModelOperation::Status => unreachable!(),
        }
      }
      ModelPhase::Set => {
        pending.phase = ModelPhase::After;
        self
          .send("get_state", json!({"type":"get_state"}), PendingKind::Model(pending))
          .await?;
      }
    }
    Ok(())
  }
}
