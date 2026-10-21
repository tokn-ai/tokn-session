//! Session-scoped model discovery and message-bound selection contracts.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelIdentity {
  pub model_provider: Option<String>,
  pub model_id: String,
  pub display_name: Option<String>,
}

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
  pub(crate) fn live(model: Option<ModelIdentity>, generation: &str) -> Self {
    Self::new(model, ModelStateSource::Live, generation)
  }

  pub(crate) fn unknown() -> Self {
    Self::new(None, ModelStateSource::Unknown, "")
  }

  pub(crate) fn new(model: Option<ModelIdentity>, source: ModelStateSource, generation: &str) -> Self {
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
  pub updates_pi_default: bool,
  /// The host can bind a composer model choice to the input admission.
  #[serde(default)]
  pub supports_model_input: bool,
}

impl SessionModelStatus {
  pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
    Self {
      state: SessionModelState::unknown(),
      can_select: false,
      can_list: false,
      reason: Some(reason.into()),
      runtime_generation: None,
      updates_pi_default: false,
      supports_model_input: false,
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelChoice {
  pub model: ModelIdentity,
  pub supports_images: bool,
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

pub(crate) fn validate_message_selection(selection: &MessageModelSelection) -> Result<(), String> {
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

pub(crate) fn valid_identity(value: &str) -> bool {
  !value.is_empty() && value.len() <= 512 && value.trim() == value && !value.chars().any(char::is_control)
}

/// Only compact display metadata crosses the viewer boundary. Native model
/// objects can include provider URLs and authentication headers.
pub(crate) fn pi_model(value: &serde_json::Value) -> Option<ModelIdentity> {
  let model_id = value.get("id")?.as_str()?.to_string();
  let model_provider = value.get("provider")?.as_str()?.to_string();
  if !valid_identity(&model_id) || !valid_identity(&model_provider) {
    return None;
  }
  Some(ModelIdentity {
    model_provider: Some(model_provider),
    model_id,
    display_name: value
      .get("name")
      .and_then(serde_json::Value::as_str)
      .filter(|name| valid_identity(name))
      .map(str::to_owned),
  })
}

pub(crate) fn pi_choices(value: &serde_json::Value) -> Result<Vec<ModelChoice>, String> {
  let values = value.as_array().ok_or("Pi returned an invalid model catalog")?;
  if values.len() > 512 {
    return Err("Pi model catalog exceeds the 512 model limit".into());
  }
  let mut identities = std::collections::HashSet::new();
  values
    .iter()
    .map(|value| {
      let model = pi_model(value).ok_or("Pi returned an invalid model")?;
      if !identities.insert((model.model_provider.clone(), model.model_id.clone())) {
        return Err("Pi returned duplicate model identities".into());
      }
      Ok(ModelChoice {
        model,
        supports_images: value
          .get("input")
          .and_then(serde_json::Value::as_array)
          .is_some_and(|input| input.iter().any(|value| value.as_str() == Some("image"))),
      })
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn pi_catalog_is_bounded_allowlisted_metadata() {
    let choices = pi_choices(
      &json!([{ "id":"test", "provider":"custom", "name":"Test", "input":["text","image"],
      "baseUrl":"secret-endpoint", "headers":{"authorization":"secret"} }]),
    )
    .unwrap();
    let display = serde_json::to_string(&choices).unwrap();
    assert!(choices[0].supports_images);
    assert!(!display.contains("secret"));
    assert!(pi_choices(&json!([{"id":"test","provider":"custom"},{"id":"test","provider":"custom"}])).is_err());
  }
}
