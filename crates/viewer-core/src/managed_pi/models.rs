use super::actor::{ModelOperation, ModelResult};
use super::*;
use crate::models::*;

impl ManagedPiSessions {
  pub(crate) async fn model_status(&self, session_key: &str) -> Option<Result<SessionModelStatus, String>> {
    self.model_owner(session_key)?;
    Some(match self.model_command(session_key, ModelOperation::Status).await {
      Ok(ModelResult::Status(status)) => Ok(status),
      Ok(_) => Err("Pi returned the wrong model response".into()),
      Err(error) => {
        let mut status = SessionModelStatus::unavailable(error.message);
        status.updates_pi_default = true;
        status.supports_model_input = true;
        Ok(status)
      }
    })
  }

  pub(crate) async fn list_models(&self, session_key: &str) -> Option<Result<ListSessionModelsResponse, String>> {
    self.model_owner(session_key)?;
    Some(match self.model_command(session_key, ModelOperation::List).await {
      Ok(ModelResult::List(response)) => Ok(response),
      Ok(_) => Err("Pi returned the wrong model response".into()),
      Err(error) => {
        let mut status = SessionModelStatus::unavailable(error.message);
        status.updates_pi_default = true;
        status.supports_model_input = true;
        Ok(ListSessionModelsResponse {
          status,
          models: Vec::new(),
        })
      }
    })
  }

  fn model_owner(&self, session_key: &str) -> Option<(String, Arc<AsyncMutex<()>>)> {
    self
      .inner
      .records
      .lock()
      .unwrap()
      .iter()
      .find(|(_, record)| owns_key(&record.receipt, session_key))
      .map(|(id, record)| (id.clone(), record.start_gate.clone()))
  }

  async fn model_command(&self, session_key: &str, operation: ModelOperation) -> Result<ModelResult, DeliveryError> {
    self.available().map_err(DeliveryError::not_sent)?;
    let (id, gate) = self
      .model_owner(session_key)
      .ok_or_else(|| DeliveryError::not_sent("This session is not owned by the viewer"))?;
    // Model popups never start a worker. Share the existing startup/admission
    // gate so selection cannot race explicit restart or idle retirement.
    let _guard = self.admission_gate(&id, gate).await.map_err(DeliveryError::not_sent)?;
    let (sender, state) = {
      let records = self.inner.records.lock().unwrap();
      let record = records
        .get(&id)
        .ok_or_else(|| DeliveryError::not_sent("Managed session disappeared"))?;
      (record.sender.clone(), record.worker_state.clone())
    };
    let sender =
      sender.ok_or_else(|| DeliveryError::not_sent("Resume this viewer-owned Pi session before choosing a model"))?;
    if !state.is_some_and(|state| matches!(*state.borrow(), WorkerState::Ready)) {
      return Err(DeliveryError::not_sent(
        "Pi is starting or stopping; wait until its runtime is ready",
      ));
    }
    let (reply, receive) = oneshot::channel();
    tokio::time::timeout(QUEUE_DEADLINE, sender.send(actor::Command::Model(operation, reply)))
      .await
      .map_err(|_| DeliveryError::not_sent("Pi command queue is busy"))?
      .map_err(|_| DeliveryError::not_sent("Pi runtime has stopped"))?;
    receive.await.map_err(|_| {
      DeliveryError::uncertain("Pi stopped before confirming the model request; refresh its current model")
    })?
  }
}
