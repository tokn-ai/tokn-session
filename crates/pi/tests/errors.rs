use serde_json::{Value, json};
use tokn_session_core::{AgentEvent, MetadataKind, Provider, Role};
use tokn_session_pi::{
  PiSessionSource,
  event::{PiMessage, PiSessionItem, PiSessionLine},
  normalize::PiNormalizer,
};

fn normalize(message: Value) -> (Value, Vec<AgentEvent>) {
  let native = json!({
    "type":"message", "id":"assistant-1", "parentId":"user-1",
    "timestamp":"2026-10-19T00:00:02.000Z", "message":message,
  });
  let mut normalizer = PiNormalizer::new();
  normalizer.normalize(serde_json::from_value(json!({"type":"session", "id":"pi-errors"})).unwrap());
  let events = normalizer.normalize(serde_json::from_value(native.clone()).unwrap());
  (native, events)
}

#[test]
fn connection_errors_remain_visible_in_history_and_incremental_normalization() {
  let fixture = include_str!("fixtures/assistant_errors.jsonl");
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/assistant_errors.jsonl");
  let loaded = PiSessionSource::new(None).load_session_path(&path).unwrap();
  let mut incremental = PiNormalizer::new();
  let mut live_events = Vec::new();
  for (index, line) in fixture.lines().enumerate() {
    let native: Value = serde_json::from_str(line).unwrap();
    let decoded: PiSessionLine = serde_json::from_str(line).unwrap();
    assert_eq!(decoded.native(), &native);
    if index >= 2 {
      // The fixture exercises the typed assistant contract, not a recovered
      // Unknown payload that happens to contain error-looking field names.
      let PiSessionItem::Message(item) = decoded.item() else {
        panic!("expected message")
      };
      let Some(PiMessage::Assistant(message)) = &item.message else {
        panic!("expected typed assistant")
      };
      assert!(message.content.is_empty());
      assert_eq!(message.stop_reason.as_deref(), Some("error"));
      assert_eq!(message.error_message.as_deref(), Some("Connection error."));
    }
    let events = incremental.normalize(decoded);
    if index >= 2 {
      let [
        AgentEvent::Error(error),
        AgentEvent::Metadata(diagnostic),
        AgentEvent::Usage(usage),
      ] = &events[..]
      else {
        panic!("failed empty assistant must retain error, correlation, and usage: {events:?}")
      };
      assert!(matches!(error.provider, Provider::Pi));
      assert_eq!(error.session_id.as_deref(), Some("pi-errors"));
      assert_eq!(error.message, "Connection error.");
      assert_eq!(error.timestamp.as_deref(), native["timestamp"].as_str());
      assert!(matches!(diagnostic.kind, MetadataKind::Diagnostic));
      assert_eq!(diagnostic.native, native);
      assert_eq!(usage.message_id.as_deref(), native["id"].as_str());
      assert_eq!(usage.record_id, usage.message_id);
      assert_eq!(usage.native, native["message"]["usage"]);
      assert_eq!(usage.total_tokens, Some(0));
    }
    live_events.extend(events);
  }
  assert_eq!(loaded.reference.id, "pi-errors");
  assert_eq!(
    loaded
      .events
      .iter()
      .filter(|event| matches!(event, AgentEvent::Error(_)))
      .count(),
    4
  );
  assert!(
    !loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Unknown(_)))
  );
  assert_eq!(
    serde_json::to_value(loaded.events).unwrap(),
    serde_json::to_value(live_events).unwrap()
  );
}

#[test]
fn failed_response_without_usage_retains_the_exact_native_message() {
  let (native, events) = normalize(json!({
    "role":"assistant", "content":[], "provider":"zai", "model":"glm-5.2",
    "stopReason":"error", "errorMessage":"Connection error.", "diagnostics":{"retry":4},
  }));
  let [AgentEvent::Error(error), AgentEvent::Metadata(diagnostic)] = &events[..] else {
    panic!("expected visible error and native diagnostic")
  };
  assert_eq!(error.message, "Connection error.");
  assert_eq!(diagnostic.native, native);
  assert_eq!(diagnostic.native["id"], "assistant-1");
  assert_eq!(diagnostic.native["parentId"], "user-1");
  assert_eq!(diagnostic.native["message"]["diagnostics"]["retry"], 4);
}

#[test]
fn explicit_failure_has_a_readable_fallback_when_error_text_is_absent_or_blank() {
  for error_message in [Value::Null, json!(""), json!(" \n\t ")] {
    let (_, events) = normalize(json!({
      "role":"assistant", "content":[], "stopReason":"error", "errorMessage":error_message,
    }));
    assert!(
      matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(_)]
      if error.message == "Pi assistant response failed")
    );
  }
}

#[test]
fn explicit_error_text_is_not_lost_when_stop_reason_is_missing() {
  let (_, events) = normalize(json!({
    "role":"assistant", "content":[], "errorMessage":"Provider rejected this request.",
  }));
  assert!(
    matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(_)]
    if error.message == "Provider rejected this request.")
  );
}

#[test]
fn partial_response_and_usage_survive_a_model_failure() {
  let (native, events) = normalize(json!({
    "role":"assistant", "content":[{"type":"text","text":"A partial reply."}],
    "stopReason":"error", "errorMessage":"Connection interrupted.",
    "usage":{"input":10,"output":3,"cacheRead":2,"cacheWrite":0,"totalTokens":15,"cost":{"total":0.01}},
  }));
  let [
    AgentEvent::Message(message),
    AgentEvent::Error(error),
    AgentEvent::Metadata(diagnostic),
    AgentEvent::Usage(usage),
  ] = &events[..]
  else {
    panic!("expected content, failure, native correlation, and usage")
  };
  assert!(matches!(message.role, Role::Assistant));
  assert_eq!(message.text, "A partial reply.");
  assert_eq!(message.message_id.as_deref(), Some("assistant-1"));
  assert_eq!(message.parent_id.as_deref(), Some("user-1"));
  assert_eq!(error.message, "Connection interrupted.");
  assert_eq!(diagnostic.native, native);
  assert_eq!(usage.message_id, message.message_id);
  assert_eq!(usage.input_tokens, 12);
  assert_eq!(usage.output_tokens, 3);
  assert_eq!(usage.native, native["message"]["usage"]);
}

#[test]
fn abort_is_a_correlated_cancellation_diagnostic_instead_of_a_model_error() {
  for content in [json!([]), json!([{"type":"text","text":"Partial before Stop."}])] {
    let (native, events) = normalize(json!({
      "role":"assistant", "content":content, "stopReason":"aborted", "errorMessage":"Request aborted.",
      "usage":{"input":10,"output":1,"totalTokens":11},
    }));
    assert!(!events.iter().any(|event| matches!(
      event,
      AgentEvent::Error(_) | AgentEvent::Unknown(_) | AgentEvent::Lifecycle(_)
    )));
    let diagnostic = events
      .iter()
      .find_map(|event| match event {
        AgentEvent::Metadata(event) => Some(event),
        _ => None,
      })
      .unwrap();
    assert!(matches!(diagnostic.kind, MetadataKind::Diagnostic));
    assert_eq!(diagnostic.summary, "Pi assistant response cancelled");
    assert_eq!(diagnostic.native, native);
    assert!(
      matches!(events.last(), Some(AgentEvent::Usage(usage)) if usage.message_id.as_deref() == Some("assistant-1"))
    );
    if !content.as_array().unwrap().is_empty() {
      assert!(matches!(events.first(), Some(AgentEvent::Message(message)) if message.text == "Partial before Stop."));
    }
  }
  let (_, events) = normalize(json!({"role":"assistant", "content":[], "stopReason":"aborted"}));
  assert!(matches!(&events[..], [AgentEvent::Metadata(event)] if event.summary == "Pi assistant response cancelled"));
}

#[test]
fn ordinary_stop_reasons_and_blank_error_fields_do_not_manufacture_failures() {
  for stop_reason in [
    Value::Null,
    json!("stop"),
    json!("length"),
    json!("toolUse"),
    json!("future_reason"),
  ] {
    let (_, events) = normalize(json!({
      "role":"assistant", "content":[{"type":"text","text":"Normal reply."}],
      "stopReason":stop_reason, "errorMessage":" \t ",
    }));
    assert!(matches!(&events[..], [AgentEvent::Message(message)] if message.text == "Normal reply."));
  }
}

#[test]
fn failures_do_not_hide_unsupported_content_or_invalid_accounting() {
  let (_, events) = normalize(json!({
    "role":"assistant", "content":[{"type":"future_block","value":42}],
    "stopReason":"error", "errorMessage":"Connection error.", "usage":{"input":-1,"output":0},
  }));
  let [
    AgentEvent::Unknown(content),
    AgentEvent::Error(_),
    AgentEvent::Metadata(_),
    AgentEvent::Unknown(usage),
  ] = &events[..]
  else {
    panic!("unknown content/accounting must remain visible")
  };
  assert_eq!(content.native.as_ref().unwrap()["value"], 42);
  assert_eq!(usage.native.as_ref().unwrap()["input"], -1);
}
