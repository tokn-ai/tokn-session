use super::*;
use crate::{input::InputTarget, models::*};

impl ViewerService {
  pub async fn get_session_model_status(
    &self,
    request: SessionModelStatusRequest,
  ) -> Result<SessionModelStatus, String> {
    if self.session_scope.is_some() {
      self.validate_session_key(&request.session_key)?;
      return Ok(SessionModelStatus::unavailable(
        "Shared sessions do not expose live model controls",
      ));
    }
    if self.relay.status().settings.mode != crate::relay::RelayMode::External
      && let Some(result) = self.managed_pi.model_status(&request.session_key).await
    {
      return Ok(result.unwrap_or_else(SessionModelStatus::unavailable));
    }
    let target = match self.model_target(&request.session_key).await {
      Ok(target) => target,
      Err(reason) => return Ok(SessionModelStatus::unavailable(reason)),
    };
    let status = match target.provider {
      ViewerProvider::Codex => crate::input::codex::model_status(&target).await,
      ViewerProvider::Pi => crate::input::pi::model_status(&target).await,
      _ => unreachable!(),
    };
    Ok(status.unwrap_or_else(|error| SessionModelStatus::unavailable(error.message)))
  }

  pub async fn list_session_models(
    &self,
    request: ListSessionModelsRequest,
  ) -> Result<ListSessionModelsResponse, String> {
    if self.session_scope.is_some() {
      self.validate_session_key(&request.session_key)?;
      return Ok(ListSessionModelsResponse {
        status: SessionModelStatus::unavailable("Shared sessions do not expose model catalogs"),
        models: Vec::new(),
      });
    }
    if self.relay.status().settings.mode != crate::relay::RelayMode::External
      && let Some(result) = self.managed_pi.list_models(&request.session_key).await
    {
      return result;
    }
    let target = match self.model_target(&request.session_key).await {
      Ok(target) => target,
      Err(reason) => {
        return Ok(ListSessionModelsResponse {
          status: SessionModelStatus::unavailable(reason),
          models: Vec::new(),
        });
      }
    };
    match target.provider {
      ViewerProvider::Codex => {
        let status = crate::input::codex::model_status(&target)
          .await
          .unwrap_or_else(|error| SessionModelStatus::unavailable(error.message));
        Ok(ListSessionModelsResponse {
          status,
          models: Vec::new(),
        })
      }
      ViewerProvider::Pi => crate::input::pi::list_models(&target)
        .await
        .map_err(|error| error.message),
      _ => unreachable!(),
    }
  }

  async fn model_target(&self, session_key: &str) -> Result<InputTarget, String> {
    let service = self.clone();
    let key = session_key.to_owned();
    tokio::task::spawn_blocking(move || service.resolve_input_target(&key))
      .await
      .map_err(|_| "Could not resolve the model control target".to_string())?
  }
}
