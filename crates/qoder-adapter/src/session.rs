use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::time::Duration;

const MAX_SESSIONS: usize = 128;
const TURN_TIMEOUT: Duration = Duration::from_secs(60 * 60);

impl Inner {
  pub(super) fn observe_model_update(&self, id: &str, update: &Value) -> Result<(), InteractionError> {
    if update["sessionUpdate"] != "config_option_update" {
      return Ok(());
    }
    let options: Vec<acp::SessionConfigOption> = decode(update["configOptions"].clone(), false)?;
    let Some(catalog) = models::from_config(Some(&options), None)? else {
      return Ok(());
    };
    if let Some(session) = self.sessions.lock().unwrap().get_mut(id) {
      apply_catalog(session, catalog);
    }
    self.publish_state(id);
    Ok(())
  }

  pub(super) async fn bind(
    &self,
    native_id: Option<&str>,
    model: Option<ModelIdentity>,
  ) -> Result<CreatedSession, InteractionError> {
    let _binding = self.binding.lock().await;
    if !self.transport.is_live() {
      return Err(InteractionError::unavailable("Qoder runtime is disconnected"));
    }
    if let Some(id) = native_id {
      if self.sessions.lock().unwrap().contains_key(id) {
        return Ok(CreatedSession {
          target: self.target(id),
          working_directory: self.directory.clone(),
        });
      }
    }
    if self.sessions.lock().unwrap().len() >= MAX_SESSIONS {
      return Err(InteractionError::unavailable("Qoder resident session limit reached"));
    }
    let method = if native_id.is_none() {
      "session/new"
    } else {
      let native = self.native.lock().unwrap();
      if native.session_capabilities.resume.is_some() {
        "session/resume"
      } else if native.load_session {
        "session/load"
      } else {
        return Err(InteractionError::unsupported("Qoder did not negotiate session resume"));
      }
    };
    let mut params = json!({"cwd":self.directory,"mcpServers":[]});
    if let Some(id) = native_id {
      params["sessionId"] = json!(id);
    }
    let result: qoder::SessionResult = decode(self.transport.call(method, params, true).await?, true)?;
    let id = match native_id {
      Some(id) if result.standard.session_id.as_ref().is_none_or(|actual| actual == id) => id.to_owned(),
      None => result
        .standard
        .session_id
        .clone()
        .ok_or_else(|| InteractionError::uncertain("Qoder created a session without confirming its identity"))?,
      _ => {
        return Err(InteractionError::uncertain(
          "Qoder resumed a different session identity",
        ));
      }
    };
    validate_id(&id).map_err(|_| InteractionError::uncertain("Qoder returned an invalid session identity"))?;
    let catalog = models::from_config(result.standard.config_options.as_deref(), None)
      .and_then(|catalog| match catalog {
        Some(catalog) => Ok(catalog),
        None => result.legacy_catalog().map(Option::unwrap_or_default),
      })
      .map_err(|_| {
        InteractionError::uncertain("Qoder created or resumed a session but returned an invalid model catalog")
      })?;
    let mut session = make_session(&id, catalog);
    if let Some(model) = model {
      if model.model_provider.is_some()
        || !session
          .models
          .iter()
          .any(|choice| choice.model.model_id == model.model_id)
      {
        return Err(InteractionError::uncertain(
          "Qoder created a session but the requested model is not available",
        ));
      }
      match &session.model_route {
        models::Route::Config { config_id } => {
          let value = self
            .transport
            .call(
              "session/set_config_option",
              json!({"sessionId":id,"configId":config_id,"value":model.model_id}),
              true,
            )
            .await
            .map_err(|_| {
              InteractionError::uncertain("Qoder created a session but did not confirm its requested model")
            })?;
          let result: acp::SetSessionConfigOptionResult = decode(value, true)?;
          let catalog = models::from_config(Some(&result.config_options), Some(config_id))
            .map_err(|_| InteractionError::uncertain("Qoder returned an invalid model selection state"))?
            .filter(|catalog| {
              catalog
                .current
                .as_ref()
                .is_some_and(|current| current.model_id == model.model_id)
            })
            .ok_or_else(|| InteractionError::uncertain("Qoder did not confirm its requested model identity"))?;
          apply_catalog(&mut session, catalog);
        }
        models::Route::QoderLegacy => {
          qoder::set_legacy_model(&self.transport, &id, &model.model_id)
            .await
            .map_err(|_| {
              InteractionError::uncertain("Qoder created a session but did not confirm its requested model")
            })?;
          session.state.model = session
            .models
            .iter()
            .find(|choice| choice.model.model_id == model.model_id)
            .map(|choice| choice.model.clone());
        }
        models::Route::Unavailable => {
          return Err(InteractionError::uncertain(
            "Qoder created a session but model selection is unavailable",
          ));
        }
      }
    }
    self.sessions.lock().unwrap().insert(id.clone(), session);
    self.publish_state(&id);
    Ok(CreatedSession {
      target: self.target(&id),
      working_directory: self.directory.clone(),
    })
  }

  pub(super) async fn message(
    self: &Arc<Self>,
    id: &str,
    input: MessageInput,
    delivery: DeliveryMode,
    request_id: String,
  ) -> Result<Reply, InteractionError> {
    if delivery != DeliveryMode::Prompt {
      return Err(InteractionError::unsupported(
        "Qoder ACP supports normal Prompt only; strict Start, steering and queues are unavailable",
      ));
    }
    if input.model.is_some() || input.expected_revision.is_some() {
      return Err(InteractionError::unsupported(
        "Qoder ACP cannot atomically apply model intent or a revision fence on a prompt",
      ));
    }
    let prompt = prompt_content(input, self.descriptor.lock().unwrap().capabilities.image_input)?;
    let dispatch = self.dispatch.clone().lock_owned().await;
    let (admission, mut receiver) = watch::channel(None);
    let token = request_id;
    {
      let mut sessions = self.sessions.lock().unwrap();
      let session = sessions.get_mut(id).expect("validated bound session");
      if session.prompt.is_some() || session.state.status != ExecutionState::Idle {
        return Err(InteractionError::new(
          ErrorKind::Conflict,
          "Qoder session is busy or its prior prompt outcome is unknown",
        ));
      }
      session.prompt = Some(Prompt {
        token: token.clone(),
        admission,
      });
      session.state.status = ExecutionState::Running;
    }
    self.publish_state(id);
    let inner = self.clone();
    let id = id.to_owned();
    tokio::spawn(async move {
      let result = inner
        .transport
        .call_with_dispatch(
          "session/prompt",
          json!({"sessionId":id,"prompt":prompt}),
          true,
          TURN_TIMEOUT,
          Some(dispatch),
        )
        .await
        .and_then(|value| decode::<qoder::PromptResult>(value, true));
      inner.finish_prompt(&id, &token, result);
    });
    let native_id = loop {
      if let Some(result) = receiver.borrow_and_update().clone() {
        break result?;
      }
      receiver
        .changed()
        .await
        .map_err(|_| InteractionError::uncertain("Qoder prompt result became unavailable"))?;
    };
    Ok(Reply::Admission(InputReceipt {
      disposition: InputDisposition::Accepted,
      model: None,
      model_ignored: false,
      native_id,
    }))
  }

  fn finish_prompt(&self, id: &str, token: &str, result: Result<qoder::PromptResult, InteractionError>) {
    let result = result.and_then(|result| {
      if result.native_message_id().is_some_and(|id| validate_id(id).is_err()) {
        return Err(InteractionError::uncertain(
          "Qoder returned an invalid native message identity",
        ));
      }
      if matches!(
        result.standard.stop_reason.as_str(),
        "end_turn" | "max_tokens" | "max_turn_requests" | "refusal" | "cancelled"
      ) {
        Ok(result)
      } else {
        Err(InteractionError::uncertain(
          "Qoder returned an unknown prompt termination reason",
        ))
      }
    });
    let mut sessions = self.sessions.lock().unwrap();
    let Some(session) = sessions.get_mut(id) else {
      return;
    };
    if session.prompt.as_ref().is_none_or(|prompt| prompt.token != token) {
      return;
    }
    let prompt = session.prompt.take().unwrap();
    session.state.status = match &result {
      Ok(_) => ExecutionState::Idle,
      Err(error)
        if matches!(
          error.kind,
          ErrorKind::InvalidRequest | ErrorKind::Rejected | ErrorKind::Unsupported
        ) =>
      {
        ExecutionState::Idle
      }
      Err(_) => ExecutionState::Unknown,
    };
    prompt.admission.send_if_modified(|admitted| {
      if admitted.is_some() {
        false
      } else {
        *admitted = Some(
          result
            .as_ref()
            .map(|result| result.native_message_id().map(str::to_owned))
            .map_err(Clone::clone),
        );
        true
      }
    });
    drop(sessions);
    self.retire_permissions(id, None);
    self.publish(
      Some(id.into()),
      RuntimeEventKind::Native(match result {
        Ok(result) => json!({"type":"qoder.prompt_result","prompt_id":token,"stop_reason":result.standard.stop_reason}),
        Err(error) => json!({"type":"qoder.prompt_result","prompt_id":token,"error_kind":error.kind}),
      }),
    );
    self.publish_state(id);
  }

  pub(super) async fn cancel(&self, id: &str) -> Result<Reply, InteractionError> {
    let _dispatch = self.dispatch.lock().await;
    let token = {
      let mut sessions = self.sessions.lock().unwrap();
      let session = sessions.get_mut(id).expect("validated bound session");
      let token = session.prompt.as_ref().map(|prompt| prompt.token.clone());
      if token.is_some() {
        session.state.status = ExecutionState::Stopping;
      }
      token
    };
    let Some(token) = token else {
      return Err(InteractionError::new(
        ErrorKind::Conflict,
        "Qoder has no active managed prompt to cancel",
      ));
    };
    self.publish_state(id);
    let sent = self
      .transport
      .send(
        json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}),
        true,
      )
      .await;
    if let Err(error) = sent {
      {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(id) {
          if session.prompt.as_ref().is_some_and(|prompt| prompt.token == token) {
            session.state.status = if error.kind == ErrorKind::Uncertain {
              ExecutionState::Unknown
            } else {
              ExecutionState::Running
            };
          }
        }
      }
      self.publish_state(id);
      return Err(error);
    }
    self.cancel_permissions(id).await.map_err(|_| {
      InteractionError::uncertain(
        "Qoder cancellation was dispatched but pending permission cancellation was not confirmed",
      )
    })?;
    Ok(Reply::Acknowledged)
  }
}

fn make_session(id: &str, catalog: models::Catalog) -> Session {
  Session {
    state: SessionState {
      session_id: Some(id.into()),
      status: ExecutionState::Idle,
      model: catalog.current,
      pending_inputs: Some(0),
      revision: None,
    },
    models: catalog.choices,
    model_route: catalog.route,
    prompt: None,
  }
}
fn apply_catalog(session: &mut Session, catalog: models::Catalog) {
  session.state.model = catalog.current;
  session.models = catalog.choices;
  session.model_route = catalog.route;
}

fn prompt_content(input: MessageInput, images_supported: bool) -> Result<Vec<acp::PromptContent>, InteractionError> {
  if input.text.chars().count() > 16 * 1024 || (input.text.trim().is_empty() && input.images.is_empty()) {
    return Err(InteractionError::invalid(
      "Qoder prompt must contain text or images within the text limit",
    ));
  }
  if input.images.len() > 4 {
    return Err(InteractionError::invalid("Qoder prompt supports at most four images"));
  }
  if !input.images.is_empty() && !images_supported {
    return Err(InteractionError::unsupported(
      "Qoder did not negotiate image prompt support",
    ));
  }
  let mut content = Vec::new();
  if !input.text.is_empty() {
    content.push(acp::PromptContent::Text { text: input.text });
  }
  let mut total = 0;
  for image in input.images {
    if !matches!(
      image.mime_type.as_str(),
      "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) || image.data.len() > 12 * 1024 * 1024
    {
      return Err(InteractionError::invalid("Invalid Qoder image type or size"));
    }
    let decoded = STANDARD
      .decode(&image.data)
      .map_err(|_| InteractionError::invalid("Invalid Qoder image base64"))?;
    total += decoded.len();
    if decoded.is_empty() || decoded.len() > 8 * 1024 * 1024 || total > 24 * 1024 * 1024 {
      return Err(InteractionError::invalid(
        "Qoder images exceed the per-image or prompt size limit",
      ));
    }
    content.push(acp::PromptContent::Image {
      data: image.data,
      mime_type: image.mime_type,
    });
  }
  Ok(content)
}
