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
  /// Verified native no-turn state, required before creation-only model setup.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub blank: Option<bool>,
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
#[serde(rename_all = "camelCase")]
pub struct SessionCreateRequest {
  pub session_id: String,
  pub cwd: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub agent_preset: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateResponse {
  pub session_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub agent_preset: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// Native selection recorded for one session; a setter also persists DSH's default.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
  pub provider: String,
  pub model: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reasoning_effort: Option<String>,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSelectModelRequest {
  pub session_id: String,
  pub provider: String,
  pub model: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSelectModelResponse {
  pub selected: ModelSelection,
  #[serde(flatten)]
  pub extra: ExtraFields,
}

/// Activating native read. Adapters must restrict it to already created sessions.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionModels {
  pub current: ModelSelection,
  pub routable: bool,
  pub groups: Vec<ModelGroup>,
  #[serde(default)]
  pub failures: Vec<Value>,
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

  #[test]
  fn creation_preserves_native_names_and_omits_unselected_preset() {
    let request = SessionCreateRequest {
      session_id: "session-reserved".into(),
      cwd: "/workspace".into(),
      agent_preset: None,
    };
    assert_eq!(
      serde_json::to_value(request).unwrap(),
      json!({"sessionId":"session-reserved","cwd":"/workspace"})
    );
    let native = json!({"sessionId":"session-reserved","agentPreset":"default-preset","future":true});
    let response: SessionCreateResponse = serde_json::from_value(native.clone()).unwrap();
    assert_eq!(response.agent_preset.as_deref(), Some("default-preset"));
    assert_eq!(serde_json::to_value(response).unwrap(), native);
  }

  #[test]
  fn creation_model_wire_uses_verified_native_names_and_preserves_extensions() {
    assert_eq!(
      serde_json::to_value(SessionSelectModelRequest {
        session_id: "new-session".into(),
        provider: "configured-provider".into(),
        model: "chosen-model".into(),
      })
      .unwrap(),
      json!({"sessionId":"new-session","provider":"configured-provider","model":"chosen-model"})
    );
    let native = json!({"current":{"provider":"provider","model":"model","reasoningEffort":"high","future":"kept"},"routable":true,"groups":[],"failures":[],"future":true});
    let response: SessionModels = serde_json::from_value(native.clone()).unwrap();
    assert_eq!(serde_json::to_value(response).unwrap(), native);
  }
}
