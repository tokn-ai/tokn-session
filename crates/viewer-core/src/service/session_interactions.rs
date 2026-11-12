//! Live requests are independent of historical question cards and never activate agents.
use super::*;
use crate::{
  input::{DeliveryError, InputTarget},
  interactions::{
    RespondSessionInteractionRequest, SessionInteraction, SessionInteractions, SessionInteractionsRequest,
  },
  model::{InputDeliveryStatus, SubmitSessionInputRequest, SubmitSessionInputResponse},
  session_supervisor::SessionTransport,
};
use tokn_interaction_core::{InteractionKind, Operation, PendingInteraction, PermissionDecision};

impl ViewerService {
  pub async fn list_session_interactions(
    &self,
    request: SessionInteractionsRequest,
  ) -> Result<SessionInteractions, String> {
    let target = self.control_target(request.session_key).await?;
    let mut result = SessionInteractions {
      session_key: target.session_key.clone(),
      runtime_id: None,
      instance_id: None,
      interactions: Vec::new(),
    };
    // Recorded Codex questions and managed Pi dialogs retain their existing UI.
    if target.input.provider != ViewerProvider::ZCode {
      return Ok(result);
    }
    let Some(binding) = self.sessions.runtimes.binding(&target.session_key) else {
      return Ok(result);
    };
    if !binding.capabilities.supports(Operation::Respond)
      || !binding.capabilities.supports(Operation::PendingInteractions)
    {
      return Ok(result);
    }
    let pending = self.sessions.pending_rpc(&target).await?;
    if self
      .sessions
      .runtimes
      .binding(&target.session_key)
      .is_none_or(|current| current.target != binding.target)
    {
      return Err("The live runtime changed. Refresh pending requests.".into());
    }
    result.runtime_id = Some(binding.target.runtime_id);
    result.instance_id = Some(binding.target.instance_id);
    result.interactions = pending
      .into_iter()
      .filter(|pending| pending.session_id.as_deref() == Some(target.input.session_id.as_str()))
      .map(public_interaction)
      .collect();
    Ok(result)
  }

  pub async fn respond_session_interaction(
    &self,
    request: RespondSessionInteractionRequest,
  ) -> Result<SubmitSessionInputResponse, String> {
    self.check_managed_workspace()?;
    let rejected = |message| {
      crate::session_supervisor::input_response(request.request_id.clone(), Err(DeliveryError::not_sent(message)))
    };
    if !valid_request_id(&request.request_id)
      || [&request.runtime_id, &request.instance_id, &request.interaction_id]
        .into_iter()
        .any(|value| !valid_live_id(value))
    {
      return Ok(rejected("Invalid interaction response identity"));
    }
    let target = match self.control_target(request.session_key.clone()).await {
      Ok(target) => target,
      Err(message) => return Ok(rejected(&message)),
    };
    if target.input.provider != ViewerProvider::ZCode {
      return Ok(rejected("This provider uses its existing interaction controls"));
    }
    let broker_target = InputTarget {
      provider: target.input.provider,
      session_id: target.input.session_id.clone(),
      session_file: target.input.session_file.clone(),
    };
    // The receipt stores only a digest, so large answer batches remain separate
    // from prompt validation and secrets are absent from its display text.
    let response = serde_json::to_vec(&(
      &request.runtime_id,
      &request.instance_id,
      &request.interaction_id,
      &request.response,
    ))
    .map_err(|_| "Could not encode the interaction response")?;
    let message = SubmitSessionInputRequest {
      session_key: target.session_key.clone(),
      request_id: request.request_id.clone(),
      text: format!("session-control:interaction:{:x}", Sha256::digest(response)),
      image_ids: Vec::new(),
      model_selection: None,
    };
    if let Some(receipt) = self.input_broker.control_receipt(&broker_target, &message) {
      return Ok(response_status(receipt));
    }
    let Some(binding) = self.sessions.runtimes.binding(&target.session_key).filter(|binding| {
      binding.target.runtime_id == request.runtime_id && binding.target.instance_id == request.instance_id
    }) else {
      return Ok(rejected(
        "The live runtime changed. Refresh pending requests before answering.",
      ));
    };
    if self.sessions.transport(&target).await? != SessionTransport::Rpc {
      return Ok(rejected("This session's control transport changed"));
    }
    let sessions = self.sessions.clone();
    let result = self
      .input_broker
      .submit_control(&broker_target, message, async move {
        sessions
          .respond_rpc_binding(
            &target,
            &binding,
            request.request_id,
            request.interaction_id,
            request.response,
          )
          .await
      })
      .await;
    Ok(response_status(result))
  }
}

fn response_status(mut result: SubmitSessionInputResponse) -> SubmitSessionInputResponse {
  if result.status == InputDeliveryStatus::Accepted {
    result.message = "Response sent. Waiting for the agent to resolve the request.".into();
  }
  result
}

fn valid_request_id(id: &str) -> bool {
  uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
}

fn valid_live_id(id: &str) -> bool {
  !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}

fn public_interaction(pending: PendingInteraction) -> SessionInteraction {
  let permission_decisions = if pending.kind == InteractionKind::Permission {
    pending.native["permission_decisions"]
      .as_array()
      .into_iter()
      .flatten()
      .filter_map(|value| match value.as_str() {
        Some("allow_once") => Some(PermissionDecision::AllowOnce),
        Some("deny") => Some(PermissionDecision::Deny),
        _ => None,
      })
      .fold(Vec::new(), |mut decisions, decision| {
        if !decisions.contains(&decision) {
          decisions.push(decision);
        }
        decisions
      })
  } else {
    Vec::new()
  };
  SessionInteraction {
    interaction_id: pending.interaction_id,
    kind: pending.kind,
    title: pending.title,
    details: pending.native["details"].as_str().map(str::to_owned),
    questions: pending.questions,
    permission_decisions,
    response_pending: pending.native["response_pending"] == true,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct Repository(PathBuf);

  impl ViewerRepository for Repository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      if provider == ViewerProvider::ZCode {
        tokn_session_zcode::ZCodeSessionSource::new(Some(self.0.clone())).list_session_headers()
      } else {
        Ok(Vec::new())
      }
    }

    fn session_header_exact(&self, _: ViewerProvider, _: &Path, id: &str) -> Result<SessionHeader, String> {
      tokn_session_zcode::ZCodeSessionSource::new(Some(self.0.clone())).session_header_exact(id)
    }

    fn load_session(&self, _: &SessionLocator) -> Result<LoadedSession, String> {
      panic!("interaction inspection must not parse saved messages");
    }
  }

  #[tokio::test]
  async fn inspection_and_stale_responses_never_activate_a_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db.sqlite");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
      .execute_batch(include_str!("../../../zcode/fixtures/session.sql"))
      .unwrap();
    let header = tokn_session_zcode::ZCodeSessionSource::new(Some(path.clone()))
      .session_header_exact("sess_zcode")
      .unwrap();
    let key = encode_session_key(&locator_for_header(ViewerProvider::ZCode, &header)).unwrap();
    let service = ViewerService::new(Arc::new(Repository(path)));
    service.refresh_provider_catalog(ViewerProvider::ZCode).unwrap();
    let pending = service
      .list_session_interactions(SessionInteractionsRequest {
        session_key: key.clone(),
      })
      .await
      .unwrap();
    assert!(pending.runtime_id.is_none());
    assert!(pending.interactions.is_empty());
    let response = service
      .respond_session_interaction(RespondSessionInteractionRequest {
        session_key: key,
        request_id: uuid::Uuid::new_v4().to_string(),
        runtime_id: "stale-runtime".into(),
        instance_id: "stale-instance".into(),
        interaction_id: "perm_one".into(),
        response: tokn_interaction_core::InteractionResponse::Permission {
          decision: PermissionDecision::AllowOnce,
        },
      })
      .await
      .unwrap();
    assert_eq!(response.status, InputDeliveryStatus::NotSent);
    assert!(service.list_rpc_runtimes().await.unwrap().runtimes.is_empty());
    service.shutdown_managed_sessions().await;
  }

  #[test]
  fn public_projection_drops_native_envelopes_and_persistent_permissions() {
    let pending: PendingInteraction = serde_json::from_value(json!({
      "interaction_id":"perm_one", "session_id":"sess_one", "kind":"permission",
      "title":"Run the requested command?", "questions":[], "native":{
        "permission_decisions":["allow_once","allow_always","deny","deny"],
        "details":"echo fixture", "response_pending":true,
        "params":{"credential":"private-native-value"}, "id":"native-envelope"
      }
    }))
    .unwrap();
    let wire = serde_json::to_value(public_interaction(pending)).unwrap();
    assert_eq!(wire["permission_decisions"], json!(["allow_once", "deny"]));
    assert_eq!(wire["details"], "echo fixture");
    assert_eq!(wire["response_pending"], true);
    assert!(!wire.to_string().contains("private-native-value"));
    assert!(wire.get("native").is_none());
    assert!(wire.get("session_id").is_none());
  }

  #[test]
  fn response_ids_are_bounded_and_uuid_aliases_do_not_create_new_receipts() {
    let id = "9a1b2c3d-1234-4321-9876-0123456789ab";
    assert!(valid_request_id(id));
    assert!(!valid_request_id(&id.to_uppercase()));
    assert!(!valid_live_id("request\n"));
    assert!(!valid_live_id(&"x".repeat(513)));
  }
}
