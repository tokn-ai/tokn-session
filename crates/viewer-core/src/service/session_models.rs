use super::*;
use crate::{models::*, session_supervisor::SessionTransport};

impl ViewerService {
  pub async fn get_session_model_status(
    &self,
    request: SessionModelStatusRequest,
  ) -> Result<SessionModelStatus, String> {
    let target = match self.control_target(request.session_key).await {
      Ok(target) => target,
      Err(reason) => return Ok(SessionModelStatus::unavailable(reason)),
    };
    if let Some(result) = self.sessions.managed.model_status(&target.session_key).await {
      return Ok(result.unwrap_or_else(SessionModelStatus::unavailable));
    }
    let result = match (target.input.provider, self.sessions.transport(&target).await?) {
      (ViewerProvider::Codex, SessionTransport::Desktop) => crate::input::codex::model_status(&target.input)
        .await
        .map_err(|error| error.message),
      (ViewerProvider::Pi, _) => crate::input::pi::model_status(&target.input)
        .await
        .map_err(|error| error.message),
      _ => self
        .sessions
        .rpc_models(&target, false)
        .await
        .map(|response| response.status),
    };
    Ok(result.unwrap_or_else(SessionModelStatus::unavailable))
  }

  pub async fn list_session_models(
    &self,
    request: ListSessionModelsRequest,
  ) -> Result<ListSessionModelsResponse, String> {
    let target = match self.control_target(request.session_key).await {
      Ok(target) => target,
      Err(reason) => {
        return Ok(ListSessionModelsResponse {
          status: SessionModelStatus::unavailable(reason),
          models: Vec::new(),
        });
      }
    };
    if let Some(result) = self.sessions.managed.list_models(&target.session_key).await {
      return result;
    }
    match (target.input.provider, self.sessions.transport(&target).await?) {
      (ViewerProvider::Codex, SessionTransport::Desktop) => Ok(ListSessionModelsResponse {
        status: crate::input::codex::model_status(&target.input)
          .await
          .unwrap_or_else(|error| SessionModelStatus::unavailable(error.message)),
        models: Vec::new(),
      }),
      (ViewerProvider::Pi, _) => crate::input::pi::list_models(&target.input)
        .await
        .map_err(|error| error.message),
      _ => self.sessions.rpc_models(&target, true).await,
    }
  }
}
