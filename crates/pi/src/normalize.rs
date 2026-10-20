use serde_json::Value;

use crate::event::{PiContentBlock, PiMessage, PiMessageItem, PiSessionItem, PiSessionLine, PiUserContent};
use tokn_session_core::{
  AgentEvent, ErrorEvent, MessageDelivery, MessageEvent, MessageImage, MessageImageSource, MetadataEvent, MetadataKind,
  Phase, Provider, ProviderChanged, ReasoningEvent, Role, SessionStarted, ToolCallEvent, ToolRecordKind, ToolTransport,
  UnknownEvent, UsageKind, tool_kind_for_name, tool_summary_for_input, tool_summary_for_io,
};

pub struct PiNormalizer {
  session_id: Option<String>,
}

impl PiNormalizer {
  pub fn new() -> Self {
    Self { session_id: None }
  }

  pub fn normalize(&mut self, line: PiSessionLine) -> Vec<AgentEvent> {
    let line_timestamp = line.timestamp().map(str::to_string);
    let (event, native) = line.into_parts();
    match event {
      PiSessionItem::Session(event) => {
        let Some(session_id) = event.id else {
          return vec![unknown_event(
            self.session_id.clone(),
            Some("session".to_string()),
            Some(native),
            line_timestamp,
          )];
        };
        self.session_id = Some(session_id.clone());
        vec![AgentEvent::SessionStarted(SessionStarted {
          provider: Provider::Pi,
          session_id,
          cwd: event.cwd,
          timestamp: event.timestamp.or(line_timestamp),
        })]
      }
      PiSessionItem::ModelChange(event) => vec![AgentEvent::ProviderChanged(ProviderChanged {
        provider: Provider::Pi,
        session_id: self.session_id.clone(),
        native_id: event.id,
        native_parent_id: event.parent_id,
        model_provider: event.provider,
        model_id: event.model_id,
        thinking_level: None,
        timestamp: event.timestamp.or(line_timestamp),
      })],
      PiSessionItem::ThinkingLevelChange(event) => {
        vec![AgentEvent::ProviderChanged(ProviderChanged {
          provider: Provider::Pi,
          session_id: self.session_id.clone(),
          native_id: event.id,
          native_parent_id: event.parent_id,
          model_provider: None,
          model_id: None,
          thinking_level: event.thinking_level,
          timestamp: event.timestamp.or(line_timestamp),
        })]
      }
      PiSessionItem::Message(event) => normalize_message(self.session_id.clone(), event, native, line_timestamp),
      PiSessionItem::Error(event) => {
        let timestamp = event.timestamp.or(line_timestamp);
        let message = event
          .message
          .filter(|message| !message.trim().is_empty())
          .or_else(|| event.error.and_then(error_payload_text))
          .unwrap_or_else(|| "unknown pi error".to_string());
        vec![
          AgentEvent::Error(ErrorEvent {
            provider: Provider::Pi,
            session_id: self.session_id.clone(),
            message,
            native: Some(native.clone()),
            timestamp: timestamp.clone(),
          }),
          AgentEvent::Metadata(MetadataEvent {
            provider: Provider::Pi,
            session_id: self.session_id.clone(),
            kind: MetadataKind::Diagnostic,
            native_type: "error".into(),
            summary: "Pi session error".into(),
            native,
            timestamp,
          }),
        ]
      }
      PiSessionItem::Compaction(_)
      | PiSessionItem::BranchSummary(_)
      | PiSessionItem::Custom(_)
      | PiSessionItem::CustomMessage(_)
      | PiSessionItem::Label(_)
      | PiSessionItem::SessionInfo(_)
      | PiSessionItem::Leaf(_)
      | PiSessionItem::ActiveToolsChange(_) => {
        crate::records::normalize(self.session_id.clone(), native, line_timestamp)
      }
      PiSessionItem::Unknown(event) => vec![unknown_event(
        self.session_id.clone(),
        event.native_type,
        Some(event.native),
        line_timestamp,
      )],
    }
  }
}

fn normalize_message(
  session_id: Option<String>,
  event: PiMessageItem,
  native: Value,
  line_timestamp: Option<String>,
) -> Vec<AgentEvent> {
  let meta = PiMessageMeta {
    id: event.id,
    parent_id: event.parent_id,
    timestamp: event.timestamp.or(line_timestamp),
  };

  let Some(message) = event.message else {
    return vec![unknown_event(
      session_id,
      Some("message".to_string()),
      Some(native),
      meta.timestamp,
    )];
  };

  let usage_kind = match &message {
    PiMessage::Assistant(_) => Some(UsageKind::ModelCall),
    PiMessage::ToolResult(_) => Some(UsageKind::OperationTotal),
    _ => None,
  };
  let accounting = usage_kind.and_then(|kind| {
    native["message"]
      .get("usage")
      .filter(|value| !value.is_null())
      .map(|usage| {
        crate::records::usage(
          session_id.clone(),
          meta.id.clone(),
          true,
          kind,
          usage.clone(),
          meta.timestamp.clone(),
        )
      })
  });
  let outcome = match &message {
    PiMessage::Assistant(message) => normalize_assistant_outcome(session_id.clone(), &meta, message, &native),
    _ => Vec::new(),
  };
  let mut events = match message {
    PiMessage::User(message) => normalize_user_message(session_id, &meta, message),
    PiMessage::Assistant(message) => normalize_assistant_message(session_id, &meta, message, !outcome.is_empty()),
    PiMessage::ToolResult(message) => normalize_tool_result_message(session_id, &meta, message),
    PiMessage::Unknown(message) => vec![unknown_event(
      session_id,
      message
        .native_type
        .map(|message_type| format!("message.{message_type}"))
        .or_else(|| Some("message".to_string())),
      Some(message.native),
      meta.timestamp,
    )],
  };
  if !outcome.is_empty() {
    // An empty failed/cancelled response has a known disposition. Keep any
    // unsupported content, but don't also manufacture an empty unknown event.
    events.retain(|event| !matches!(event, AgentEvent::Unknown(event) if event.native.is_none()));
    events.extend(outcome);
  }
  if let Some(accounting) = accounting {
    // A usage-only assistant record is still useful; don't manufacture unknown
    // content for a valid empty response. Unsupported content remains visible.
    if matches!(accounting, AgentEvent::Usage(_)) && native["message"]["content"] == serde_json::json!([]) {
      events.retain(|event| !matches!(event, AgentEvent::Unknown(event) if event.native.is_none()));
    }
    events.push(accounting);
  }
  events
}

fn normalize_assistant_outcome(
  session_id: Option<String>,
  meta: &PiMessageMeta,
  message: &crate::event::PiAssistantMessage,
  native: &Value,
) -> Vec<AgentEvent> {
  let cancelled = message.stop_reason.as_deref() == Some("aborted");
  let error = message
    .error_message
    .as_deref()
    .filter(|message| !message.trim().is_empty());
  if !cancelled && message.stop_reason.as_deref() != Some("error") && error.is_none() {
    return Vec::new();
  }
  let timestamp = meta
    .timestamp
    .clone()
    .or_else(|| message.timestamp.map(|value| value.to_string()));
  let native = assistant_diagnostic_native(native);
  let mut events = Vec::new();
  if !cancelled {
    events.push(AgentEvent::Error(ErrorEvent {
      provider: Provider::Pi,
      session_id: session_id.clone(),
      message: error.unwrap_or("Pi assistant response failed").to_string(),
      native: Some(native.clone()),
      timestamp: timestamp.clone(),
    }));
  }
  // Retain the existing diagnostic alongside the visible error (or cancellation),
  // including id/parentId, provider/model, stopReason, and provider diagnostics.
  // Both payloads omit provider-withheld thinking content.
  // Pi assistant entries have no authoritative turn ID for Lifecycle events.
  events.push(AgentEvent::Metadata(MetadataEvent {
    provider: Provider::Pi,
    session_id,
    kind: MetadataKind::Diagnostic,
    native_type: "message".into(),
    summary: if cancelled {
      "Pi assistant response cancelled"
    } else {
      "Pi assistant response failed"
    }
    .into(),
    native,
    timestamp,
  }));
  events
}

fn assistant_diagnostic_native(native: &Value) -> Value {
  let mut native = native.clone();
  if let Some(content) = native
    .get_mut("message")
    .and_then(|message| message.get_mut("content"))
    .and_then(Value::as_array_mut)
  {
    for block in content {
      if block.get("type").and_then(Value::as_str) == Some("thinking")
        && block.get("redacted").and_then(Value::as_bool) == Some(true)
      {
        // Keep only the explicit marker: unknown fields may carry additional
        // withheld payload just as thinking/thinkingSignature/data do.
        *block = serde_json::json!({"type":"thinking", "redacted":true});
      }
    }
  }
  native
}

struct PiMessageMeta {
  id: Option<String>,
  parent_id: Option<String>,
  timestamp: Option<String>,
}

fn normalize_user_message(
  session_id: Option<String>,
  meta: &PiMessageMeta,
  message: crate::event::PiUserMessage,
) -> Vec<AgentEvent> {
  let timestamp = meta
    .timestamp
    .clone()
    .or_else(|| message.timestamp.map(|value| value.to_string()));
  let mut events = Vec::new();

  match message.content {
    PiUserContent::Text(text) => events.push(message_event(
      session_id.clone(),
      meta,
      Role::User,
      text,
      timestamp.clone(),
    )),
    PiUserContent::Blocks(blocks) => {
      for (index, block) in blocks.into_iter().enumerate() {
        match block {
          PiContentBlock::Text(content) => {
            let native = json_value(&content);
            if let Some(text) = content.text.filter(|text| !text.is_empty()) {
              events.push(message_event(
                session_id.clone(),
                meta,
                Role::User,
                text,
                timestamp.clone(),
              ));
            } else {
              events.push(unknown_event(
                session_id.clone(),
                Some("message.content.text".to_string()),
                Some(native),
                timestamp.clone(),
              ));
            }
          }
          PiContentBlock::Image(content) => {
            let mut event = message_event(session_id.clone(), meta, Role::User, String::new(), timestamp.clone());
            if let AgentEvent::Message(message) = &mut event {
              message.images.push(recorded_image(index, content));
            }
            events.push(event);
          }
          PiContentBlock::Unknown(content) => events.push(unknown_event(
            session_id.clone(),
            prefixed_type("message.content", content.native_type.as_deref()),
            Some(content.native),
            timestamp.clone(),
          )),
          content => {
            let native_type = prefixed_type("message.content", content.native_type());
            events.push(unknown_event(
              session_id.clone(),
              native_type,
              Some(json_value(content)),
              timestamp.clone(),
            ));
          }
        }
      }
    }
    PiUserContent::Missing => {}
    PiUserContent::Unknown(content) => events.push(unknown_event(
      session_id.clone(),
      Some("message.content".to_string()),
      Some(content),
      timestamp.clone(),
    )),
  }

  ensure_message_events(combine_message_blocks(events), session_id, timestamp)
}

fn normalize_assistant_message(
  session_id: Option<String>,
  meta: &PiMessageMeta,
  message: crate::event::PiAssistantMessage,
  has_outcome: bool,
) -> Vec<AgentEvent> {
  let timestamp = meta
    .timestamp
    .clone()
    .or_else(|| message.timestamp.map(|value| value.to_string()));
  let mut events = Vec::new();

  for (index, block) in message.content.into_iter().enumerate() {
    match block {
      PiContentBlock::Text(content) => {
        let native = json_value(&content);
        match content.text {
          Some(text) if !text.is_empty() => events.push(message_event(
            session_id.clone(),
            meta,
            Role::Assistant,
            text,
            timestamp.clone(),
          )),
          // Pi's run-failure handler emits an empty recognized text block.
          // The outcome diagnostic preserves it without inventing unknown
          // content. Missing/malformed text and unsupported blocks still surface.
          Some(_) if has_outcome => {}
          _ => events.push(unknown_event(
            session_id.clone(),
            Some("message.content.text".to_string()),
            Some(native),
            timestamp.clone(),
          )),
        }
      }
      PiContentBlock::Thinking(content) => {
        let redacted = content.redacted;
        if content.thinking.is_some() || content.thinking_signature.is_some() || redacted == Some(true) {
          events.push(AgentEvent::Reasoning(ReasoningEvent {
            provenance: None,
            provider: Provider::Pi,
            session_id: session_id.clone(),
            message_id: meta.id.clone(),
            parent_id: meta.parent_id.clone(),
            phase: Phase::Finished,
            text: (redacted != Some(true))
              .then(|| content.thinking.and_then(present_text))
              .flatten(),
            summary: None,
            redacted,
            encrypted_content: None,
            signature: content.thinking_signature,
            timestamp: timestamp.clone(),
          }));
        }
      }
      PiContentBlock::ToolCall(content) => {
        let native = json_value(&content);
        let Some(name) = content.name else {
          events.push(unknown_event(
            session_id.clone(),
            Some("message.content.toolCall".to_string()),
            Some(native),
            timestamp.clone(),
          ));
          continue;
        };
        events.push(AgentEvent::ToolCall(ToolCallEvent {
          provider: Provider::Pi,
          session_id: session_id.clone(),
          turn_id: None,
          message_id: meta.id.clone(),
          parent_id: meta.parent_id.clone(),
          record_kind: ToolRecordKind::Invocation,
          tool_call_id: content.id,
          provider_tool_name: Some(name.clone()),
          tool_name: Some(name.clone()),
          tool_kind: tool_kind_for_name(&name),
          transport: Some(ToolTransport::Native),
          summary: tool_summary_for_input(&name, &content.arguments),
          phase: Phase::Started,
          input: Some(content.arguments),
          output: None,
          is_error: None,
          native: Some(native),
          timestamp: timestamp.clone(),
        }));
      }
      PiContentBlock::Image(content) => {
        let mut event = message_event(
          session_id.clone(),
          meta,
          Role::Assistant,
          String::new(),
          timestamp.clone(),
        );
        if let AgentEvent::Message(message) = &mut event {
          message.images.push(recorded_image(index, content));
        }
        events.push(event);
      }
      PiContentBlock::Unknown(content) => events.push(unknown_event(
        session_id.clone(),
        prefixed_type("message.content", content.native_type.as_deref()),
        Some(content.native),
        timestamp.clone(),
      )),
    }
  }

  ensure_message_events(combine_message_blocks(events), session_id, timestamp)
}

fn recorded_image(index: usize, content: tokn_pi_protocol::ImageContent) -> MessageImage {
  MessageImage {
    image_id: format!("image.v1.{index}"),
    mime_type: content.mime_type,
    name: None,
    source: content
      .data
      .filter(|data| !data.is_empty())
      .map(|data| MessageImageSource::Base64 { data })
      .unwrap_or(MessageImageSource::Unavailable { reference: None }),
  }
}

fn combine_message_blocks(events: Vec<AgentEvent>) -> Vec<AgentEvent> {
  let mut combined = Vec::with_capacity(events.len());
  for event in events {
    if let AgentEvent::Message(message) = &event
      && let Some(AgentEvent::Message(previous)) = combined.last_mut()
      && previous.message_id == message.message_id
      && previous.role == message.role
    {
      if !previous.text.is_empty() && !message.text.is_empty() {
        previous.text.push('\n');
      }
      previous.text.push_str(&message.text);
      previous.images.extend(message.images.iter().cloned());
    } else {
      combined.push(event);
    }
  }
  combined
}

fn normalize_tool_result_message(
  session_id: Option<String>,
  meta: &PiMessageMeta,
  message: crate::event::PiToolResultMessage,
) -> Vec<AgentEvent> {
  let timestamp = meta
    .timestamp
    .clone()
    .or_else(|| message.timestamp.map(|value| value.to_string()));
  let content = Value::Array(message.content.into_iter().map(json_value).collect());
  let details = message.details;
  let output = match details.as_ref() {
    Some(details) => serde_json::json!({
      "content": content,
      "details": details,
    }),
    None => content,
  };
  let tool_name = message.tool_name;

  vec![AgentEvent::ToolCall(ToolCallEvent {
    provider: Provider::Pi,
    session_id,
    turn_id: None,
    message_id: meta.id.clone(),
    parent_id: meta.parent_id.clone(),
    record_kind: ToolRecordKind::Result,
    tool_call_id: message.tool_call_id,
    provider_tool_name: tool_name.clone(),
    tool_name: tool_name.clone(),
    tool_kind: tool_name
      .as_deref()
      .map(tool_kind_for_name)
      .unwrap_or(tokn_session_core::ToolKind::Unknown),
    transport: Some(ToolTransport::Native),
    summary: tool_summary_for_io(tool_name.as_deref(), details.as_ref(), Some(&output)),
    phase: Phase::Finished,
    input: None,
    output: Some(output),
    is_error: message.is_error,
    native: None,
    timestamp,
  })]
}

fn message_event(
  session_id: Option<String>,
  meta: &PiMessageMeta,
  role: Role,
  text: String,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Message(MessageEvent {
    images: Vec::new(),
    provenance: None,
    provider: Provider::Pi,
    session_id,
    message_id: meta.id.clone(),
    parent_id: meta.parent_id.clone(),
    role,
    delivery: match role {
      Role::Assistant => MessageDelivery::Final,
      _ => MessageDelivery::Unspecified,
    },
    phase: Phase::Finished,
    text,
    timestamp,
  })
}

fn ensure_message_events(
  events: Vec<AgentEvent>,
  session_id: Option<String>,
  timestamp: Option<String>,
) -> Vec<AgentEvent> {
  if events.is_empty() {
    return vec![unknown_event(session_id, Some("message".to_string()), None, timestamp)];
  }
  events
}

fn unknown_event(
  session_id: Option<String>,
  native_type: Option<String>,
  native: Option<Value>,
  timestamp: Option<String>,
) -> AgentEvent {
  AgentEvent::Unknown(UnknownEvent {
    provider: Provider::Pi,
    session_id,
    native_type,
    native,
    timestamp,
  })
}

fn present_text(text: String) -> Option<String> {
  (!text.is_empty()).then_some(text)
}

fn error_payload_text(value: Value) -> Option<String> {
  match value {
    Value::String(text) => (!text.trim().is_empty()).then_some(text),
    Value::Number(number) => Some(number.to_string()),
    Value::Object(fields) if !fields.is_empty() => Some(Value::Object(fields).to_string()),
    Value::Array(items) if !items.is_empty() => Some(Value::Array(items).to_string()),
    _ => None,
  }
}

fn prefixed_type(prefix: &str, native_type: Option<&str>) -> Option<String> {
  Some(match native_type {
    Some(native_type) => format!("{prefix}.{native_type}"),
    None => prefix.to_string(),
  })
}

fn json_value(value: impl serde::Serialize) -> Value {
  serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokn_session_core::{ToolKind, ToolSummary};

  #[test]
  fn image_only_and_mixed_messages_preserve_images_without_tool_reordering() {
    let events = normalize_fixture(
      r#"{"type":"session","id":"pi"}
{"type":"message","id":"user","message":{"role":"user","content":[{"type":"image","mimeType":"image/png","data":"YQ=="}]}}
{"type":"message","id":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"caption"},{"type":"image","mimeType":"image/jpeg","data":"Yg=="},{"type":"toolCall","id":"call","name":"read","arguments":{}},{"type":"image","mimeType":"image/png"}]}}"#,
    );
    let [
      AgentEvent::SessionStarted(_),
      AgentEvent::Message(user),
      AgentEvent::Message(assistant),
      AgentEvent::ToolCall(_),
      AgentEvent::Message(after),
    ] = &events[..]
    else {
      panic!("unexpected image projections: {events:?}")
    };
    assert!(user.text.is_empty());
    assert_eq!(user.images[0].image_id, "image.v1.0");
    assert!(matches!(&user.images[0].source, MessageImageSource::Base64 { data } if data == "YQ=="));
    assert_eq!(assistant.text, "caption");
    assert_eq!(assistant.images[0].mime_type.as_deref(), Some("image/jpeg"));
    assert_eq!(assistant.images[0].image_id, "image.v1.1");
    assert_eq!(after.images[0].image_id, "image.v1.3");
    assert!(matches!(
      &after.images[0].source,
      MessageImageSource::Unavailable { reference: None }
    ));
  }

  #[test]
  fn normalizes_basic_fixture_without_stopping_at_unknown_roles() {
    let events = normalize_fixture(include_str!("../fixtures/basic_session.jsonl"));

    assert_eq!(events.len(), 11);
    assert!(matches!(&events[0], AgentEvent::SessionStarted(event) if event.session_id == "pi-session"));
    assert!(
      matches!(&events[1], AgentEvent::ProviderChanged(event) if event.model_provider.as_deref() == Some("openai") && event.model_id.as_deref() == Some("gpt-5"))
    );
    assert!(
      matches!(&events[2], AgentEvent::ProviderChanged(event) if event.thinking_level.as_deref() == Some("high"))
    );
    assert!(
      matches!(&events[3], AgentEvent::Message(event) if matches!(event.role, Role::User) && event.text == "inspect the project")
    );
    assert!(
      matches!(&events[4], AgentEvent::Reasoning(event) if event.text.as_deref() == Some("checking files") && event.signature.as_deref() == Some("sig-1"))
    );
    assert!(
      matches!(&events[5], AgentEvent::ToolCall(event) if event.tool_call_id.as_deref() == Some("call-1") && matches!(event.tool_kind, ToolKind::FileRead) && matches!(event.phase, Phase::Started))
    );
    assert!(
      matches!(&events[6], AgentEvent::Message(event) if matches!(event.role, Role::Assistant) && matches!(event.delivery, MessageDelivery::Final) && event.text == "done")
    );
    assert!(
      matches!(&events[7], AgentEvent::ToolCall(event) if event.tool_call_id.as_deref() == Some("call-1") && event.is_error == Some(false) && matches!(event.phase, Phase::Finished))
    );
    assert!(
      matches!(&events[8], AgentEvent::Unknown(event) if event.native_type.as_deref() == Some("message.bashExecution"))
    );
    assert!(matches!(&events[9], AgentEvent::Unknown(event) if event.native_type.as_deref() == Some("compaction")));
    assert!(matches!(&events[10], AgentEvent::Unknown(event) if event.native_type.as_deref() == Some("future_entry")));

    let AgentEvent::ToolCall(tool) = &events[5] else {
      panic!("expected tool call");
    };
    assert!(matches!(
      tool.summary,
      Some(ToolSummary::FileRead {
        path: Some(ref path)
      }) if path == "README.md"
    ));

    let AgentEvent::ToolCall(result) = &events[7] else {
      panic!("expected tool result");
    };
    assert!(result.input.is_none());
    assert!(matches!(
      result.summary,
      Some(ToolSummary::FileRead {
        path: Some(ref path)
      }) if path == "README.md"
    ));
    let output = result.output.as_ref().expect("tool result should retain output");
    assert_eq!(output["content"][0]["text"], "project readme");
    assert_eq!(output["details"]["path"], "README.md");
  }

  #[test]
  fn tool_result_without_details_keeps_content_as_output() {
    let events = normalize_fixture(
      r#"{"type":"session","id":"pi-session"}
{"type":"message","id":"tool-1","message":{"role":"toolResult","toolCallId":"call-1","toolName":"bash","content":[{"type":"text","text":"hello"}],"isError":false}}"#,
    );

    let AgentEvent::ToolCall(result) = &events[1] else {
      panic!("expected tool result");
    };
    assert_eq!(
      result.output.as_ref(),
      Some(&serde_json::json!([{ "type": "text", "text": "hello" }]))
    );
    assert!(matches!(
      result.summary,
      Some(ToolSummary::Shell {
        command: None,
        cwd: None,
        exit_code: None,
      })
    ));
  }

  #[test]
  fn preserves_redacted_thinking_without_exposing_content_or_hiding_event() {
    let events = normalize_fixture(
      r#"{"type":"session","id":"pi-session"}
{"type":"message","id":"assistant-1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private chain of thought","thinkingSignature":"sig-1","redacted":true}]}}"#,
    );

    let AgentEvent::Reasoning(event) = &events[1] else {
      panic!("expected redacted reasoning");
    };
    assert_eq!(event.redacted, Some(true));
    assert!(event.text.is_none());
    assert_eq!(event.signature.as_deref(), Some("sig-1"));
    assert!(!events[1].is_hidden());

    let serialized = serde_json::to_string(event).expect("reasoning event should serialize");
    assert!(!serialized.contains("private chain of thought"));
  }

  #[test]
  fn emits_an_explicit_event_when_redacted_thinking_has_no_payload() {
    let events = normalize_fixture(
      r#"{"type":"session","id":"pi-session"}
{"type":"message","id":"assistant-1","message":{"role":"assistant","content":[{"type":"thinking","redacted":true}]}}"#,
    );

    assert!(matches!(
      &events[..],
      [AgentEvent::SessionStarted(_), AgentEvent::Reasoning(event)]
        if event.redacted == Some(true) && event.text.is_none() && event.signature.is_none()
    ));
  }

  #[test]
  fn tool_result_preserves_content_block_casing_and_extra_fields() {
    let events = normalize_fixture(
      r#"{"type":"session","id":"pi-session"}
{"type":"message","id":"tool-1","message":{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[{"type":"image","data":"aGVsbG8=","mimeType":"image/png","providerMetadata":{"assetId":"asset-1"}},{"type":"text","text":"caption","annotations":["generated"]}],"details":{"path":"diagram.png"},"isError":false}}"#,
    );

    let AgentEvent::ToolCall(result) = &events[1] else {
      panic!("expected tool result");
    };
    let output = result.output.as_ref().expect("tool result should retain output");
    assert_eq!(output["content"][0]["mimeType"], "image/png");
    assert!(output["content"][0].get("mime_type").is_none());
    assert_eq!(output["content"][0]["providerMetadata"]["assetId"], "asset-1");
    assert_eq!(output["content"][1]["annotations"][0], "generated");
    assert_eq!(output["details"]["path"], "diagram.png");
  }

  #[test]
  fn edit_result_metadata_does_not_become_a_file_path() {
    let events = normalize_fixture(
      r#"{"type":"session","id":"pi-session"}
{"type":"message","id":"assistant-1","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"edit","arguments":{"path":"src/lib.rs","oldText":"before","newText":"after"}}]}}
{"type":"message","id":"tool-1","message":{"role":"toolResult","toolCallId":"call-1","toolName":"edit","content":[{"type":"text","text":"Updated src/lib.rs"}],"details":{"diff":"-before\n+after","patch":"*** Begin Patch","firstChangedLine":1},"isError":false}}"#,
    );

    let AgentEvent::ToolCall(invocation) = &events[1] else {
      panic!("expected edit invocation");
    };
    assert!(matches!(
      invocation.summary,
      Some(ToolSummary::FileEdit {
        path: Some(ref path),
        ..
      }) if path == "src/lib.rs"
    ));

    let AgentEvent::ToolCall(result) = &events[2] else {
      panic!("expected edit result");
    };
    assert_eq!(invocation.tool_call_id, result.tool_call_id);
    assert!(matches!(
      result.summary,
      Some(ToolSummary::FileEdit {
        path: None,
        added: None,
        removed: None,
      })
    ));
  }

  #[test]
  fn keeps_unknown_content_blocks_visible() {
    let events = normalize_fixture(
      r#"{"type":"session","id":"pi-session"}
{"type":"message","id":"assistant-1","message":{"role":"assistant","content":[{"type":"future_block","answer":42}]}}"#,
    );

    let AgentEvent::Unknown(event) = &events[1] else {
      panic!("expected unknown content");
    };
    assert_eq!(event.native_type.as_deref(), Some("message.content.future_block"));
    assert_eq!(
      event.native.as_ref().and_then(|value| value.get("answer")),
      Some(&serde_json::json!(42))
    );
  }

  fn normalize_fixture(input: &str) -> Vec<AgentEvent> {
    let mut normalizer = PiNormalizer::new();
    input
      .lines()
      .filter(|line| !line.trim().is_empty())
      .flat_map(|line| {
        let line: PiSessionLine = serde_json::from_str(line).expect("fixture line should decode");
        normalizer.normalize(line)
      })
      .collect()
  }
}
