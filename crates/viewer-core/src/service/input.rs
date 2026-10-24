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
    let target = match self.control_target(request.session_key).await {
      Ok(target) => target,
      Err(error) => return Ok(input_status_response(Err(input::DeliveryError::not_sent(error)), false)),
    };
    let resume_required = self
      .sessions
      .pi
      .input_status(&target.session_key)
      .is_some_and(|status| status.is_ok_and(|status| status.resume_required))
      || (target.input.provider != ViewerProvider::Pi
        && self.sessions.transport(&target).await? == crate::session_supervisor::SessionTransport::Rpc
        && self.sessions.runtimes.binding(&target.session_key).is_none());
    Ok(input_status_response(
      self.sessions.input_status(&target).await,
      resume_required,
    ))
  }

  pub async fn submit_session_input(
    &self,
    mut request: SubmitSessionInputRequest,
  ) -> Result<SubmitSessionInputResponse, String> {
    if let Err(message) = input::validate_request(&mut request) {
      return Ok(SubmitSessionInputResponse {
        model_result: None,
        request_id: request.request_id,
        status: InputDeliveryStatus::NotSent,
        message,
      });
    }
    self.check_managed_workspace()?;
    if let Ok(locator) = decode_session_key(&request.session_key) {
      let receipt_target = InputTarget {
        provider: locator.provider,
        session_id: locator.session_id,
        session_file: locator.source_path,
      };
      if let Some(receipt) = self.input_broker.receipt(&receipt_target, &request) {
        return Ok(receipt);
      }
    }
    let target = match self.control_target(request.session_key.clone()).await {
      Ok(target) => target,
      Err(message) => {
        return Ok(SubmitSessionInputResponse {
          model_result: None,
          request_id: request.request_id,
          status: InputDeliveryStatus::NotSent,
          message,
        });
      }
    };
    if let Some(receipt) = self.input_broker.receipt(&target.input, &request) {
      return Ok(receipt);
    }
    let images = match self
      .resolve_message_images(&target.session_key, &request.image_ids, target.input.provider)
      .await
    {
      Ok(images) => images,
      Err(message) => {
        return Ok(SubmitSessionInputResponse {
          model_result: None,
          request_id: request.request_id,
          status: InputDeliveryStatus::NotSent,
          message,
        });
      }
    };
    let expected_transport = self.sessions.transport(&target).await?;
    let broker_target = InputTarget {
      provider: target.input.provider,
      session_id: target.input.session_id.clone(),
      session_file: target.input.session_file.clone(),
    };
    let sessions = self.sessions.clone();
    let delivery = request.clone();
    Ok(
      self
        .input_broker
        .submit_custom(&broker_target, request, async move {
          sessions.submit(target, delivery, images, expected_transport).await
        })
        .await,
    )
  }

  async fn resolve_message_images(
    &self,
    session_key: &str,
    image_ids: &[String],
    provider: ViewerProvider,
  ) -> Result<Vec<crate::images::ResolvedImage>, String> {
    if image_ids.is_empty() {
      return Ok(Vec::new());
    }
    let store = self.images.clone();
    let target = crate::images::ImageTarget::Session {
      session_key: session_key.into(),
    };
    let ids = image_ids.to_vec();
    tokio::task::spawn_blocking(move || {
      let images = store.resolve(&target, &ids)?;
      // Codex reads these paths after IPC admission. Pi receives embedded
      // bytes owned by detached delivery work, so its staging can expire.
      if provider == ViewerProvider::Codex {
        store.retain(&ids)?;
      }
      Ok(images)
    })
    .await
    .map_err(|_| "Could not load image attachments".to_string())?
  }

  pub(super) fn resolve_input_target(&self, session_key: &str) -> Result<InputTarget, String> {
    if self.relay.status().settings.mode == crate::relay::RelayMode::External {
      return Err("Message input is unavailable through the external snapshot connection. Connect to that machine's viewer API instead.".into());
    }
    let locator = decode_session_key(session_key)?;
    if !matches!(
      locator.provider,
      ViewerProvider::Codex | ViewerProvider::Pi | ViewerProvider::Dsh
    ) {
      let reason = if locator.provider == ViewerProvider::OpenCode {
        "This provider has no compatible live history binding yet (OpenCode V1 history cannot use the V2 RPC runtime)."
      } else {
        "This provider has no compatible live history binding yet."
      };
      return Err(reason.into());
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

  #[test]
  fn input_status_preserves_image_support_independently_of_worker_resume() {
    for (images_available, resume_required) in [(true, true), (false, true), (true, false)] {
      let status = input_status_response(
        Ok(input::InputCapability {
          message: "Ready for a new message".into(),
          images_available,
        }),
        resume_required,
      );
      let wire = serde_json::to_value(status).unwrap();
      assert_eq!(wire["available"], true);
      assert_eq!(wire["images_available"], images_available);
      assert_eq!(
        wire["images_stageable"], true,
        "staging is independent of the active model"
      );
      if resume_required {
        assert_eq!(wire["resume_required"], true);
      } else {
        assert!(wire.get("resume_required").is_none());
      }
      assert!(!wire["image_mime_types"].as_array().unwrap().is_empty());
    }
    let unavailable = input_status_response(Err(input::DeliveryError::not_sent("Worker unavailable")), true);
    assert!(!unavailable.available);
    assert!(!unavailable.images_available);
    assert!(!unavailable.images_stageable);
    assert!(!unavailable.resume_required);
    assert_eq!(unavailable.message, "Worker unavailable");
  }

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
  async fn only_codex_paths_require_permanent_retention_before_delivery() {
    use crate::images::{ImageTarget, ReleaseImageUploadRequest, UploadImageChunkRequest};
    use base64::Engine;
    for provider in [ViewerProvider::Codex, ViewerProvider::Pi] {
      let root = tempfile::tempdir().unwrap();
      let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
      let target = ImageTarget::Session {
        session_key: "synthetic-session".into(),
      };
      let mut encoded = std::io::Cursor::new(Vec::new());
      image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
      let bytes = encoded.into_inner();
      let upload_id = uuid::Uuid::new_v4().to_string();
      let uploaded = service
        .images
        .upload(UploadImageChunkRequest {
          target: target.clone(),
          upload_id: upload_id.clone(),
          offset: 0,
          total_bytes: bytes.len(),
          mime_type: "image/png".into(),
          name: None,
          data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        })
        .unwrap();
      let ids = vec![uploaded.image_id.unwrap()];
      let resolved = service
        .resolve_message_images("synthetic-session", &ids, provider)
        .await
        .unwrap();
      service
        .images
        .release(ReleaseImageUploadRequest {
          target: target.clone(),
          upload_id,
        })
        .unwrap();
      assert_eq!(
        resolved[0].bytes.as_slice(),
        bytes.as_slice(),
        "Pi delivery retains bytes independently of its staged file"
      );
      assert_eq!(
        service.images.resolve(&target, &ids).is_ok(),
        provider == ViewerProvider::Codex
      );
      assert_eq!(resolved[0].path.exists(), provider == ViewerProvider::Codex);
    }
  }

  #[tokio::test]
  async fn invalid_input_is_not_sent_and_does_not_replace_an_uncertain_receipt() {
    let root = tempfile::tempdir().unwrap();
    let service = ViewerService::native(root.path().join("index.sqlite")).unwrap();
    let target = InputTarget {
      provider: ViewerProvider::Codex,
      session_id: "owned".into(),
      session_file: root.path().join("session.jsonl"),
    };
    let request = SubmitSessionInputRequest {
      model_selection: None,
      session_key: "owned-session".into(),
      request_id: uuid::Uuid::new_v4().to_string(),
      text: "caption".into(),
      image_ids: vec![uuid::Uuid::new_v4().to_string()],
    };
    service
      .input_broker
      .submit_custom(&target, request.clone(), async {
        Err(input::DeliveryError::uncertain("connection ended after writing"))
      })
      .await;
    let mut invalid = request.clone();
    invalid.image_ids = vec!["/client/path.png".into()];
    assert_eq!(
      service.submit_session_input(invalid).await.unwrap().status,
      InputDeliveryStatus::NotSent
    );
    assert_eq!(
      service.input_broker.receipt(&target, &request).unwrap().status,
      InputDeliveryStatus::Unknown
    );
    let result = service
      .input_broker
      .submit_custom(&target, request, async {
        panic!("uncertain receipt must never replay")
      })
      .await;
    assert_eq!(result.status, InputDeliveryStatus::Unknown);
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
        model_selection: None,
        session_key: "forged-key".into(),
        request_id: uuid::Uuid::new_v4().to_string(),
        text: "hello".into(),
        image_ids: Vec::new(),
      })
      .await
      .unwrap();
    assert_eq!(result.status, InputDeliveryStatus::NotSent);
  }
}

fn input_status_response(
  status: Result<input::InputCapability, input::DeliveryError>,
  resume_required: bool,
) -> SessionInputStatus {
  let (available, message, images_available) = match status {
    Ok(capability) => (true, capability.message, capability.images_available),
    Err(error) => (false, error.message, false),
  };
  SessionInputStatus {
    available,
    message,
    max_length: MAX_INPUT_LENGTH,
    resume_required: available && resume_required,
    images_available,
    images_stageable: available,
    max_images: crate::images::MAX_IMAGES,
    max_image_bytes: crate::images::MAX_IMAGE_BYTES,
    max_total_image_bytes: crate::images::MAX_TOTAL_IMAGE_BYTES,
    image_mime_types: crate::images::IMAGE_MIME_TYPES
      .iter()
      .map(|mime| (*mime).into())
      .collect(),
  }
}
