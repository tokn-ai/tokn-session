use serde_json::{Value, json};
use tokn_session_codex::normalize::CodexNormalizer;
use tokn_session_core::{AgentEvent, MetadataKind, UsageKind};

fn line(normalizer: &mut CodexNormalizer, native: Value) -> Vec<AgentEvent> {
  normalizer.normalize(serde_json::from_value(native).unwrap())
}

fn normalizer() -> CodexNormalizer {
  let mut normalizer = CodexNormalizer::new();
  line(
    &mut normalizer,
    json!({"type":"session_meta","payload":{"id":"codex-1"}}),
  );
  normalizer
}

fn paginated_normalizer() -> CodexNormalizer {
  let mut normalizer = CodexNormalizer::new();
  line(
    &mut normalizer,
    json!({"type":"session_meta","payload":{"id":"codex-1","history_mode":"paginated"}}),
  );
  normalizer
}

#[test]
fn compaction_checkpoint_and_notice_are_one_operation_without_a_reply() {
  for canonical in [false, true] {
    let mut normalizer = if canonical {
      paginated_normalizer()
    } else {
      normalizer()
    };
    let mut events = line(
      &mut normalizer,
      json!({"type":"compacted","ordinal":10,"payload":{
        "message":"retained context", "replacement_history":[{"type":"compaction","encrypted_content":"opaque"}],
        "window_id":"window-2", "previous_window_id":"window-1", "window_number":2
      }}),
    );
    events.extend(line(&mut normalizer, token_usage_record()));
    events.extend(line(&mut normalizer, token_count(35)));
    let notice = if canonical {
      json!({"type":"item_completed","thread_id":"codex-1","turn_id":"turn-1",
        "item":{"type":"ContextCompaction","id":"compact-1"},"completed_at_ms":1})
    } else {
      json!({"type":"context_compacted"})
    };
    events.extend(line(&mut normalizer, json!({"type":"event_msg","payload":notice})));
    let operations = tokn_session_core::compaction_operations(&events);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].source_event_indices.len(), 2);
    assert_eq!(operations[0].event.summary.as_deref(), Some("retained context"));
    assert!(operations[0].event.summary_opaque);
    assert_eq!(operations[0].event.context.window_id.as_deref(), Some("window-2"));
    assert!(
      !events
        .iter()
        .any(|e| matches!(e, AgentEvent::Message(_) | AgentEvent::Lifecycle(_)))
    );
  }
}

fn snapshot_records() -> Vec<Value> {
  include_str!("../fixtures/compaction_snapshot.jsonl")
    .lines()
    .map(|record| serde_json::from_str(record).unwrap())
    .collect()
}

fn normalize_records(records: Vec<Value>) -> Vec<AgentEvent> {
  let mut normalizer = CodexNormalizer::new();
  records
    .into_iter()
    .flat_map(|record| line(&mut normalizer, record))
    .collect()
}

#[test]
fn persisted_snapshot_batch_preserves_one_operation_and_both_observations() {
  for message in ["", "retained context"] {
    for canonical in [false, true] {
      let mut records = snapshot_records();
      records[2]["payload"]["message"] = json!(message);
      if !canonical {
        records[0]["payload"]["history_mode"] = json!("legacy");
        records.last_mut().unwrap()["payload"] = json!({"type":"context_compacted"});
      }
      let events = normalize_records(records);
      let observations: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
          AgentEvent::Compaction(event) => Some(event),
          _ => None,
        })
        .collect();
      assert_eq!(observations.len(), 2);
      assert!(
        observations
          .iter()
          .all(|event| event.compaction_id.as_deref() == Some("window-2"))
      );
      let operations = tokn_session_core::compaction_operations(&events);
      assert_eq!(operations.len(), 1);
      assert_eq!(operations[0].source_event_indices.len(), 2);
      assert!(operations[0].event.summary_opaque);
      assert_eq!(
        operations[0].event.summary.as_deref(),
        (!message.is_empty()).then_some(message)
      );
      assert_eq!(operations[0].event.context.window_id.as_deref(), Some("window-2"));
      assert!(!events.iter().any(|event| matches!(event, AgentEvent::Message(_))));
      assert_eq!(
        events
          .iter()
          .filter(|event| matches!(event, AgentEvent::Lifecycle(_)))
          .count(),
        1
      );
    }
  }
}

#[test]
fn snapshot_turn_binds_old_checkpoints_without_resume_metadata() {
  let mut records = snapshot_records();
  records[2]["payload"].as_object_mut().unwrap().remove("resume_metadata");
  assert_eq!(
    tokn_session_core::compaction_operations(&normalize_records(records.clone())).len(),
    1
  );
  records.last_mut().unwrap()["payload"]["turn_id"] = json!("another-turn");
  assert_eq!(
    tokn_session_core::compaction_operations(&normalize_records(records)).len(),
    2
  );
}

#[test]
fn older_settings_snapshots_may_omit_the_thread_identity() {
  for identity in [None, Some(Value::Null)] {
    let mut records = snapshot_records();
    let payload = records[5]["payload"].as_object_mut().unwrap();
    match identity {
      Some(identity) => {
        payload.insert("thread_id".into(), identity);
      }
      None => {
        payload.remove("thread_id");
      }
    }
    assert_eq!(
      tokn_session_core::compaction_operations(&normalize_records(records)).len(),
      1
    );
  }
}

#[test]
fn snapshot_accounting_must_belong_to_the_checkpoint_session_and_turn() {
  let mut accounting = token_usage_record();
  accounting["payload"]["thread_id"] = json!("fixture");
  accounting["payload"]["session_id"] = json!("fixture");
  let mut records = snapshot_records();
  records.insert(records.len() - 1, accounting.clone());
  assert_eq!(
    tokn_session_core::compaction_operations(&normalize_records(records)).len(),
    1
  );
  for field in ["thread_id", "session_id", "turn_id"] {
    let mut foreign = accounting.clone();
    foreign["payload"][field] = json!("another");
    let mut records = snapshot_records();
    records.insert(records.len() - 1, foreign);
    assert_eq!(
      tokn_session_core::compaction_operations(&normalize_records(records)).len(),
      2,
      "{field}"
    );
  }
}

#[test]
fn repeated_or_out_of_order_context_updates_are_not_checkpoint_snapshot_records() {
  for index in [3, 4, 5] {
    let mut records = snapshot_records();
    records.insert(index + 1, records[index].clone());
    assert_eq!(
      tokn_session_core::compaction_operations(&normalize_records(records)).len(),
      2,
      "repeated {index}"
    );
  }
  let mut records = snapshot_records();
  records.swap(3, 4);
  assert_eq!(
    tokn_session_core::compaction_operations(&normalize_records(records)).len(),
    2
  );
}

#[test]
fn unrelated_or_consumed_reply_records_break_snapshot_correlation() {
  let async_reply = "<send_user_message_question_reply>\n[{\"questionItemId\":\"opaque-id\",\"question\":\"Which?\",\"answer\":\"Local\"}]\n</send_user_message_question_reply>";
  for record in [
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"another-turn"}}),
    json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"turn-1"}}),
    json!({"type":"event_msg","payload":{"type":"future_record"}}),
    json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply"}]}}),
    json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"exec_command","arguments":"{}"}}),
    json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":false}}),
    json!({"type":"session_meta","payload":{"id":"another-session","history_mode":"paginated"}}),
    json!({"type":"event_msg","payload":{"type":"user_message","message":async_reply}}),
    json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"fixture","turn_id":"turn-1",
      "item":{"type":"FunctionCallOutput","id":"question-call","name":"request_user_input","output":{"answers":{}}}}}),
  ] {
    let mut records = snapshot_records();
    records.insert(records.len() - 1, record.clone());
    assert_eq!(
      tokn_session_core::compaction_operations(&normalize_records(records)).len(),
      2,
      "{record}"
    );
  }
}

#[test]
fn malformed_or_mismatched_snapshot_records_break_correlation() {
  for (index, pointer, value) in [
    (2, "/payload/resume_metadata", json!([])),
    (2, "/payload/resume_metadata/last_started_turn_id", json!(0)),
    (2, "/payload/resume_metadata/last_started_turn_id", json!(" ")),
    (3, "/payload/full", json!(false)),
    (3, "/payload/state", json!(null)),
    (4, "/payload/turn_id", json!("another-turn")),
    (4, "/payload/turn_id", json!("")),
    (5, "/payload/thread_id", json!("another-session")),
    (5, "/payload/thread_id", json!(0)),
    (5, "/payload/thread_id", json!(" ")),
    (5, "/payload/thread_settings", json!(null)),
    (6, "/payload/info/total_token_usage/total_tokens", json!(-1)),
    (7, "/payload/thread_id", json!("another-session")),
    (7, "/payload/turn_id", json!("another-turn")),
  ] {
    let mut records = snapshot_records();
    *records[index].pointer_mut(pointer).unwrap() = value;
    assert_eq!(
      tokn_session_core::compaction_operations(&normalize_records(records)).len(),
      2,
      "{index} {pointer}"
    );
  }
  for record in [
    json!({"type":"token_usage_record","payload":{}}),
    json!({"type":"event_msg","payload":{"type":"token_count"}}),
  ] {
    let mut records = snapshot_records();
    records.insert(records.len() - 1, record.clone());
    assert_eq!(
      tokn_session_core::compaction_operations(&normalize_records(records)).len(),
      2,
      "{record}"
    );
  }
}

#[test]
fn malformed_completion_consumes_pending_checkpoint_without_hiding_the_source() {
  for (pointer, value) in [
    ("/payload/thread_id", json!(null)),
    ("/payload/turn_id", json!(" ")),
    ("/payload/item/id", json!("")),
  ] {
    let mut records = snapshot_records();
    let mut malformed = records.last().unwrap().clone();
    *malformed.pointer_mut(pointer).unwrap() = value;
    records.insert(records.len() - 1, malformed);
    let events = normalize_records(records);
    assert!(events.iter().any(|event| matches!(event, AgentEvent::Unknown(_))));
    assert_eq!(tokn_session_core::compaction_operations(&events).len(), 2);
  }
}

#[test]
fn successive_checkpoints_and_blank_window_ids_keep_distinct_operation_keys() {
  let mut records = snapshot_records();
  records[2]["payload"]["window_id"] = json!(" ");
  let mut next = records[2..].to_vec();
  next[0]["payload"]["window_id"] = json!("");
  next.last_mut().unwrap()["payload"]["item"]["id"] = json!("compact-2");
  records.extend(next);
  let events = normalize_records(records);
  let operations = tokn_session_core::compaction_operations(&events);
  assert_eq!(operations.len(), 2);
  assert_eq!(operations[0].event.compaction_id.as_deref(), Some("checkpoint:1"));
  assert_eq!(operations[1].event.compaction_id.as_deref(), Some("checkpoint:2"));
  assert!(
    operations
      .iter()
      .all(|operation| operation.source_event_indices.len() == 2)
  );
}

#[test]
fn unrelated_records_break_codex_compaction_correlation() {
  let mut normalizer = normalizer();
  let mut events = line(
    &mut normalizer,
    json!({"type":"compacted","payload":{"message":"summary"}}),
  );
  events.extend(line(
    &mut normalizer,
    json!({"type":"turn_context","payload":{"turn_id":"next"}}),
  ));
  events.extend(line(
    &mut normalizer,
    json!({"type":"event_msg","payload":{"type":"context_compacted"}}),
  ));
  assert_eq!(tokn_session_core::compaction_operations(&events).len(), 2);
}

#[test]
fn opaque_compaction_alias_and_marker_only_context_compaction_are_distinct() {
  for (payload, opaque) in [
    (json!({"type":"compaction_summary","encrypted_content":"opaque"}), true),
    (json!({"type":"context_compaction"}), false),
  ] {
    let events = line(&mut normalizer(), json!({"type":"response_item","payload":payload}));
    assert!(matches!(&events[..], [AgentEvent::Compaction(e)] if e.summary_opaque == opaque));
  }
  let events = line(
    &mut normalizer(),
    json!({"type":"response_item","payload":{"type":"compaction"}}),
  );
  assert!(matches!(&events[..], [AgentEvent::Unknown(_)]));
}

fn token_count(total: u64) -> Value {
  let counters = json!({"input_tokens":total-5,"output_tokens":5,"cached_input_tokens":10,
    "cache_write_input_tokens":2,"reasoning_output_tokens":2,"total_tokens":total});
  json!({"type":"event_msg","ordinal":42,"timestamp":"2026-08-28T00:00:00Z","payload":{
    "type":"token_count","info":{"total_token_usage":counters,"last_token_usage":counters,
      "model_context_window":100000,"future_field":true},"rate_limits":null
  }})
}

fn token_usage_record() -> Value {
  let counters = |total: u64| {
    json!({"input_tokens":total-5,"output_tokens":5,"cached_input_tokens":10,
      "cache_write_input_tokens":2,"reasoning_output_tokens":2,"total_tokens":total})
  };
  json!({"type":"token_usage_record","ordinal":41,"timestamp":"2026-09-13T04:00:00Z","payload":{
    "thread_id":"codex-1","session_id":"codex-1","turn_id":"turn-1","root_turn_id":"root-turn-1",
    "response_id":"response-1","usage":counters(35),"turn_token_usage":counters(70),
    "thread_token_usage":counters(140),"future_field":true
  }})
}

#[test]
fn token_usage_record_is_a_model_call_without_adding_aggregate_or_cached_tokens() {
  let record = token_usage_record();
  let events = line(&mut normalizer(), record.clone());
  let [AgentEvent::Usage(event)] = &events[..] else {
    panic!("expected one model call")
  };
  assert_eq!(event.kind, UsageKind::ModelCall);
  assert_eq!(event.session_id.as_deref(), Some("codex-1"));
  assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
  assert_eq!(event.record_id.as_deref(), Some("response-1"));
  assert!(event.message_id.is_none());
  assert!(event.step_id.is_none());
  assert_eq!(event.input_tokens, 30);
  assert_eq!(event.output_tokens, 5);
  assert_eq!(event.total_tokens, Some(35));
  assert_eq!(event.cache_read_tokens, Some(10));
  assert_eq!(event.cache_write_tokens, Some(2));
  assert_eq!(event.reasoning_tokens, Some(2));
  assert_eq!(event.timestamp.as_deref(), Some("2026-09-13T04:00:00Z"));
  assert_eq!(event.native, record["payload"]);
}

#[test]
fn model_calls_with_equal_counters_remain_distinct_from_session_snapshots() {
  let mut normalizer = normalizer();
  let mut record = token_usage_record();
  let mut events = line(&mut normalizer, record.clone());
  events.extend(line(&mut normalizer, token_count(140)));
  record["payload"]["response_id"] = json!("response-2");
  events.extend(line(&mut normalizer, record));
  // Per-call observations must not reset the separate snapshot deduplication.
  assert!(line(&mut normalizer, token_count(140)).is_empty());
  let kinds: Vec<_> = events
    .iter()
    .map(|event| match event {
      AgentEvent::Usage(event) => event.kind,
      _ => panic!("expected usage"),
    })
    .collect();
  assert_eq!(
    kinds,
    [UsageKind::ModelCall, UsageKind::SessionSnapshot, UsageKind::ModelCall]
  );
}

#[test]
fn token_usage_record_keeps_optional_identity_and_cache_counters_absent() {
  let mut record = token_usage_record();
  let payload = record["payload"].as_object_mut().unwrap();
  for field in [
    "response_id",
    "turn_id",
    "root_turn_id",
    "turn_token_usage",
    "thread_token_usage",
  ] {
    payload.remove(field);
  }
  payload["usage"]
    .as_object_mut()
    .unwrap()
    .remove("cache_write_input_tokens");
  let events = line(&mut normalizer(), record);
  let [AgentEvent::Usage(event)] = &events[..] else {
    panic!("expected model call")
  };
  assert_eq!(event.record_id.as_deref(), Some("41"));
  assert!(event.turn_id.is_none());
  assert!(event.cache_write_tokens.is_none());
}

#[test]
fn token_usage_record_retains_zero_and_large_counters() {
  for total in [0, u64::MAX] {
    let mut record = token_usage_record();
    record["payload"]["usage"] = json!({"input_tokens":total,"output_tokens":0,
      "cached_input_tokens":0,"reasoning_output_tokens":0,"total_tokens":total});
    let events = line(&mut normalizer(), record);
    assert!(matches!(&events[..], [AgentEvent::Usage(event)]
      if event.input_tokens == total && event.output_tokens == 0 && event.total_tokens == Some(total)));
  }
}

#[test]
fn malformed_token_usage_records_retain_the_complete_native_envelope() {
  for (pointer, value) in [
    ("/payload/usage", Value::Null),
    ("/payload/usage/input_tokens", json!(-1)),
    ("/payload/usage/total_tokens", json!("bad")),
    ("/payload/turn_token_usage/output_tokens", json!(-1)),
    ("/payload/thread_token_usage/cached_input_tokens", json!(true)),
  ] {
    let mut record = token_usage_record();
    *record.pointer_mut(pointer).unwrap() = value;
    let events = line(&mut normalizer(), record.clone());
    assert!(matches!(&events[..], [AgentEvent::Unknown(event)]
      if event.native_type.as_deref() == Some("token_usage_record") && event.native.as_ref() == Some(&record)));
  }
}

#[test]
fn historical_subagent_filter_applies_before_per_response_usage() {
  let mut normalizer = CodexNormalizer::new_historical();
  line(
    &mut normalizer,
    json!({"type":"session_meta","payload":{"id":"child",
    "source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}}}),
  );
  assert!(line(&mut normalizer, token_usage_record()).is_empty());
  line(
    &mut normalizer,
    json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}}),
  );
  let mut record = token_usage_record();
  record["payload"]["thread_id"] = json!("child");
  let events = line(&mut normalizer, record);
  assert!(matches!(&events[..], [AgentEvent::Usage(event)]
    if event.kind == UsageKind::ModelCall && event.session_id.as_deref() == Some("child")));
}

#[test]
fn token_count_is_a_replaceable_snapshot_without_double_counting_cache() {
  let mut normalizer = normalizer();
  let record = token_count(35);
  let events = line(&mut normalizer, record.clone());
  let [AgentEvent::Usage(event)] = &events[..] else {
    panic!("expected snapshot")
  };
  assert_eq!(event.kind, UsageKind::SessionSnapshot);
  assert_eq!(event.session_id.as_deref(), Some("codex-1"));
  assert_eq!(event.record_id.as_deref(), Some("42"));
  assert!(event.turn_id.is_none());
  assert!(event.message_id.is_none());
  assert_eq!(event.input_tokens, 30);
  assert_eq!(event.output_tokens, 5);
  assert_eq!(event.cache_read_tokens, Some(10));
  assert_eq!(event.cache_write_tokens, Some(2));
  assert_eq!(event.total_tokens, Some(35));
  assert_eq!(event.native, record["payload"]["info"]);
  assert!(line(&mut normalizer, record).is_empty());
  for total in [40, 20, 5] {
    let events = line(&mut normalizer, token_count(total));
    assert!(matches!(&events[..], [AgentEvent::Usage(event)] if event.total_tokens == Some(total)));
  }
}

#[test]
fn rate_limit_only_changes_do_not_repeat_usage() {
  let mut normalizer = normalizer();
  let mut record = token_count(35);
  line(&mut normalizer, record.clone());
  record["payload"]["rate_limits"] = json!({"primary":{"used_percent":25.0,"window_minutes":300},"plan_type":"pro"});
  let events = line(&mut normalizer, record.clone());
  assert!(matches!(&events[..], [AgentEvent::Metadata(event)] if matches!(event.kind, MetadataKind::Diagnostic)));
  assert!(line(&mut normalizer, record.clone()).is_empty());
  record["payload"]["rate_limits"] = Value::Null;
  assert!(matches!(&line(&mut normalizer, record)[..], [AgentEvent::Metadata(_)]));
}

#[test]
fn unavailable_usage_does_not_fabricate_zero_and_resets_duplicate_detection() {
  let mut normalizer = normalizer();
  line(&mut normalizer, token_count(35));
  let events = line(
    &mut normalizer,
    json!({"type":"event_msg","payload":{"type":"token_count","info":null}}),
  );
  assert!(matches!(&events[..], [AgentEvent::Metadata(event)] if event.summary == "usage unavailable"));
  assert!(matches!(
    &line(&mut normalizer, token_count(35))[..],
    [AgentEvent::Usage(_)]
  ));
}

#[test]
fn total_only_context_estimates_remain_visible() {
  let mut record = token_count(35);
  for field in ["total_token_usage", "last_token_usage"] {
    record["payload"]["info"][field] = json!({"input_tokens":0,"output_tokens":0,"cached_input_tokens":0,
      "reasoning_output_tokens":0,"total_tokens":100000});
  }
  let events = line(&mut normalizer(), record);
  assert!(
    matches!(&events[..], [AgentEvent::Usage(event)] if event.total_tokens == Some(100000) && event.input_tokens == 0)
  );
}

#[test]
fn malformed_usage_and_rate_limits_stay_unknown() {
  for (pointer, value) in [
    ("/payload/info/total_token_usage/input_tokens", json!(-1)),
    ("/payload/info/last_token_usage/output_tokens", json!("bad")),
    ("/payload/info/total_token_usage/total_tokens", Value::Null),
    ("/payload/rate_limits", json!({"primary":{}})),
    ("/payload/rate_limits", json!({"individual_limit":{}})),
    ("/payload/rate_limits", json!([])),
  ] {
    let mut record = token_count(35);
    *record.pointer_mut(pointer).unwrap() = value;
    let events = line(&mut normalizer(), record.clone());
    assert!(matches!(&events[..], [AgentEvent::Unknown(event)] if event.native.as_ref() == Some(&record)));
  }
}

#[test]
fn context_records_are_metadata_not_final_messages() {
  for record in [
    json!({"type":"turn_context","payload":{"turn_id":"turn-1","model":"model","effort":"low"}}),
    json!({"type":"world_state","payload":{"full":false,"state":{"opaque":true}}}),
    json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":false}}),
    json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":2}}),
  ] {
    let events = line(&mut normalizer(), record.clone());
    assert!(
      matches!(&events[..], [AgentEvent::Metadata(event)] if event.native == record),
      "{record}"
    );
  }
  let record = json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"codex-1","turn_id":"turn-1",
    "item":{"type":"ContextCompaction","id":"compact-1"},"completed_at_ms":1}});
  assert!(matches!(&line(&mut paginated_normalizer(), record.clone())[..],
    [AgentEvent::Compaction(event)] if event.compaction_id.as_deref() == Some("compact-1")));
}

#[test]
fn malformed_context_and_future_events_stay_unknown() {
  for record in [
    json!({"type":"turn_context","payload":{}}),
    json!({"type":"world_state","payload":{"full":true}}),
    json!({"type":"compacted","payload":{}}),
    json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":-1}}),
    json!({"type":"event_msg","payload":{"type":"future_event","ignorable":true}}),
  ] {
    assert!(
      matches!(&line(&mut normalizer(), record.clone())[..], [AgentEvent::Unknown(_)]),
      "{record}"
    );
  }
  let record = json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"codex-1","turn_id":"turn-1",
    "item":{"type":"ContextCompaction"},"completed_at_ms":1}});
  assert!(matches!(
    &line(&mut paginated_normalizer(), record)[..],
    [AgentEvent::Unknown(_)]
  ));
}

#[test]
fn historical_subagent_filter_applies_before_accounting_and_context() {
  let mut normalizer = CodexNormalizer::new_historical();
  line(
    &mut normalizer,
    json!({"type":"session_meta","payload":{"id":"child",
    "source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}}}),
  );
  assert!(line(&mut normalizer, token_count(35)).is_empty());
  assert!(
    line(
      &mut normalizer,
      json!({"type":"world_state","payload":{"full":true,"state":{}}})
    )
    .is_empty()
  );
  assert!(
    line(
      &mut normalizer,
      json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}})
    )
    .is_empty()
  );
  let events = line(&mut normalizer, token_count(35));
  assert!(matches!(&events[..], [AgentEvent::Usage(event)] if event.session_id.as_deref() == Some("child")));
}

#[test]
fn rollback_and_compaction_reset_snapshot_deduplication() {
  for record in [
    json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
    json!({"type":"compacted","payload":{"message":"summary"}}),
  ] {
    let mut normalizer = normalizer();
    line(&mut normalizer, token_count(35));
    line(&mut normalizer, record);
    assert!(matches!(
      &line(&mut normalizer, token_count(35))[..],
      [AgentEvent::Usage(_)]
    ));
  }
  let mut normalizer = paginated_normalizer();
  line(&mut normalizer, token_count(35));
  line(
    &mut normalizer,
    json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"codex-1","turn_id":"turn-1",
      "item":{"type":"ContextCompaction","id":"compact-1"},"completed_at_ms":1}}),
  );
  assert!(matches!(
    &line(&mut normalizer, token_count(35))[..],
    [AgentEvent::Usage(_)]
  ));
}
