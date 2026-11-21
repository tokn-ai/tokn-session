//! Qoder compatibility details that are not part of the current standard ACP schema.
//!
//! These wrappers deliberately keep retired model selectors and Qoder's optional
//! root response field out of tokn-acp-protocol. Standard session config wins.
use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct SessionResult {
  #[serde(flatten)]
  pub standard: acp::SessionResult,
  #[serde(rename = "models")]
  legacy_models: Option<Value>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyModels {
  current_model_id: String,
  available_models: Vec<LegacyModel>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyModel {
  model_id: String,
  name: String,
}
impl SessionResult {
  pub fn legacy_catalog(&self) -> Result<Option<models::Catalog>, InteractionError> {
    let Some(value) = self.legacy_models.as_ref() else {
      return Ok(None);
    };
    let models: LegacyModels = decode(value.clone(), false)?;
    models::catalog(
      &models.current_model_id,
      models
        .available_models
        .into_iter()
        .map(|model| (model.model_id, model.name))
        .collect(),
      models::Route::QoderLegacy,
    )
    .map(Some)
  }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PromptResult {
  #[serde(flatten)]
  pub standard: acp::PromptResult,
  user_message_id: Option<String>,
}
impl PromptResult {
  pub fn native_message_id(&self) -> Option<&str> {
    self.user_message_id.as_deref()
  }
}

pub(super) fn tool_name(call: &acp::ToolCall) -> Option<&str> {
  call.meta.as_ref()?.get("qoder")?.get("toolName")?.as_str()
}

pub(super) fn unsupported_interaction(call: &acp::ToolCall) -> bool {
  matches!(
    tool_name(call),
    Some("ExitPlanMode" | "AskUserQuestion" | "EnterPlanMode")
  )
}

pub(super) async fn set_legacy_model(
  transport: &transport::Transport,
  session: &str,
  model: &str,
) -> Result<(), InteractionError> {
  let response = transport
    .call("session/set_model", json!({"sessionId":session,"modelId":model}), true)
    .await?;
  if response.is_object() {
    Ok(())
  } else {
    Err(InteractionError::uncertain(
      "Qoder did not confirm its legacy model selection",
    ))
  }
}
