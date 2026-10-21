//! Native headless Pi RPC. Unknown records remain lossless native JSON.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const MAX_DIALOG_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_DIALOG_OPTIONS: usize = 128;
pub const MAX_ID_BYTES: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Request {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<String>,
  #[serde(flatten)]
  pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
  Prompt {
    message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<ImageContent>,
    #[serde(rename = "streamingBehavior", skip_serializing_if = "Option::is_none")]
    streaming_behavior: Option<StreamingBehavior>,
  },
  Steer {
    message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<ImageContent>,
  },
  FollowUp {
    message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<ImageContent>,
  },
  Abort,
  GetState,
  GetAvailableModels,
  GetAvailableThinkingLevels,
  GetCommands,
  GetSessionStats,
  GetMessages,
  GetEntries {
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
  },
  GetTree,
  GetLastAssistantText,
  SetModel {
    provider: String,
    #[serde(rename = "modelId")]
    model_id: String,
  },
  SetThinkingLevel {
    level: String,
  },
  NewSession {
    #[serde(rename = "parentSession", skip_serializing_if = "Option::is_none")]
    parent_session: Option<String>,
  },
  SwitchSession {
    #[serde(rename = "sessionPath")]
    session_path: String,
  },
}

impl Command {
  pub fn name(&self) -> &'static str {
    match self {
      Self::Prompt { .. } => "prompt",
      Self::Steer { .. } => "steer",
      Self::FollowUp { .. } => "follow_up",
      Self::Abort => "abort",
      Self::GetState => "get_state",
      Self::GetAvailableModels => "get_available_models",
      Self::GetAvailableThinkingLevels => "get_available_thinking_levels",
      Self::GetCommands => "get_commands",
      Self::GetSessionStats => "get_session_stats",
      Self::GetMessages => "get_messages",
      Self::GetEntries { .. } => "get_entries",
      Self::GetTree => "get_tree",
      Self::GetLastAssistantText => "get_last_assistant_text",
      Self::SetModel { .. } => "set_model",
      Self::SetThinkingLevel { .. } => "set_thinking_level",
      Self::NewSession { .. } => "new_session",
      Self::SwitchSession { .. } => "switch_session",
    }
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum StreamingBehavior {
  #[serde(rename = "steer")]
  Steer,
  #[serde(rename = "followUp")]
  FollowUp,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImageContent {
  #[serde(rename = "type")]
  pub kind: String,
  pub data: String,
  #[serde(rename = "mimeType")]
  pub mime_type: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Response {
  #[serde(default)]
  pub id: Option<String>,
  pub command: String,
  pub success: bool,
  #[serde(default)]
  pub data: Option<Value>,
  #[serde(default)]
  pub error: Option<String>,
  #[serde(flatten)]
  pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionUiResponse {
  ExtensionUiResponse {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cancelled: Option<bool>,
  },
}

/// Typed projections always retain the full native envelope, including future fields.
#[derive(Clone, Debug)]
pub enum ServerMessage {
  Response {
    response: Response,
    native: Value,
  },
  ExtensionUi {
    dialog: Option<ExtensionDialog>,
    native: Value,
  },
  Notification {
    kind: String,
    native: Value,
  },
  Unknown(Value),
}

impl ServerMessage {
  pub fn decode(native: Value) -> Self {
    match native.get("type").and_then(Value::as_str) {
      Some("response") => match serde_json::from_value(native.clone()) {
        Ok(response) => Self::Response { response, native },
        Err(_) => Self::Unknown(native),
      },
      Some("extension_ui_request") => match parse_extension_dialog(&native) {
        Ok(dialog) => Self::ExtensionUi { dialog, native },
        Err(_) => Self::Unknown(native),
      },
      Some(kind) if is_notification(kind) => Self::Notification {
        kind: kind.to_owned(),
        native,
      },
      _ => Self::Unknown(native),
    }
  }

  pub fn native(&self) -> &Value {
    match self {
      Self::Response { native, .. }
      | Self::ExtensionUi { native, .. }
      | Self::Notification { native, .. }
      | Self::Unknown(native) => native,
    }
  }
}

fn is_notification(kind: &str) -> bool {
  matches!(
    kind,
    "agent_start"
      | "agent_end"
      | "agent_settled"
      | "turn_start"
      | "turn_end"
      | "message_start"
      | "message_update"
      | "message_end"
      | "tool_execution_start"
      | "tool_execution_update"
      | "tool_execution_end"
      | "queue_update"
      | "compaction_start"
      | "compaction_end"
      | "auto_compaction_start"
      | "auto_compaction_end"
      | "auto_retry_start"
      | "auto_retry_end"
      | "extension_error"
      | "bash_execution_update"
      | "summarization_retry_scheduled"
      | "summarization_retry_attempt_start"
      | "summarization_retry_finished"
  )
}

#[derive(Clone, Debug, PartialEq)]
pub struct State {
  pub session_id: String,
  pub session_file: Option<PathBuf>,
  pub session_name: Option<String>,
  pub thinking_level: Option<String>,
  pub is_streaming: bool,
  pub is_compacting: bool,
  pub pending_message_count: u64,
  pub images_available: bool,
  pub model: Option<Value>,
  pub native: Value,
}

pub fn parse_state(data: &Value) -> Result<State, String> {
  if !data.is_object() {
    return Err("Pi RPC state must be an object".into());
  }
  let session_id = identity(data, "sessionId")?.to_owned();
  let session_file = match data.get("sessionFile") {
    None | Some(Value::Null) => None,
    Some(Value::String(path)) if !path.trim().is_empty() && !path.contains('\0') => Some(PathBuf::from(path)),
    _ => return Err("Pi RPC state has an invalid sessionFile".into()),
  };
  Ok(State {
    session_id,
    session_file,
    session_name: data.get("sessionName").and_then(Value::as_str).map(str::to_owned),
    thinking_level: data.get("thinkingLevel").and_then(Value::as_str).map(str::to_owned),
    is_streaming: boolean(data, "isStreaming")?,
    is_compacting: boolean(data, "isCompacting")?,
    pending_message_count: data
      .get("pendingMessageCount")
      .and_then(Value::as_u64)
      .ok_or("Pi RPC state has an invalid pendingMessageCount")?,
    images_available: data
      .get("model")
      .and_then(|model| model.get("input"))
      .and_then(Value::as_array)
      .is_some_and(|input| input.iter().any(|value| value.as_str() == Some("image"))),
    model: data.get("model").filter(|model| !model.is_null()).cloned(),
    native: data.clone(),
  })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionDialog {
  pub dialog_id: String,
  pub kind: String,
  pub title: String,
  pub message: Option<String>,
  pub options: Vec<String>,
  pub initial_value: Option<String>,
  pub timeout_ms: Option<u64>,
}

pub fn is_cosmetic_extension_method(method: &str) -> bool {
  matches!(
    method,
    "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text"
  )
}

pub fn parse_extension_dialog(value: &Value) -> Result<Option<ExtensionDialog>, String> {
  if value.get("type").and_then(Value::as_str) != Some("extension_ui_request") {
    return Err("invalid Pi extension UI request type".into());
  }
  let method = value
    .get("method")
    .and_then(Value::as_str)
    .ok_or("Pi extension UI request has no method")?;
  if is_cosmetic_extension_method(method) {
    return Ok(None);
  }
  if !matches!(method, "confirm" | "select" | "input" | "editor") {
    return Err("unsupported Pi extension dialog method".into());
  }
  let mut dialog = ExtensionDialog {
    dialog_id: identity(value, "id")?.to_owned(),
    kind: method.to_owned(),
    title: dialog_text(value, "title", true)?.unwrap(),
    message: None,
    options: Vec::new(),
    initial_value: None,
    timeout_ms: value
      .get("timeout")
      .and_then(Value::as_u64)
      .filter(|timeout| *timeout > 0),
  };
  match method {
    "confirm" => dialog.message = dialog_text(value, "message", true)?,
    "select" => {
      let options = value
        .get("options")
        .and_then(Value::as_array)
        .ok_or("Pi select dialog has invalid options")?;
      if options.len() > MAX_DIALOG_OPTIONS {
        return Err("Pi select dialog exceeds the 128 option limit".into());
      }
      dialog.options = options
        .iter()
        .map(|option| {
          option
            .as_str()
            .filter(|text| text.len() <= MAX_DIALOG_TEXT_BYTES)
            .map(str::to_owned)
            .ok_or_else(|| "Pi select dialog has an invalid or oversized option".to_owned())
        })
        .collect::<Result<_, _>>()?;
    }
    "input" => dialog.message = dialog_text(value, "placeholder", false)?,
    "editor" => dialog.initial_value = dialog_text(value, "prefill", false)?,
    _ => unreachable!(),
  }
  Ok(Some(dialog))
}

fn dialog_text(value: &Value, field: &str, required: bool) -> Result<Option<String>, String> {
  match value.get(field) {
    None if !required => Ok(None),
    Some(Value::String(text)) if text.len() <= MAX_DIALOG_TEXT_BYTES => Ok(Some(text.clone())),
    _ => Err(format!("Pi extension dialog has an invalid or oversized {field}")),
  }
}

fn identity<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
  let identity = value
    .get(field)
    .and_then(Value::as_str)
    .ok_or_else(|| format!("Pi RPC has an invalid {field}"))?;
  if identity.is_empty()
    || identity.len() > MAX_ID_BYTES
    || identity.trim() != identity
    || identity.chars().any(char::is_control)
  {
    return Err(format!("Pi RPC has an invalid {field}"));
  }
  Ok(identity)
}

fn boolean(value: &Value, field: &str) -> Result<bool, String> {
  value
    .get(field)
    .and_then(Value::as_bool)
    .ok_or_else(|| format!("Pi RPC state has an invalid {field}"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn native_wire_names_and_unknown_records_are_preserved() {
    let request = Request {
      id: Some("r1".into()),
      command: Command::Prompt {
        message: "hello".into(),
        images: vec![],
        streaming_behavior: Some(StreamingBehavior::FollowUp),
      },
    };
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["streamingBehavior"], "followUp");
    assert!(value.get("images").is_none());
    for value in [
      json!({"type":"future_event","secret_native":1}),
      json!({"type":"response","success":"invalid"}),
      json!(["unknown"]),
    ] {
      let message = ServerMessage::decode(value.clone());
      assert!(matches!(message, ServerMessage::Unknown(_)));
      assert_eq!(message.native(), &value);
    }
  }

  #[test]
  fn response_keeps_future_fields_and_ui_hints_are_not_answers() {
    let value = json!({"type":"response","id":"r","command":"get_state","success":true,"future":42});
    let ServerMessage::Response { response, native } = ServerMessage::decode(value.clone()) else {
      panic!("response expected")
    };
    assert_eq!(response.extra["future"], 42);
    assert_eq!(native, value);
    let dialog = parse_extension_dialog(
      &json!({"type":"extension_ui_request","id":"d","method":"input","title":"Name","placeholder":"A hint"}),
    )
    .unwrap()
    .unwrap();
    assert_eq!(dialog.message.as_deref(), Some("A hint"));
    assert!(dialog.initial_value.is_none());
  }
}
