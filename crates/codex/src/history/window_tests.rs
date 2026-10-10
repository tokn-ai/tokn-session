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
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024);
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
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024);
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
  let mut reader = CodexHistoryReader::new_window(path.clone(), false, 64 * 1024);
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
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024);
  let loaded = reader.poll(&source).unwrap().unwrap();
  assert_eq!(loaded.records.len(), 8);
  assert!(loaded.records.iter().flat_map(|record| &record.events).any(|event| matches!(event, AgentEvent::ToolCall(tool) if tool.record_kind == tokn_session_core::ToolRecordKind::Invocation)));
}

#[test]
fn legacy_without_turn_starts_loads_full_history() {
  let (_directory, path, source) = fixture(&row(
    json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"old format"}]}}),
  ));
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024);
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
  let mut reader = CodexHistoryReader::new_window(head_path, false, 64 * 1024);
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
  let mut reader = CodexHistoryReader::new_window(path, false, 64 * 1024);
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
  let mut reader = CodexHistoryReader::new_window(path.clone(), false, 64 * 1024);
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
  let mut reader = CodexHistoryReader::new_window(path.clone(), false, 64 * 1024);
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
