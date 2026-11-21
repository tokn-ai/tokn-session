//! Current standard Agent Client Protocol v1 wire types.
//! Vendor metadata stays opaque; provider extensions and legacy APIs belong in adapters.
mod config;
pub use config::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
  pub protocol_version: u32,
  #[serde(default)]
  pub agent_capabilities: AgentCapabilities,
  pub agent_info: Option<AgentInfo>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
  pub name: String,
  pub title: Option<String>,
  pub version: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
  #[serde(default)]
  pub load_session: bool,
  #[serde(default)]
  pub prompt_capabilities: PromptCapabilities,
  #[serde(default)]
  pub session_capabilities: SessionCapabilities,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PromptCapabilities {
  #[serde(default)]
  pub image: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SessionCapabilities {
  pub resume: Option<ResumeCapability>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ResumeCapability {}

/// Common setup response fields. New-session clients must additionally require
/// `session_id`; load/resume responses may omit it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResult {
  pub session_id: Option<String>,
  pub config_options: Option<Vec<SessionConfigOption>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
  pub stop_reason: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptContent {
  Text {
    text: String,
  },
  Image {
    data: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
  },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdate {
  pub session_id: String,
  pub update: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
  pub session_id: String,
  pub tool_call: ToolCall,
  pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
  pub tool_call_id: String,
  pub title: Option<String>,
  pub kind: Option<String>,
  pub raw_input: Option<Value>,
  pub content: Option<Vec<Value>>,
  #[serde(rename = "_meta")]
  pub meta: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
  pub option_id: String,
  pub name: String,
  pub kind: String,
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn treats_omitted_capabilities_as_unsupported_and_tolerates_extensions() {
    let result: InitializeResult = serde_json::from_value(json!({
      "protocolVersion":1,"agentCapabilities":{"_meta":{"vendor":"fixture"}}
    }))
    .unwrap();
    assert!(!result.agent_capabilities.load_session);
    assert!(!result.agent_capabilities.prompt_capabilities.image);
    assert!(result.agent_capabilities.session_capabilities.resume.is_none());
  }

  #[test]
  fn preserves_opaque_permission_identifiers() {
    let request: PermissionRequest = serde_json::from_value(json!({
      "sessionId":"session-1","toolCall":{"toolCallId":"call-1","rawInput":{"command":"fixture"}},
      "options":[{"optionId":"once","kind":"allow_once","name":"Allow once"}],"extension":true
    }))
    .unwrap();
    assert_eq!(request.tool_call.tool_call_id, "call-1");
    assert_eq!(request.options[0].option_id, "once");
  }

  #[test]
  fn legacy_and_vendor_response_fields_do_not_change_standard_decoding() {
    let session: SessionResult = serde_json::from_value(json!({
      "sessionId":"session-1", "models":{"unrecognized_legacy_shape":true}
    }))
    .unwrap();
    assert_eq!(session.session_id.as_deref(), Some("session-1"));
    assert!(session.config_options.is_none());
    let prompt: PromptResult = serde_json::from_value(json!({
      "stopReason":"end_turn", "userMessageId":{"opaque_extension":true}
    }))
    .unwrap();
    assert_eq!(prompt.stop_reason, "end_turn");
  }

  #[test]
  fn validates_standard_implementation_and_resume_capability_shapes() {
    assert!(serde_json::from_value::<AgentInfo>(json!({"name":"agent"})).is_err());
    let implementation: AgentInfo = serde_json::from_value(json!({"name":"agent", "version":"1.0"})).unwrap();
    assert_eq!(implementation.version, "1.0");
    for value in [json!(true), json!("enabled"), json!(0)] {
      assert!(serde_json::from_value::<SessionCapabilities>(json!({"resume":value})).is_err());
    }
    let capabilities: SessionCapabilities = serde_json::from_value(json!({"resume":{"_meta":{"extra":true}}})).unwrap();
    assert!(capabilities.resume.is_some());
  }
}
