use super::*;
use serde_json::json;
use std::io::Write;
use tempfile::TempDir;

fn row(value: serde_json::Value) -> String {
  format!("{value}\n")
}

fn turn(index: usize) -> String {
  row(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":format!("turn-{index}")}}))
    + &row(
      json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("prompt-{index}")}]}}),
    )
    + &row(
      json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":format!("answer-{index}")}]}}),
    )
}

fn fixture(body: &str) -> (TempDir, PathBuf, CodexSessionSource) {
  let directory = TempDir::new().unwrap();
  let path = directory.path().join("rollout-lazy.jsonl");
  std::fs::write(
    &path,
    row(json!({"type":"session_meta","payload":{"id":"lazy","cwd":"/tmp"}})) + body,
  )
  .unwrap();
  let source = CodexSessionSource::new(Some(directory.path().to_path_buf()));
  (directory, path, source)
}

#[test]
fn latest_turn_does_not_materialize_old_payloads_and_expands_explicitly() {
  let old = turn(0) + &row(json!({"type":"unknown","payload":{"data":"x".repeat(2 * 1024 * 1024)}}));
  let (_directory, path, source) = fixture(&(old + &turn(1) + &turn(2)));
  // The limit applies to loaded content, not the size of omitted history.
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024, 1);
  let latest = reader.poll(&source).unwrap().unwrap();
  assert!(latest.source_start.unwrap() > 2 * 1024 * 1024);
  assert_eq!(latest.records.len(), 3);
  assert!(reader.stats().rows_parsed < 10);
  assert!(reader.stats().boundary_bytes_read < 128 * 1024);
  assert!(reader.poll(&source).unwrap().is_none());
  reader.expand(Some(1));
  let earlier = reader.poll(&source).unwrap().unwrap();
  assert_eq!(earlier.records.len(), 6);
  assert_eq!(earlier.records[3].record_id, latest.records[0].record_id);
}

#[test]
fn malformed_omitted_history_is_reported_when_loaded() {
  let (_directory, path, source) = fixture(&("invalid json\n".to_string() + &turn(1)));
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024, 1);
  assert!(reader.poll(&source).unwrap().unwrap().source_start.unwrap() > 0);
  reader.expand(None);
  assert!(reader.poll(&source).err().unwrap().contains("invalid Codex history"));
}

#[test]
fn incomplete_tail_is_retained_until_append_completes_it() {
  let partial = row(
    json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"completed later"}]}}),
  );
  let split = partial.len() / 2;
  let (_directory, path, source) = fixture(&(turn(0) + &partial[..split]));
  let mut reader = CodexHistoryReader::new_window(path.clone(), false, 64 * 1024, 1);
  assert_eq!(reader.poll(&source).unwrap().unwrap().records.len(), 3);
  std::fs::OpenOptions::new()
    .append(true)
    .open(path)
    .unwrap()
    .write_all(partial[split..].as_bytes())
    .unwrap();
  let delta = reader.poll(&source).unwrap().unwrap();
  assert!(!delta.reset);
  assert_eq!(delta.records.len(), 1);
}

#[test]
fn tool_result_widens_the_window_to_its_invocation() {
  let invocation = row(
    json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-old","arguments":"{\"cmd\":\"pwd\"}"}}),
  );
  let result =
    row(json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-old","output":"/tmp"}}));
  let (_directory, path, source) = fixture(&(turn(0) + &invocation + &turn(1) + &result));
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024, 1);
  let loaded = reader.poll(&source).unwrap().unwrap();
  assert_eq!(loaded.records.len(), 8);
  assert!(loaded.records.iter().flat_map(|record| &record.events).any(|event| matches!(event, AgentEvent::ToolCall(tool) if tool.record_kind == tokn_session_core::ToolRecordKind::Invocation)));
}

#[test]
fn legacy_without_turn_starts_loads_full_history() {
  let (_directory, path, source) = fixture(&row(
    json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"old format"}]}}),
  ));
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024, 1);
  let loaded = reader.poll(&source).unwrap().unwrap();
  assert_eq!(loaded.source_start, Some(0));
  assert_eq!(loaded.records.len(), 2);
}

#[test]
fn latest_turn_skips_inherited_body_but_keeps_prefix_verification() {
  let directory = TempDir::new().unwrap();
  let base_path = directory.path().join("rollout-base-lazy.jsonl");
  let head_path = directory.path().join("rollout-head-lazy.jsonl");
  let base =
    row(json!({"ordinal":0,"type":"session_meta","payload":{"id":"lazy","history_mode":"paginated","cwd":"/tmp"}}))
      + &row(json!({"ordinal":1,"type":"event_msg","payload":{"type":"task_started","turn_id":"older"}}))
      + &row(json!({"ordinal":2,"type":"unknown","payload":{"data":"x".repeat(1024 * 1024)}}));
  std::fs::write(&base_path, &base).unwrap();
  let head = row(
    json!({"ordinal":3,"type":"session_meta","payload":{"id":"lazy","history_mode":"paginated","cwd":"/tmp","history_base":{"thread_id":"lazy","end_ordinal_exclusive":3,"end_byte_offset":base.len()}}}),
  ) + &row(json!({"ordinal":4,"type":"event_msg","payload":{"type":"task_started","turn_id":"latest"}}))
    + &row(json!({"ordinal":5,"type":"event_msg","payload":{"type":"user_message","message":"latest"}}));
  std::fs::write(&head_path, head).unwrap();
  let source = CodexSessionSource::new(Some(directory.path().into()));
  let mut reader = CodexHistoryReader::new_window(head_path, false, 64 * 1024, 1);
  let latest = reader.poll(&source).unwrap().unwrap();
  assert_eq!(latest.records.len(), 2);
  assert!(reader.stats().source_bytes_read < 1024);
  assert!(latest.source_start.unwrap() > base.len() as u64);
  // A same-size rewrite of a skipped prefix must still invalidate its lineage.
  let changed = base.replace("older", "other");
  std::fs::write(base_path, changed).unwrap();
  assert!(reader.poll(&source).unwrap().unwrap().reset);
}

#[test]
fn completed_lifecycle_snapshots_do_not_require_an_invocation() {
  let snapshot = row(
    json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"lazy","turn_id":"turn-1","item":{"type":"CommandExecution","id":"complete-command","command":["pwd"],"cwd":"/tmp","status":"completed","exit_code":0,"stdout":"/tmp"}}}),
  );
  let (_directory, path, source) = fixture(&(turn(0) + &turn(1) + &snapshot));
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024, 1);
  let loaded = reader.poll(&source).unwrap().unwrap();
  assert_eq!(loaded.records.len(), 4);
  assert!(loaded.source_start.unwrap() > 0);
}

#[test]
fn late_result_for_omitted_invocation_widens_during_live_follow() {
  let invocation = row(
    json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"late-call","arguments":"{\"cmd\":\"pwd\"}"}}),
  );
  let (_directory, path, source) = fixture(&(turn(0) + &invocation + &turn(1)));
  let mut reader = CodexHistoryReader::new_window(path.clone(), false, 64 * 1024, 1);
  assert_eq!(reader.poll(&source).unwrap().unwrap().records.len(), 3);
  let result = row(
    json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"late-call","output":"/tmp"}}),
  );
  std::fs::OpenOptions::new()
    .append(true)
    .open(path)
    .unwrap()
    .write_all(result.as_bytes())
    .unwrap();
  let widened = reader.poll(&source).unwrap().unwrap();
  assert!(widened.reset);
  assert_eq!(widened.records.len(), 8);
}

#[test]
fn a_new_turn_without_a_user_message_does_not_slide_the_loaded_range() {
  let (_directory, path, source) = fixture(&(turn(0) + &turn(1)));
  let mut reader = CodexHistoryReader::new_window(path.clone(), false, 64 * 1024, 1);
  let first = reader.poll(&source).unwrap().unwrap();
  let start = row(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}));
  std::fs::OpenOptions::new()
    .append(true)
    .open(path)
    .unwrap()
    .write_all(start.as_bytes())
    .unwrap();
  assert!(!reader.poll(&source).unwrap().unwrap().reset);
  reader.invalidate();
  let reset = reader.poll(&source).unwrap().unwrap();
  assert_eq!(reset.source_start, first.source_start);
  assert_eq!(reset.records.len(), 4);
}

#[test]
fn lazy_model_checkpoint_is_separate_cached_and_preserves_source_identity() {
  let settings = row(json!({"type":"event_msg","payload":{"type":"thread_settings_applied",
    "thread_settings":{"model_provider_id":"custom","model":"before"}}}));
  let context = row(json!({"type":"turn_context","payload":{"turn_id":"turn-4","model":"after"}}));
  let body = settings + &turn(0) + &turn(1) + &turn(2) + &turn(3) + &turn(4) + &context;
  let (_directory, path, source) = fixture(&body);
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024, 3);
  let retained = reader.poll(&source).unwrap().unwrap();
  let baseline = retained.reference.model_baseline.as_ref().unwrap();
  assert_eq!(baseline.model.model_provider.as_deref(), Some("custom"));
  assert_eq!(baseline.model.model_id, "before");
  assert_eq!(baseline.origin, tokn_session_core::ModelOrigin::Selection);
  assert_eq!(retained.records.len(), 10);
  assert!(
    retained
      .records
      .iter()
      .flat_map(|record| &record.events)
      .any(|event| matches!(event,
    AgentEvent::ProviderChanged(event) if event.model_id.as_deref() == Some("after")
      && event.model_provider.as_deref() == Some("custom")))
  );
  let checkpoint_reads = reader.stats().model_checkpoint_bytes_read;
  assert!(checkpoint_reads > 0);
  reader.invalidate();
  let same = reader.poll(&source).unwrap().unwrap();
  assert_eq!(reader.stats().model_checkpoint_bytes_read, checkpoint_reads);
  assert_eq!(same.records[0].record_id, retained.records[0].record_id);
  reader.expand(None);
  let full = reader.poll(&source).unwrap().unwrap();
  assert!(full.reference.model_baseline.is_none());
  assert_eq!(full.records[8].record_id, retained.records[0].record_id);
}

#[test]
fn latest_model_checkpoint_does_not_scan_older_payloads_to_guess_a_provider() {
  let large = row(json!({"type":"unknown","payload":{"data":"x".repeat(2 * 1024 * 1024)}}));
  let context = row(json!({"type":"turn_context","payload":{"turn_id":"old","model":"unqualified"}}));
  let (_directory, path, source) = fixture(&(large + &context + &turn(1)));
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024, 1);
  let retained = reader.poll(&source).unwrap().unwrap();
  let baseline = retained.reference.model_baseline.as_ref().unwrap();
  assert_eq!(baseline.model.model_id, "unqualified");
  assert!(baseline.model.model_provider.is_none());
  assert_eq!(reader.stats().rows_parsed, 3);
  assert!(reader.stats().model_checkpoint_bytes_read < 128 * 1024);
}

#[test]
fn absent_model_checkpoint_has_a_hard_read_budget_without_loading_omitted_rows() {
  let large = row(json!({"type":"unknown","payload":{"data":"x".repeat(2 * 1024 * 1024)}}));
  let (_directory, path, source) = fixture(&(large + &turn(1)));
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024, 1);
  let retained = reader.poll(&source).unwrap().unwrap();
  assert!(retained.reference.model_baseline.is_none());
  assert_eq!(retained.records.len(), 3);
  assert_eq!(reader.stats().rows_parsed, 3);
  assert_eq!(reader.stats().model_checkpoint_bytes_read, 1024 * 1024);
  assert!(retained.source_start.unwrap() > 2 * 1024 * 1024);
  assert!(reader.poll(&source).unwrap().is_none());
}

#[test]
fn response_metadata_is_not_seeded_from_lazy_model_baselines_and_follows_exact_appends() {
  fn response(turn: &str, text: &str) -> String {
    row(
      json!({"type":"response_item","payload":{"type":"message","id":text,"role":"assistant",
      "phase":"final_answer","content":[{"type":"output_text","text":text}],
      "internal_chat_message_metadata_passthrough":{"turn_id":turn}}}),
    )
  }
  fn messages(records: &[NormalizedRecord]) -> Vec<(&str, &tokn_session_core::MessageEvent)> {
    records
      .iter()
      .flat_map(|record| {
        record.events.iter().filter_map(|event| match event {
          AgentEvent::Message(message) if message.role == tokn_session_core::Role::Assistant => {
            Some((record.record_id.as_str(), message))
          }
          _ => None,
        })
      })
      .collect()
  }
  let old = row(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}))
    + &row(json!({"type":"turn_context","payload":{"turn_id":"old","model":"model-a"}}))
    + &response("old", "old-answer")
    + &row(json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}));
  let current = row(json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"current"}}))
    + &response("current", "unscoped-answer");
  let (_directory, path, source) = fixture(&(old + &current));
  let mut reader = CodexHistoryReader::new_window(path.clone(), true, 64 * 1024, 1);
  let latest = reader.poll(&source).unwrap().unwrap();
  assert!(latest.source_start.unwrap() > 0);
  assert_eq!(
    latest.reference.model_baseline.as_ref().unwrap().model.model_id,
    "model-a"
  );
  let retained = messages(&latest.records);
  assert_eq!(retained.len(), 1);
  assert!(retained[0].1.response_metadata.is_none());
  let retained_key = retained[0].0.to_owned();

  let append = row(json!({"type":"turn_context","payload":{"turn_id":"current","model":"model-b"}}))
    + &response("current", "current-answer");
  std::fs::OpenOptions::new()
    .append(true)
    .open(&path)
    .unwrap()
    .write_all(append.as_bytes())
    .unwrap();
  let delta = reader.poll(&source).unwrap().unwrap();
  assert!(!delta.reset);
  let appended = messages(&delta.records);
  assert_eq!(appended.len(), 1);
  assert_eq!(
    appended[0]
      .1
      .response_metadata
      .as_ref()
      .unwrap()
      .model
      .as_ref()
      .unwrap()
      .model
      .model_id,
    "model-b"
  );
  let appended_key = appended[0].0.to_owned();

  let full = CodexHistoryReader::new(path, true, 64 * 1024)
    .poll(&source)
    .unwrap()
    .unwrap();
  let full_messages = messages(&full.records);
  assert_eq!(full_messages.len(), 3);
  assert_eq!(full_messages[1].0, retained_key);
  assert_eq!(full_messages[2].0, appended_key);
  assert_eq!(full_messages[1].1.response_metadata, retained[0].1.response_metadata);
  assert_eq!(full_messages[2].1.response_metadata, appended[0].1.response_metadata);
}

#[test]
fn goal_triggered_window_keeps_the_preamble_and_finds_a_continued_goal_completion() {
  let objective = "Review the parser.";
  let goal = |status| json!({"threadId":"lazy","objective":objective,"status":status,"createdAt":10});
  let marker = row(
    json!({"type":"response_item","payload":{"type":"message","role":"user","id":"goal-input",
    "content":[{"type":"input_text","text":format!(
      "<codex_internal_context source=\"user_goal\">\nUser set the goal: \"{objective}\"\nUser set goal status: \"active\".\n</codex_internal_context>")}],
    "internal_chat_message_metadata_passthrough":{"turn_id":"input-turn","content_item_kinds":["user.goal"],
    "content_item_metadata":[{"provenance":{"type":"harness","hook":false}}]}}}),
  );
  let active =
    row(json!({"type":"event_msg","payload":{"type":"thread_goal_updated","threadId":"lazy","goal":goal("active")}}));
  let goal_turn = row(
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"work",
    "turn_attribution":{"turn_trigger":"goal"}}}),
  );
  let (_directory, path, source) = fixture(&(turn(0) + &marker + &active + &goal_turn));
  let mut reader = CodexHistoryReader::new_window(path.clone(), false, 64 * 1024, 1);
  let first = reader.poll(&source).unwrap().unwrap();
  assert!(first.source_start.unwrap() > 0);
  assert!(
    matches!(first.records[0].events.as_slice(), [AgentEvent::Message(m)] if m.is_user_goal() && m.text == objective)
  );

  // A later goal continuation can have no user prompt or active snapshot in its
  // own turn. Its successful receipt still belongs to the original objective.
  let metadata = json!({"turn_id":"continued"});
  let continuation = row(
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"continued",
    "turn_attribution":{"turn_trigger":"goal"}}}),
  ) + &row(
    json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"update",
      "input":"text(await tools.update_goal({status:\"complete\"}))","internal_chat_message_metadata_passthrough":metadata}}),
  ) + &row(
    json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"update",
      "output":[{"type":"input_text","text":"Script completed\nWall time 0.0 seconds\nOutput:\n"},
        {"type":"input_text","text":json!({"goal":goal("complete")}).to_string()}],"internal_chat_message_metadata_passthrough":metadata}}),
  );
  std::fs::OpenOptions::new()
    .append(true)
    .open(&path)
    .unwrap()
    .write_all(continuation.as_bytes())
    .unwrap();

  let appended = reader.poll(&source).unwrap().unwrap();
  let mut reopened = CodexHistoryReader::new_window(path, false, 64 * 1024, 1);
  let reopened = reopened.poll(&source).unwrap().unwrap();
  for snapshot in [appended, reopened] {
    assert_eq!(
      snapshot
        .records
        .iter()
        .flat_map(|r| &r.events)
        .filter(|event| matches!(event,
      AgentEvent::GoalUpdated(g) if g.turn_id.as_deref()==Some("continued") && g.goal==Some(goal("complete"))))
        .count(),
      1
    );
  }
}

#[test]
fn optional_goal_preamble_does_not_load_an_oversized_unrelated_record() {
  let old = row(json!({"type":"unknown","payload":{"text":"x".repeat(256 * 1024)}}));
  let recent = row(
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"current",
    "turn_attribution":{"turn_trigger":"goal"}}}),
  );
  let (_directory, path, source) = fixture(&(old + &recent));
  let limits = CodexHistoryLimits {
    max_record_bytes: 32 * 1024,
    ..Default::default()
  };
  let mut reader = CodexHistoryReader::new_window_with_limits(path, false, limits, 1);
  let page = reader.poll(&source).unwrap().unwrap();
  assert_eq!(page.records.len(), 1);
  assert!(page.source_start.unwrap() > 256 * 1024);
}

#[test]
fn goal_lookbehind_skips_unrelated_message_payloads() {
  // This provider extension exceeds the depth accepted when materializing a
  // serde_json::Value. Scanning for a marker only needs the content item kinds.
  let extra = format!("{}null{}", "[".repeat(150), "]".repeat(150));
  let previous = format!(
    r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[],"extension":{extra}}}}}"#
  );
  let recent = row(
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"goal",
    "turn_attribution":{"turn_trigger":"goal"}}}),
  );
  let (_directory, path, source) = fixture(&(previous + "\n" + &recent));
  let mut reader = CodexHistoryReader::new_window(path, false, 32 * 1024, 1);
  let page = reader.poll(&source).unwrap().unwrap();
  assert_eq!(page.records.len(), 1);
  assert!(matches!(page.records[0].events.as_slice(), [AgentEvent::Lifecycle(turn)] if turn.turn_id == "goal"));
}
