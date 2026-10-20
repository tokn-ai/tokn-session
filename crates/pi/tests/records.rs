use serde_json::{Value, json};
use tokn_session_core::{AgentEvent, MessageDelivery, Role, UsageKind};
use tokn_session_pi::normalize::PiNormalizer;

fn normalize(mut record: Value) -> Vec<AgentEvent> {
  record["id"] = json!("entry-1");
  record["parentId"] = Value::Null;
  record["timestamp"] = json!("2026-08-28T00:00:00Z");
  let mut normalizer = PiNormalizer::new();
  normalizer.normalize(serde_json::from_value(json!({"type":"session", "id":"pi-1"})).unwrap());
  normalizer.normalize(serde_json::from_value(record).unwrap())
}

fn usage() -> Value {
  json!({"input":10,"output":5,"cacheRead":20,"cacheWrite":3,"cacheWrite1h":2,
    "reasoning":2,"totalTokens":38,"cost":{"total":0.125},"future_counter":99})
}

#[test]
fn assistant_usage_includes_cache_once_and_preserves_costs() {
  let events = normalize(json!({"type":"message","message":{
    "role":"assistant","content":[{"type":"text","text":"done"}],"usage":usage()
  }}));
  assert_eq!(events.len(), 2);
  let AgentEvent::Usage(event) = &events[1] else {
    panic!("expected usage")
  };
  assert_eq!(event.kind, UsageKind::ModelCall);
  assert_eq!(event.session_id.as_deref(), Some("pi-1"));
  assert_eq!(event.message_id.as_deref(), Some("entry-1"));
  assert_eq!(event.record_id, event.message_id);
  assert_eq!(event.input_tokens, 33);
  assert_eq!(event.output_tokens, 5);
  assert_eq!(event.total_tokens, Some(38));
  assert_eq!(event.reasoning_tokens, Some(2));
  assert_eq!(event.native, usage());
  assert!(event.turn_id.is_none());
}

#[test]
fn empty_assistant_with_usage_is_not_unknown_but_unsupported_content_still_is() {
  let mut record = json!({"type":"message","message":{"role":"assistant","content":[],"usage":usage()}});
  let events = normalize(record.clone());
  assert!(matches!(&events[..], [AgentEvent::Usage(_)]));
  record["message"]["content"] = json!([{"type":"future_block","value":42}]);
  let events = normalize(record);
  assert!(matches!(&events[..], [AgentEvent::Unknown(_), AgentEvent::Usage(_)]));
}

#[test]
fn tool_and_summary_accounting_is_an_operation_total() {
  for record in [
    json!({"type":"message","message":{"role":"toolResult","toolCallId":"call-1",
      "toolName":"delegate","content":[{"type":"text","text":"done"}],"usage":usage()}}),
    json!({"type":"compaction","summary":"summary","firstKeptEntryId":"keep","tokensBefore":100,"usage":usage()}),
    json!({"type":"branch_summary","summary":"summary","fromId":"branch","usage":usage()}),
  ] {
    let is_message = record["type"] == "message";
    let events = normalize(record);
    assert_eq!(events.len(), 2);
    let AgentEvent::Usage(event) = &events[1] else {
      panic!("expected operation accounting")
    };
    assert_eq!(event.kind, UsageKind::OperationTotal);
    assert_eq!(event.message_id.is_some(), is_message);
    assert_eq!(event.record_id.as_deref(), Some("entry-1"));
    assert_eq!(event.native, usage());
  }
}

#[test]
fn recognized_metadata_preserves_native_without_conversation_text() {
  for record in [
    json!({"type":"custom","customType":"extension/state","data":{"opaque":true}}),
    json!({"type":"label","targetId":"target","label":"label"}),
    json!({"type":"label","targetId":"target"}),
    json!({"type":"session_info","name":"title"}),
    json!({"type":"session_info"}),
    json!({"type":"leaf","targetId":"leaf"}),
    json!({"type":"active_tools_change","activeToolNames":["read","write"]}),
  ] {
    let events = normalize(record.clone());
    let [AgentEvent::Metadata(event)] = &events[..] else {
      panic!("expected metadata for {record}")
    };
    assert_eq!(event.native_type, record["type"].as_str().unwrap());
    for (key, value) in record.as_object().unwrap() {
      assert_eq!(&event.native[key], value);
    }
  }
}

#[test]
fn custom_messages_have_extension_provenance_and_visibility_not_a_human_role() {
  for display in [false, true] {
    let events = normalize(json!({"type":"custom_message","customType":"plugin/status","content":[
      {"type":"text","text":"extension content"},{"type":"image","mimeType":"image/png","data":"abc"}
    ],"display":display,"details":{"origin":"hook"}}));
    let [AgentEvent::Message(event)] = &events[..] else {
      panic!("expected extension message")
    };
    assert!(matches!(event.role, Role::System));
    assert!(matches!(event.delivery, MessageDelivery::Unspecified));
    assert_eq!(event.text, "extension content");
    assert_eq!(event.images.len(), 1);
    assert_eq!(event.images[0].mime_type.as_deref(), Some("image/png"));
    assert!(matches!(&event.images[0].source, tokn_session_core::MessageImageSource::Base64 { data } if data == "abc"));
    let provenance = event.provenance.as_ref().unwrap();
    assert_eq!(provenance.display, Some(display));
    assert_eq!(provenance.source["custom_type"], "plugin/status");
    assert_eq!(provenance.native.as_ref().unwrap()["details"]["origin"], "hook");
    assert_eq!(events[0].is_hidden(), !display);
  }
}

#[test]
fn malformed_and_future_records_remain_unknown_even_when_hidden() {
  for record in [
    json!({"type":"compaction","summary":"missing required fields"}),
    json!({"type":"custom"}),
    json!({"type":"branch_summary","fromId":"branch"}),
    json!({"type":"label","label":123}),
    json!({"type":"active_tools_change"}),
    json!({"type":"custom_message","customType":"plugin","display":false,"content":[{"type":"future_block"}]}),
    json!({"type":"future_entry","value":42}),
  ] {
    let hidden = record["display"] == false;
    let events = normalize(record.clone());
    assert!(matches!(&events[..], [AgentEvent::Unknown(_)]), "{record}");
    assert_eq!(events[0].is_hidden(), hidden);
  }
}

#[test]
fn invalid_usage_does_not_swallow_readable_message() {
  for counters in [
    json!({"input":-1,"output":5}),
    json!({"input":1,"output":"5"}),
    json!({"input":u64::MAX,"output":5,"cacheRead":1}),
  ] {
    let events = normalize(json!({"type":"message","message":{"role":"assistant",
      "content":[{"type":"text","text":"still readable"}],"usage":counters}}));
    assert!(matches!(&events[..], [AgentEvent::Message(_), AgentEvent::Unknown(_)]));
  }
}

#[test]
fn compaction_is_a_context_checkpoint_not_a_reply() {
  let events = normalize(
    json!({"type":"compaction","summary":"private context","firstKeptEntryId":"keep",
    "tokensBefore":200,"fromHook":true,"details":{"plugin":"custom"}}),
  );
  let [AgentEvent::Compaction(event)] = &events[..] else {
    panic!("expected context")
  };
  assert_eq!(event.state, tokn_session_core::CompactionState::Completed);
  assert_eq!(event.summary.as_deref(), Some("private context"));
  assert_eq!(event.context.first_kept_entry_id.as_deref(), Some("keep"));
  assert_eq!(event.measurements[0].tokens, 200);
  assert_eq!(event.measurements[0].estimated, None);
}

#[test]
fn compaction_fixture_preserves_the_earlier_conversation() {
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compaction.jsonl");
  let session = tokn_session_pi::PiSessionSource::new(None)
    .load_session_path(&path)
    .unwrap();
  assert!(matches!(&session.events[1], AgentEvent::Message(e) if e.text == "Keep this conversation visible."));
  assert!(matches!(&session.events[2], AgentEvent::Compaction(e) if e.compaction_id.as_deref() == Some("compact-1")));
}
