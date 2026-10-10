//! Compile the exhaustive match and full struct literal supported by 0.1.0.
//! New wire formats must not force existing consumers to rewrite either.
use serde_json::{Map, Value, json};
use tokn_codex_protocol::{RolloutItem, RolloutLine, SessionMetaItem};

fn published_item_kind(item: &RolloutItem) -> &str {
  match item {
    RolloutItem::SessionMeta(_) => "session_meta",
    RolloutItem::ResponseItem(_) => "response_item",
    RolloutItem::InterAgentCommunication(_) => "inter_agent_communication",
    RolloutItem::InterAgentCommunicationMetadata(_) => "inter_agent_communication_metadata",
    RolloutItem::Compacted(_) => "compacted",
    RolloutItem::TurnContext(_) => "turn_context",
    RolloutItem::WorldState(_) => "world_state",
    RolloutItem::EventMessage(_) => "event_msg",
    RolloutItem::Unknown(_) => "unknown",
  }
}

#[test]
fn published_enum_and_session_metadata_literals_remain_compatible() {
  let metadata = SessionMetaItem {
    id: Some("thread".into()),
    timestamp: None,
    cwd: None,
    model_provider: None,
    parent_thread_id: None,
    source: None,
    git: None,
    extra: Map::new(),
  };
  assert_eq!(
    published_item_kind(&RolloutItem::SessionMeta(metadata.clone())),
    "session_meta"
  );
  assert_eq!(metadata.history_mode().unwrap(), None);
  assert_eq!(metadata.history_base().unwrap(), None);

  let line: RolloutLine = serde_json::from_value(json!({
    "type":"token_usage_record","payload":{"response_id":"response"}
  }))
  .unwrap();
  assert_eq!(published_item_kind(line.item()), "unknown");
  assert_eq!(
    line.token_usage_record().unwrap().response_id.as_deref(),
    Some("response")
  );
  assert_eq!(published_item_kind(&line.into_item()), "unknown");
}

#[test]
fn history_accessors_validate_extensions_without_losing_native_metadata() {
  let payload = json!({"id":"thread","history_mode":"paginated","history_base":{
    "thread_id":"thread","end_ordinal_exclusive":42,"end_byte_offset":1234,"future":true
  }});
  let mut metadata: SessionMetaItem = serde_json::from_value(payload.clone()).unwrap();
  assert_eq!(metadata.history_mode().unwrap(), Some("paginated"));
  assert_eq!(metadata.history_base().unwrap().unwrap().extra["future"], true);
  assert_eq!(metadata.extra["history_mode"], payload["history_mode"]);
  assert_eq!(metadata.extra["history_base"], payload["history_base"]);

  for invalid in [json!(false), json!(3), json!({})] {
    metadata.extra.insert("history_mode".into(), invalid);
    assert!(metadata.history_mode().is_err());
  }
  metadata.extra.insert("history_mode".into(), Value::Null);
  assert_eq!(metadata.history_mode().unwrap(), None);
  for invalid in [
    json!(false),
    json!(3),
    json!({}),
    json!({
      "thread_id":"thread","end_ordinal_exclusive":"42","end_byte_offset":1234
    }),
  ] {
    metadata.extra.insert("history_base".into(), invalid);
    assert!(metadata.history_base().is_err());
  }
  metadata.extra.insert("history_base".into(), Value::Null);
  assert_eq!(metadata.history_base().unwrap(), None);
}

#[test]
fn malformed_history_envelopes_remain_unknown_and_lossless() {
  for (field, invalid) in [
    ("history_mode", json!(false)),
    ("history_base", json!({"thread_id":"thread"})),
  ] {
    let mut native = json!({"type":"session_meta","payload":{"id":"thread"}});
    native["payload"][field] = invalid;
    let line: RolloutLine = serde_json::from_value(native.clone()).unwrap();
    let RolloutItem::Unknown(item) = line.item() else {
      panic!("malformed history metadata must remain unknown");
    };
    assert!(item.parse_error.is_some());
    assert_eq!(serde_json::to_value(line).unwrap(), native);
  }
}

#[test]
fn accounting_accessors_ignore_unrelated_and_malformed_records() {
  for native in [
    json!({"type":"world_state","payload":{"full":true,"state":{}}}),
    json!({"type":"event_msg","payload":{"type":"token_usage_record"}}),
    json!({"type":"token_usage_record","payload":{"usage":{"total_tokens":5}}}),
  ] {
    let line: RolloutLine = serde_json::from_value(native.clone()).unwrap();
    assert!(line.token_usage_record().is_none());
    assert_eq!(serde_json::to_value(line).unwrap(), native);
  }
}
