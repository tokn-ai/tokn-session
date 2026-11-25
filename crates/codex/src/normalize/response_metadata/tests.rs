use crate::normalize::CodexNormalizer;
use serde_json::{Value, json};
use tokn_session_core::{AgentEvent, MessageEvent, ModelOrigin};

fn push(normalizer: &mut CodexNormalizer, value: Value) -> Vec<AgentEvent> {
  normalizer.normalize(serde_json::from_value(value).unwrap())
}

fn context(turn: &str, model: Value) -> Value {
  json!({"type":"turn_context","payload":{"turn_id":turn,"model":model}})
}

fn start(normalizer: &mut CodexNormalizer, turn: &str, model: &str) {
  push(
    normalizer,
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
  );
  push(normalizer, context(turn, json!(model)));
}

fn normalizer(paginated: bool) -> CodexNormalizer {
  let mut normalizer = CodexNormalizer::new();
  let mut payload = json!({"id":"owner","model_provider":"provider"});
  if paginated {
    payload["history_mode"] = json!("paginated");
  }
  push(&mut normalizer, json!({"type":"session_meta","payload":payload}));
  normalizer
}

fn raw_message(normalizer: &mut CodexNormalizer, turn: Value) -> MessageEvent {
  let mut payload = json!({"type":"message","role":"assistant","id":"message","phase":"final_answer",
    "content":[{"type":"output_text","text":"answer"}]});
  if !turn.is_null() {
    payload["internal_chat_message_metadata_passthrough"] = json!({"turn_id":turn});
  }
  let events = push(normalizer, json!({"type":"response_item","payload":payload}));
  let [AgentEvent::Message(message)] = events.as_slice() else {
    panic!("expected assistant message: {events:?}")
  };
  message.clone()
}

fn canonical(normalizer: &mut CodexNormalizer, session: &str, turn: &str, kind: &str) -> MessageEvent {
  let events = push(
    normalizer,
    json!({"type":"event_msg","payload":{
      "type":kind,"thread_id":session,"turn_id":turn,"item":{
        "type":"AgentMessage","id":"canonical","content":[{"type":"Text","text":"answer"}],"phase":"final_answer"
      }
    }}),
  );
  let [AgentEvent::Message(message)] = events.as_slice() else {
    panic!("expected canonical message: {events:?}")
  };
  message.clone()
}

#[test]
fn exact_turn_context_is_request_metadata_and_persistent_tier_is_not_response_fast() {
  let mut normalizer = normalizer(false);
  push(
    &mut normalizer,
    json!({"type":"event_msg","payload":{"type":"thread_settings_applied",
    "thread_id":"owner","thread_settings":{"model":"initial","model_provider_id":"provider","service_tier":"priority"}}}),
  );
  start(&mut normalizer, "turn-one", "requested-model");
  // The native initial context is immediately followed by a fresh persistent
  // checkpoint. Its model/tier can differ from this already-captured turn.
  push(
    &mut normalizer,
    json!({"type":"event_msg","payload":{"type":"thread_settings_applied",
      "thread_id":"owner","thread_settings":{"model":"next-model","model_provider_id":"next-provider","service_tier":null}}}),
  );
  push(
    &mut normalizer,
    json!({"type":"response_item","payload":{"type":"message","role":"user",
    "content":[{"type":"input_text","text":"prompt"}],"internal_chat_message_metadata_passthrough":{"turn_id":"turn-one"}}}),
  );
  let message = raw_message(&mut normalizer, json!("turn-one"));
  let metadata = message.response_metadata.unwrap();
  assert!(metadata.fast_mode.is_none());
  let model = metadata.model.unwrap();
  assert_eq!(model.origin, ModelOrigin::Request);
  assert_eq!(model.model.model_id, "requested-model");
  assert!(
    model.model.model_provider.is_none(),
    "the turn does not record a provider"
  );
}

#[test]
fn switches_missing_context_and_explicit_clears_do_not_reuse_previous_labels() {
  let mut normalizer = normalizer(false);
  start(&mut normalizer, "one", "model-a");
  assert!(raw_message(&mut normalizer, json!("one")).response_metadata.is_some());
  push(
    &mut normalizer,
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"two"}}),
  );
  assert!(raw_message(&mut normalizer, json!("two")).response_metadata.is_none());
  push(&mut normalizer, context("two", json!("model-b")));
  assert_eq!(
    raw_message(&mut normalizer, json!("two"))
      .response_metadata
      .unwrap()
      .model
      .unwrap()
      .model
      .model_id,
    "model-b"
  );
  for clear in [Value::Null, json!(" "), json!("x".repeat(513)), json!(123)] {
    push(&mut normalizer, context("two", json!("model-b")));
    push(&mut normalizer, context("two", clear));
    assert!(raw_message(&mut normalizer, json!("two")).response_metadata.is_none());
  }
}

#[test]
fn missing_malformed_or_mismatched_turn_ids_never_use_physical_adjacency() {
  let mut normalizer = normalizer(false);
  start(&mut normalizer, "one", "model-a");
  for turn in [Value::Null, json!(123), json!("other")] {
    assert!(raw_message(&mut normalizer, turn).response_metadata.is_none());
  }
  assert!(raw_message(&mut normalizer, json!("one")).response_metadata.is_some());
  push(
    &mut normalizer,
    json!({"type":"turn_context","payload":{"model":"unscoped"}}),
  );
  assert!(raw_message(&mut normalizer, json!("one")).response_metadata.is_none());
}

#[test]
fn canonical_stream_phases_require_exact_owner_and_turn() {
  let mut normalizer = normalizer(true);
  start(&mut normalizer, "one", "model-a");
  let started = canonical(&mut normalizer, "owner", "one", "item_started");
  let completed = canonical(&mut normalizer, "owner", "one", "item_completed");
  assert!(started.response_metadata.is_some());
  assert_eq!(started.response_metadata, completed.response_metadata);
  assert_eq!(started.message_id, completed.message_id);
  assert!(
    canonical(&mut normalizer, "foreign", "one", "item_completed")
      .response_metadata
      .is_none()
  );
  assert!(
    canonical(&mut normalizer, "owner", "other", "item_completed")
      .response_metadata
      .is_none()
  );
  push(
    &mut normalizer,
    json!({"type":"session_meta","payload":{"id":"foreign"}}),
  );
  start(&mut normalizer, "one", "foreign-model");
  assert!(
    canonical(&mut normalizer, "foreign", "one", "item_completed")
      .response_metadata
      .is_none()
  );
}

#[test]
fn unrelated_settings_do_not_replace_turn_context_and_terminal_records_clear_it() {
  let mut normalizer = normalizer(false);
  start(&mut normalizer, "one", "model-a");
  push(
    &mut normalizer,
    json!({"type":"event_msg","payload":{"type":"thread_settings_applied",
    "thread_id":"foreign","thread_settings":{"model":"foreign"}}}),
  );
  assert!(raw_message(&mut normalizer, json!("one")).response_metadata.is_some());
  push(
    &mut normalizer,
    json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
  );
  assert!(raw_message(&mut normalizer, json!("one")).response_metadata.is_some());
  for settings in [
    json!({"type":"event_msg","payload":{"type":"thread_settings_applied","thread_id":"owner","thread_settings":{"model":"other"}}}),
    json!({"type":"world_state","payload":{"full":false,"state":{"model":"other"}}}),
  ] {
    push(&mut normalizer, settings);
    assert_eq!(
      raw_message(&mut normalizer, json!("one"))
        .response_metadata
        .unwrap()
        .model
        .unwrap()
        .model
        .model_id,
      "model-a"
    );
  }
  for clear in [
    json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"one"}}),
    json!({"type":"compacted","payload":{"message":"checkpoint"}}),
  ] {
    push(&mut normalizer, clear);
    assert!(raw_message(&mut normalizer, json!("one")).response_metadata.is_none());
    start(&mut normalizer, "one", "model-a");
  }
}
