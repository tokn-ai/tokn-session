//! Public live interaction views. Native callback envelopes stay with their adapter.
use serde::{Deserialize, Serialize};
use tokn_interaction_core::{InteractionKind, InteractionResponse, PermissionDecision, QuestionItem};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInteractionsRequest {
  pub session_key: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionInteraction {
  pub interaction_id: String,
  pub kind: InteractionKind,
  pub title: String,
  pub details: Option<String>,
  pub questions: Vec<QuestionItem>,
  pub permission_decisions: Vec<PermissionDecision>,
  pub response_pending: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionInteractions {
  pub session_key: String,
  pub runtime_id: Option<String>,
  pub instance_id: Option<String>,
  pub interactions: Vec<SessionInteraction>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RespondSessionInteractionRequest {
  pub session_key: String,
  pub request_id: String,
  pub runtime_id: String,
  pub instance_id: String,
  pub interaction_id: String,
  pub response: InteractionResponse,
}
