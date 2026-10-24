//! Native OpenCode V2 wire types. Historical V1 storage is a separate protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

mod sse;
pub use sse::{SseDecoder, SseError};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Envelope<T> {
  pub data: T,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub location: Option<Location>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Location {
  pub directory: String,
  #[serde(rename = "workspaceID", default, skip_serializing_if = "Option::is_none")]
  pub workspace_id: Option<String>,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelRef {
  pub id: String,
  #[serde(rename = "providerID")]
  pub provider_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub variant: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionCreateRequest {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub id: Option<String>,
  pub location: Location,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub agent: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<ModelRef>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionInfo {
  pub id: String,
  pub location: Location,
  #[serde(default)]
  pub title: Option<String>,
  #[serde(default)]
  pub model: Option<ModelRef>,
  #[serde(default)]
  pub agent: Option<String>,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelInfo {
  pub id: String,
  #[serde(rename = "providerID")]
  pub provider_id: String,
  pub name: String,
  pub capabilities: ModelCapabilities,
  #[serde(default)]
  pub enabled: bool,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelCapabilities {
  #[serde(default)]
  pub input: Vec<String>,
  #[serde(default)]
  pub output: Vec<String>,
  #[serde(default)]
  pub tools: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
  Steer,
  Queue,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Prompt {
  pub text: String,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub files: Vec<FileAttachment>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileAttachment {
  pub uri: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PromptRequest {
  pub id: String,
  pub prompt: Prompt,
  pub delivery: Delivery,
  pub resume: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Admission {
  pub id: String,
  #[serde(rename = "sessionID")]
  pub session_id: String,
  pub prompt: Value,
  pub delivery: Delivery,
  #[serde(rename = "admittedSeq")]
  pub admitted_seq: u64,
  #[serde(rename = "promotedSeq", default)]
  pub promoted_seq: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionRequest {
  pub id: String,
  #[serde(rename = "sessionID")]
  pub session_id: String,
  pub questions: Vec<Question>,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Question {
  pub question: String,
  pub header: String,
  pub options: Vec<QuestionOption>,
  #[serde(default)]
  pub multiple: bool,
  #[serde(default = "default_true")]
  pub custom: bool,
}

fn default_true() -> bool {
  true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionOption {
  pub label: String,
  pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PermissionRequest {
  pub id: String,
  #[serde(rename = "sessionID")]
  pub session_id: String,
  pub action: String,
  pub resources: Vec<String>,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

/// Native events retain unknown kinds and added envelope fields unchanged.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NativeEvent {
  pub id: String,
  #[serde(rename = "type")]
  pub kind: String,
  pub data: Value,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub location: Option<Location>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub durable: Option<Value>,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn preserves_unknown_event_and_fields() {
    let native = json!({"id":"evt_1","type":"future.event","data":{"new":true},"next":42});
    let event: NativeEvent = serde_json::from_value(native.clone()).unwrap();
    assert_eq!(serde_json::to_value(event).unwrap(), native);
  }

  #[test]
  fn creation_uses_preallocated_id_and_native_workspace_name() {
    let request = SessionCreateRequest {
      id: Some("ses_reserved".into()),
      location: Location {
        directory: "/workspace".into(),
        workspace_id: Some("wrk_native".into()),
        extra: BTreeMap::new(),
      },
      agent: None,
      model: None,
    };
    assert_eq!(
      serde_json::to_value(request).unwrap(),
      json!({"id":"ses_reserved","location":{"directory":"/workspace","workspaceID":"wrk_native"}})
    );
  }

  #[test]
  fn question_custom_defaults_to_true() {
    let question: Question = serde_json::from_value(json!({
      "question":"Choose", "header":"Choice", "options":[], "multiple":true
    }))
    .unwrap();
    assert!(question.custom);
    assert!(question.multiple);
  }

  #[test]
  fn prompt_uses_native_names_and_explicit_delivery() {
    let request = PromptRequest {
      id: "msg_test".into(),
      prompt: Prompt {
        text: "hello".into(),
        files: vec![],
      },
      delivery: Delivery::Steer,
      resume: true,
    };
    assert_eq!(
      serde_json::to_value(request).unwrap(),
      json!({
        "id":"msg_test", "prompt":{"text":"hello"}, "delivery":"steer", "resume":true
      })
    );
  }
}
