//! Native SDK stdio contracts. The adapter uses Gateway for catalog/state,
//! explicit steering/queue admission, and active-turn cancellation.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const METHODS: &[&str] = &[
  "initialize",
  "session/prompt",
  "session/query",
  "session/resume",
  "session/wait",
  "session/working-directory/get",
  "session/working-directory/set",
  "shutdown",
];
pub const HOST_REQUESTS: &[&str] = &["user-question/ask", "approval/request"];
pub const NOTIFICATIONS: &[&str] = &[
  "session.event",
  "session.status",
  "session.query.completed",
  "user-question.cancelled",
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
  pub approval_requests: Option<bool>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub capabilities: Option<Capabilities>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reasoning_effort: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub max_tokens: Option<u64>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub user_questions: Option<bool>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionParams {
  pub session_id: String,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeParams {
  pub session_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub permission_mode: Option<String>,
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
