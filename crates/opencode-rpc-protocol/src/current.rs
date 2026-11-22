//! OpenCode 2.x session inbox and form wire contracts. Earlier V2 shapes remain
//! available at the crate root for runtimes that still expose that API.
use crate::{Delivery, Prompt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PromptRequest {
  pub id: String,
  #[serde(flatten)]
  pub prompt: Prompt,
  pub delivery: Delivery,
  pub resume: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Admission {
  pub id: String,
  #[serde(rename = "sessionID")]
  pub session_id: String,
  #[serde(rename = "type")]
  pub kind: String,
  pub payload: Value,
  pub delivery: Delivery,
  pub time: AdmissionTime,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AdmissionTime {
  pub created: i64,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

/// Forms can contain fields beyond the interaction contract. Preserve them so
/// clients can decline unsupported forms without inventing an answer schema.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FormRequest {
  pub id: String,
  #[serde(rename = "sessionID")]
  pub session_id: String,
  pub title: String,
  pub fields: Vec<Value>,
  #[serde(default)]
  pub metadata: Option<Value>,
  #[serde(flatten)]
  pub extra: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn current_prompt_is_flat_and_inbox_admission_is_not_a_legacy_projection() {
    let prompt = PromptRequest {
      id: "msg_native".into(),
      prompt: Prompt {
        text: "Hello".into(),
        files: Vec::new(),
      },
      delivery: Delivery::Queue,
      resume: true,
    };
    assert_eq!(
      serde_json::to_value(prompt).unwrap(),
      json!({"id":"msg_native","text":"Hello","delivery":"queue","resume":true})
    );
    let admission: Admission = serde_json::from_value(json!({
      "id":"msg_native","sessionID":"ses_native","type":"user","payload":{"text":"Hello"},
      "delivery":"queue","time":{"created":1},"added":true
    }))
    .unwrap();
    assert_eq!(admission.kind, "user");
    assert_eq!(admission.extra["added"], true);
  }
}
