use serde_json::Value;
use tokn_opencode_protocol::run::{RunEvent, RunLine};
use tokn_session_core::{
  AgentEvent, LiveSessionEvent, MessageDelivery, MessageEvent, Phase, Provider, ReasoningEvent, Role, UnknownEvent,
};

use crate::normalize::{timestamp, tool_event, usage_event};

pub struct OpenCodeLiveNormalizer;

impl OpenCodeLiveNormalizer {
  pub fn normalize_line(line: &str) -> Result<Vec<LiveSessionEvent>, String> {
    if line.trim().is_empty() {
      return Ok(Vec::new());
    }

    let line: RunLine = serde_json::from_str(line).map_err(|err| format!("invalid opencode live json: {err}"))?;
    let session_id = line.session_id().map(str::to_string);
    let raw_timestamp = line.timestamp();
    let (event, native) = line.into_parts();

    Ok(match event {
      RunEvent::Text(part) => vec![wrap_agent_event(AgentEvent::Message(MessageEvent {
        provenance: None,
        provider: Provider::OpenCode,
        session_id,
        message_id: part.identity.message_id.or(part.identity.id),
        parent_id: None,
        role: Role::Assistant,
        delivery: MessageDelivery::Final,
        phase: Phase::Finished,
        text: part.text,
        timestamp: timestamp(raw_timestamp),
      }))],
      RunEvent::Reasoning(part) => vec![wrap_agent_event(AgentEvent::Reasoning(ReasoningEvent {
        provenance: None,
        provider: Provider::OpenCode,
        session_id,
        message_id: part.identity.message_id.or(part.identity.id),
        parent_id: None,
        phase: Phase::Finished,
        text: Some(part.text),
        summary: None,
        redacted: None,
        encrypted_content: None,
        signature: None,
        timestamp: timestamp(raw_timestamp),
      }))],
      RunEvent::ToolUse(part) => {
        let session_id = session_id.unwrap_or_default();
        let message_id = part.identity.message_id.unwrap_or_else(|| "live".to_string());
        vec![wrap_agent_event(tool_event(
          Provider::OpenCode,
          session_id,
          message_id,
          None,
          part.call_id.or(part.identity.id),
          part.tool,
          part.state,
          raw_timestamp,
        ))]
      }
      RunEvent::Error(error) => match crate::errors::normalize(
        Provider::OpenCode,
        session_id.clone(),
        &error.error,
        "error",
        native.clone(),
        timestamp(raw_timestamp),
      ) {
        Some(events) => events.into_iter().map(wrap_agent_event).collect(),
        None => vec![unknown_live_event(
          session_id,
          Some("error".into()),
          native,
          raw_timestamp,
        )],
      },
      RunEvent::StepStart(_) => vec![unknown_live_event(
        session_id,
        Some("step_start".to_string()),
        native,
        raw_timestamp,
      )],
      RunEvent::StepFinish(part) => {
        let native_usage = native.get("part").and_then(|part| part.get("tokens")).cloned();
        let usage = native_usage
          .zip(part.tokens.as_ref())
          .and_then(|(native_usage, tokens)| {
            let step_id = part.identity.id.clone();
            usage_event(
              session_id.clone(),
              part.identity.message_id.clone(),
              step_id.clone(),
              step_id,
              tokens,
              native_usage,
              timestamp(raw_timestamp),
            )
          });
        match usage {
          Some(event) => vec![wrap_agent_event(event)],
          // A live `step_finish` used to be emitted as an unknown event. Keep
          // that lossless fallback for missing or invalid accounting rather
          // than fabricating a zero-valued usage card.
          None => vec![unknown_live_event(
            session_id,
            Some("step_finish".to_string()),
            native,
            raw_timestamp,
          )],
        }
      }
      RunEvent::Unknown(item) => vec![unknown_live_event(session_id, item.native_type, native, raw_timestamp)],
    })
  }
}

fn wrap_agent_event(event: AgentEvent) -> LiveSessionEvent {
  LiveSessionEvent::Event(event)
}

fn unknown_live_event(
  session_id: Option<String>,
  native_type: Option<String>,
  native: Value,
  raw_timestamp: Option<i64>,
) -> LiveSessionEvent {
  LiveSessionEvent::Unknown(UnknownEvent {
    provider: Provider::OpenCode,
    session_id,
    native_type,
    native: Some(native),
    timestamp: timestamp(raw_timestamp),
  })
}

#[cfg(test)]
mod tests {
  use tokn_session_core::{AgentEvent, LiveSessionEvent, MessageDelivery, ToolSummary, UsageKind};

  use super::OpenCodeLiveNormalizer;

  #[test]
  fn normalizes_text_line_to_live_agent_event() {
    let events = OpenCodeLiveNormalizer::normalize_line(
      r#"{"type":"text","timestamp":1710000000000,"sessionID":"ses_123","part":{"id":"part_1","sessionID":"ses_123","messageID":"msg_1","type":"text","text":"done","time":{"end":1710000000000}}}"#,
    )
    .unwrap();

    assert_eq!(events.len(), 1);
    let LiveSessionEvent::Event(AgentEvent::Message(message)) = &events[0] else {
      panic!("expected live message event");
    };
    assert_eq!(message.session_id.as_deref(), Some("ses_123"));
    assert_eq!(message.message_id.as_deref(), Some("msg_1"));
    assert!(matches!(message.delivery, MessageDelivery::Final));
    assert_eq!(message.text, "done");
    assert_eq!(message.timestamp.as_deref(), Some("1710000000000"));
  }

  #[test]
  fn normalizes_tool_use_line_to_semantic_tool_event() {
    let events = OpenCodeLiveNormalizer::normalize_line(
      r#"{"type":"tool_use","timestamp":1710000000001,"sessionID":"ses_123","part":{"id":"tool_1","sessionID":"ses_123","messageID":"msg_2","type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"cargo test"},"output":"ok","metadata":{"exit":0}}}}"#,
    )
    .unwrap();

    assert_eq!(events.len(), 1);
    let LiveSessionEvent::Event(AgentEvent::ToolCall(tool)) = &events[0] else {
      panic!("expected live tool event");
    };
    assert_eq!(tool.session_id.as_deref(), Some("ses_123"));
    assert_eq!(tool.message_id.as_deref(), Some("msg_2"));
    assert_eq!(tool.tool_call_id.as_deref(), Some("tool_1"));
    assert_eq!(tool.is_error, Some(false));
    let Some(ToolSummary::Shell { command, exit_code, .. }) = &tool.summary else {
      panic!("expected shell summary");
    };
    assert_eq!(command.as_deref(), Some("cargo test"));
    assert_eq!(*exit_code, Some(0));
  }

  #[test]
  fn normalizes_valid_step_finish_usage() {
    let tokens = serde_json::json!({
      "input": 10,
      "output": 11,
      "reasoning": 12,
      "cache": {"read": 13, "write": 14},
      "total": 999
    });
    let events = OpenCodeLiveNormalizer::normalize_line(&format!(
      r#"{{"type":"step_finish","timestamp":1710000000002,"sessionID":"ses_123","part":{{"id":"prt_step","sessionID":"ses_123","messageID":"msg_2","type":"step-finish","tokens":{tokens}}}}}"#
    ))
    .unwrap();

    assert_eq!(events.len(), 1);
    let LiveSessionEvent::Event(AgentEvent::Usage(usage)) = &events[0] else {
      panic!("expected live usage event");
    };
    assert_eq!(usage.kind, UsageKind::ModelCall);
    assert_eq!(usage.session_id.as_deref(), Some("ses_123"));
    assert_eq!(usage.message_id.as_deref(), Some("msg_2"));
    assert_eq!(usage.step_id.as_deref(), Some("prt_step"));
    assert_eq!(usage.record_id.as_deref(), Some("prt_step"));
    assert_eq!(usage.input_tokens, 37);
    assert_eq!(usage.output_tokens, 11);
    assert_eq!(usage.cache_read_tokens, Some(13));
    assert_eq!(usage.cache_write_tokens, Some(14));
    assert_eq!(usage.reasoning_tokens, Some(12));
    assert_eq!(usage.total_tokens, Some(999));
    assert_eq!(usage.native, tokens);
    assert_eq!(usage.timestamp.as_deref(), Some("1710000000002"));
  }

  #[test]
  fn keeps_missing_or_invalid_step_finish_usage_unknown() {
    for line in [
      r#"{"type":"step_finish","timestamp":1710000000002,"sessionID":"ses_123","part":{"id":"prt_missing","type":"step-finish"}}"#,
      r#"{"type":"step_finish","timestamp":1710000000003,"sessionID":"ses_123","part":{"id":"prt_invalid","type":"step-finish","tokens":{"input":-1,"output":2,"reasoning":0,"cache":{"read":0,"write":0}}}}"#,
    ] {
      let events = OpenCodeLiveNormalizer::normalize_line(line).unwrap();
      let LiveSessionEvent::Unknown(event) = &events[0] else {
        panic!("expected unknown live event");
      };
      assert_eq!(event.native_type.as_deref(), Some("step_finish"));
      assert_eq!(event.session_id.as_deref(), Some("ses_123"));
      assert!(event.native.is_some());
    }
  }

  #[test]
  fn preserves_step_and_unknown_live_lines() {
    for (line, expected_type) in [
      (
        r#"{"type":"step_start","timestamp":1710000000002,"sessionID":"ses_123","part":{"id":"step_1","type":"step-start"}}"#,
        "step_start",
      ),
      (
        r#"{"type":"future_event","timestamp":1710000000003,"sessionID":"ses_123","answer":42}"#,
        "future_event",
      ),
    ] {
      let events = OpenCodeLiveNormalizer::normalize_line(line).unwrap();
      let LiveSessionEvent::Unknown(event) = &events[0] else {
        panic!("expected unknown live event");
      };
      assert_eq!(event.session_id.as_deref(), Some("ses_123"));
      assert_eq!(event.native_type.as_deref(), Some(expected_type));
      assert!(event.native.is_some());
    }
  }

  #[test]
  fn preserves_malformed_known_live_line() {
    let events = OpenCodeLiveNormalizer::normalize_line(
      r#"{"type":"text","timestamp":1710000000004,"sessionID":"ses_123","part":{"type":"text","text":42}}"#,
    )
    .unwrap();

    let LiveSessionEvent::Unknown(event) = &events[0] else {
      panic!("expected malformed line to remain visible");
    };
    assert_eq!(event.native_type.as_deref(), Some("text"));
    assert_eq!(
      event.native.as_ref().and_then(|native| native.pointer("/part/text")),
      Some(&serde_json::json!(42))
    );
  }
}
