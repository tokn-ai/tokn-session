use super::*;
use crate::input::{self, InputTarget, MAX_INPUT_LENGTH};
use crate::model::{
  InputDeliveryStatus, SessionInputStatus, SessionInputStatusRequest, SubmitSessionInputRequest,
  SubmitSessionInputResponse,
};

impl ViewerService {
  pub async fn get_session_input_status(
    &self,
    request: SessionInputStatusRequest,
  ) -> Result<SessionInputStatus, String> {
    if self.session_scope.is_none()
      && self.relay.status().settings.mode != crate::relay::RelayMode::External
      && let Some(status) = self.managed_pi.input_status(&request.session_key)
    {
      return Ok(match status {
        Ok(message) => SessionInputStatus {
          available: true,
          message,
          max_length: MAX_INPUT_LENGTH,
        },
        Err(error) => SessionInputStatus {
          available: false,
          message: error.message,
          max_length: MAX_INPUT_LENGTH,
        },
      });
    }
    let service = self.clone();
    let target = tokio::task::spawn_blocking(move || service.resolve_input_target(&request.session_key))
      .await
      .map_err(|_| "Could not check session input")?;
    let status = match target {
      Ok(target) => input::status(&target).await.map_err(|error| error.message),
      Err(error) => Err(error),
    };
    Ok(match status {
      Ok(message) => SessionInputStatus {
        available: true,
        message,
        max_length: MAX_INPUT_LENGTH,
      },
      Err(message) => SessionInputStatus {
        available: false,
        message,
        max_length: MAX_INPUT_LENGTH,
      },
    })
  }

  pub async fn submit_session_input(
    &self,
    mut request: SubmitSessionInputRequest,
  ) -> Result<SubmitSessionInputResponse, String> {
    input::validate_request(&mut request)?;
    if self.session_scope.is_none()
      && self.relay.status().settings.mode != crate::relay::RelayMode::External
      && let Some(status) = self.managed_pi.input_status(&request.session_key)
    {
      if let Err(error) = status {
        return Ok(SubmitSessionInputResponse {
          request_id: request.request_id,
          status: InputDeliveryStatus::NotSent,
          message: error.message,
        });
      }
      let locator = decode_session_key(&request.session_key)?;
      let target = InputTarget {
        provider: locator.provider,
        session_id: locator.session_id,
        session_file: locator.source_path,
      };
      let manager = self.managed_pi.clone();
      let session_key = request.session_key.clone();
      let request_id = request.request_id.clone();
      let text = request.text.clone();
      return Ok(
        self
          .input_broker
          .submit_custom(&target, request, async move {
            manager.submit_owned_input(&session_key, &request_id, &text).await
          })
          .await,
      );
    }
    let service = self.clone();
    let key = request.session_key.clone();
    let target = tokio::task::spawn_blocking(move || service.resolve_input_target(&key))
      .await
      .map_err(|_| "Could not resolve the message target")?;
    let target = match target {
      Ok(target) => target,
      Err(message) => {
        return Ok(SubmitSessionInputResponse {
          request_id: request.request_id,
          status: InputDeliveryStatus::NotSent,
          message,
        });
      }
    };
    Ok(self.input_broker.submit(target, request).await)
  }

  pub(super) fn resolve_input_target(&self, session_key: &str) -> Result<InputTarget, String> {
    if self.relay.status().settings.mode == crate::relay::RelayMode::External {
      return Err("Message input is unavailable through the external snapshot connection. Connect to that machine's viewer API instead.".into());
    }
    let locator = decode_session_key(session_key)?;
    if !matches!(locator.provider, ViewerProvider::Codex | ViewerProvider::Pi) {
      return Err("Message input is not available for this provider yet.".into());
    }
    // Never trust a client-supplied path, provider, working directory, or
    // parent identity. Resolve the exact catalog key, then recheck its header.
    let catalog = self
      .indexed_session_inventory(locator.provider)?
      .ok_or("Session catalog is not ready")?;
    let catalog_header = catalog
      .headers
      .into_iter()
      .find(|header| locator_for_header(locator.provider, header) == locator)
      .ok_or("Session is not in this machine's catalog")?;
    let session_file = catalog_header.path;
    if !session_file.is_absolute() {
      return Err("The selected session does not have an absolute source path.".into());
    }
    let header = self
      .repository
      .session_header_at_path(locator.provider, &session_file)
      .map_err(|_| "The selected session file is unavailable.")?;
    if header.id != locator.session_id {
      return Err("The selected session file has changed. Refresh the session list.".into());
    }
    if locator.provider == ViewerProvider::Codex
      && (header.parent_session_id.as_deref().is_some_and(|id| !id.is_empty())
        || header
          .agent_path
          .as_deref()
          .is_some_and(|path| !path.is_empty() && path != "/root"))
    {
      return Err(
        "Codex subagents receive messages through their parent task. Select the parent task to send a message.".into(),
      );
    }
    Ok(InputTarget {
      provider: locator.provider,
      session_id: header.id,
      session_file,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct HeaderRepository(Mutex<SessionHeader>);

  impl ViewerRepository for HeaderRepository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      Ok(if provider == ViewerProvider::Codex {
        vec![self.0.lock().unwrap().clone()]
      } else {
        vec![]
      })
    }

    fn session_header_at_path(&self, _provider: ViewerProvider, _path: &Path) -> Result<SessionHeader, String> {
      Ok(self.0.lock().unwrap().clone())
    }

    fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
      panic!("input should inspect the header, not parse the full conversation")
    }
  }

  #[test]
  fn resolves_only_cataloged_sessions_and_rechecks_root_identity() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    std::fs::write(&path, "fixture").unwrap();
    let header: SessionHeader = serde_json::from_value(json!({"id":"session-one", "path":path})).unwrap();
    let locator = locator_for_header(ViewerProvider::Codex, &header);
    let key = encode_session_key(&locator).unwrap();
    let repository = Arc::new(HeaderRepository(Mutex::new(header)));
    let service = ViewerService::new(repository.clone());
    assert!(service.resolve_input_target(&key).is_err());
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    let target = service.resolve_input_target(&key).unwrap();
    assert_eq!(target.session_id, "session-one");
    assert_eq!(target.session_file, path);

    repository.0.lock().unwrap().parent_session_id = Some("parent".into());
    assert!(service.resolve_input_target(&key).unwrap_err().contains("parent task"));
    repository.0.lock().unwrap().parent_session_id = None;
    repository.0.lock().unwrap().agent_path = Some("/root/child".into());
    assert!(service.resolve_input_target(&key).unwrap_err().contains("parent task"));
    repository.0.lock().unwrap().agent_path = None;
    repository.0.lock().unwrap().id = "replaced".into();
    assert!(service.resolve_input_target(&key).unwrap_err().contains("has changed"));
  }

  #[tokio::test]
  async fn unsupported_and_forged_targets_are_unavailable_without_delivery() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let status = service
      .get_session_input_status(SessionInputStatusRequest {
        session_key: "forged-key".into(),
      })
      .await
      .unwrap();
    assert!(!status.available);
    let result = service
      .submit_session_input(SubmitSessionInputRequest {
        session_key: "forged-key".into(),
        request_id: uuid::Uuid::new_v4().to_string(),
        text: "hello".into(),
      })
      .await
      .unwrap();
    assert_eq!(result.status, InputDeliveryStatus::NotSent);
  }
}
