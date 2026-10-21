//! DSH ApiProxy's HTTP/SSE four-quadrant protocol.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type ExtraFields = Map<String, Value>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
  #[serde(rename = "type")]
  pub native_type: String,
  pub rpc_id: String,
  pub method: String,
  pub payload: Value,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

impl Request {
  pub fn client(rpc_id: impl Into<String>, method: impl Into<String>, payload: Value) -> Self {
    Self {
      native_type: "client-request".into(),
      rpc_id: rpc_id.into(),
      method: method.into(),
      payload,
      extra: ExtraFields::new(),
    }
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcError {
  pub code: String,
  pub message: String,
  #[serde(default)]
  pub details: Value,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// Wide result preserves new fields; consumers must validate ok/value/error.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcResult {
  pub ok: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub value: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<RpcError>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
  #[serde(rename = "type")]
  pub native_type: String,
  pub rpc_id: String,
  pub result: RpcResult,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
  pub accepted: bool,
  #[serde(default)]
  pub reason: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDescription {
  pub version: String,
  pub cwd: String,
  #[serde(default)]
  pub provider: Option<String>,
  #[serde(default)]
  pub model: Option<String>,
  pub attached_sessions: u64,
  pub can_open_path: bool,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
  pub session_id: String,
  pub running: bool,
  #[serde(default)]
  pub cwd: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionList {
  pub items: Vec<SessionSummary>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelCatalog {
  pub groups: Vec<ModelGroup>,
  #[serde(default)]
  pub failures: Vec<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelGroup {
  pub id: String,
  pub name: String,
  pub models: Vec<CatalogModel>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogModel {
  pub id: String,
  pub name: String,
  #[serde(default)]
  pub description: Option<String>,
  #[serde(default)]
  pub reasoning: Option<Value>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionOption {
  pub label: String,
  #[serde(default)]
  pub description: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
  pub id: String,
  pub question: String,
  #[serde(default)]
  pub options: Vec<QuestionOption>,
  #[serde(default)]
  pub multi_select: bool,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn unknown_envelope_and_question_fields_survive() {
    let native = json!({"type":"server-request","rpcId":"question-1","method":"question/requested","payload":{"sessionId":"s","questions":[{"id":"q","question":"Pick?","future":true}]},"futureEnvelope":[1]});
    let decoded: Request = serde_json::from_value(native.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), native);
    let question: Question = serde_json::from_value(native["payload"]["questions"][0].clone()).unwrap();
    assert_eq!(question.extra["future"], true);
  }
}
