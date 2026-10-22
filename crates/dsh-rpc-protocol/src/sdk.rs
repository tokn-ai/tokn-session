//! The native SDK stdio protocol is intentionally much smaller than ApiProxy.
//! It has no query, steer, cancel, resume, or human-question response methods.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const METHODS: &[&str] = &["initialize", "session/prompt", "shutdown"];
pub const NOTIFICATIONS: &[&str] = &[
  "session.event",
  "session.status",
  "subagent.started",
  "subagent.finished",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
  pub cwd: String,
  pub provider: String,
  pub model: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub max_tokens: Option<u64>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
  pub session_id: String,
  pub content_blocks: Vec<Value>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
  pub message_id: String,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

/// Unknown methods, fields, errors and provider-defined events stay inspectable.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Frame {
  pub jsonrpc: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub id: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub method: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub params: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub result: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<Value>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}
