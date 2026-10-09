//! Structured question payloads. Unknown fields remain available for inspection.

use serde::{Deserialize, Serialize};

use crate::rollout::ExtraFields;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AsyncUserInputQuestion {
  pub title: String,
  #[serde(default)]
  pub options: Option<Vec<String>>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RequestUserInputQuestion {
  pub id: String,
  pub header: String,
  pub question: String,
  #[serde(default, rename = "isOther", alias = "is_other")]
  pub is_other: bool,
  #[serde(default, rename = "isSecret", alias = "is_secret")]
  pub is_secret: bool,
  #[serde(default)]
  pub options: Option<Vec<RequestUserInputOption>>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RequestUserInputOption {
  pub label: String,
  pub description: String,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RequestUserInputEvent {
  pub call_id: String,
  #[serde(default)]
  pub turn_id: Option<String>,
  pub questions: Vec<RequestUserInputQuestion>,
  #[serde(default, rename = "isBlocking", alias = "is_blocking")]
  pub is_blocking: Option<bool>,
  #[serde(default, rename = "autoResolutionMs", alias = "auto_resolution_ms")]
  pub auto_resolution_ms: Option<u64>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}
