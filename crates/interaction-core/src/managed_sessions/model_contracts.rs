//! Session-scoped model discovery and message-bound selection contracts.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use crate::{ModelChoice, ModelIdentity};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStateSource {
  Live,
  Recorded,
  Catalog,
  Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionModelState {
  pub model: Option<ModelIdentity>,
  pub source: ModelStateSource,
  pub revision: String,
}

impl SessionModelState {
  pub fn live(model: Option<ModelIdentity>, generation: &str) -> Self {
    Self::new(model, ModelStateSource::Live, generation)
  }

  pub fn unknown() -> Self {
    Self::new(None, ModelStateSource::Unknown, "")
  }

  pub fn new(model: Option<ModelIdentity>, source: ModelStateSource, generation: &str) -> Self {
    let revision = format!(
      "{:x}",
      Sha256::digest(serde_json::to_vec(&(generation, &model, source)).unwrap())
    );
    Self {
      model,
      source,
      revision,
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionModelStatusRequest {
  pub session_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionModelStatus {
  pub state: SessionModelState,
  pub can_select: bool,
  pub can_list: bool,
  pub reason: Option<String>,
  pub runtime_generation: Option<String>,
  /// Preserve the established viewer wire key while keeping the core contract provider-neutral.
  #[serde(rename = "updates_pi_default")]
  pub updates_default: bool,
  /// The host can bind a composer model choice to the input admission.
  #[serde(default)]
  pub supports_model_input: bool,
}

impl SessionModelStatus {
  pub fn unavailable(reason: impl Into<String>) -> Self {
    Self {
      state: SessionModelState::unknown(),
      can_select: false,
      can_list: false,
      reason: Some(reason.into()),
      runtime_generation: None,
      updates_default: false,
      supports_model_input: false,
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListSessionModelsRequest {
  pub session_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListSessionModelsResponse {
  pub status: SessionModelStatus,
  pub models: Vec<ModelChoice>,
}

/// A draft choice captured with one message. It is never applied by opening
/// the picker, and a busy runtime discards it before steering the active turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageModelSelection {
  pub model_provider: Option<String>,
  pub model_id: String,
  pub expected_revision: String,
  pub runtime_generation: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageModelOutcome {
  Unchanged,
  Applied,
  IgnoredWhileRunning,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageModelResult {
  pub outcome: MessageModelOutcome,
  pub state: SessionModelState,
}

pub fn validate_message_selection(selection: &MessageModelSelection) -> Result<(), String> {
  if !valid_identity(&selection.model_id)
    || !valid_identity(&selection.expected_revision)
    || [&selection.model_provider, &selection.runtime_generation]
      .into_iter()
      .flatten()
      .any(|value| !valid_identity(value))
  {
    return Err("Invalid message model selection".into());
  }
  Ok(())
}

pub fn valid_identity(value: &str) -> bool {
  !value.is_empty() && value.len() <= 512 && value.trim() == value && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn generic_default_capability_preserves_the_established_wire_key() {
    let mut status = SessionModelStatus::unavailable("test");
    status.updates_default = true;
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(value["updates_pi_default"], true);
    assert!(value.get("updates_default").is_none());
    assert!(
      serde_json::from_value::<SessionModelStatus>(value)
        .unwrap()
        .updates_default
    );
  }
}
