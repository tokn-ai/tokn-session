use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokn_session_codex::{
  CodexHistoryLimits, CodexHistoryProjectionReader, CodexHistoryReader, CodexScratchBudget, CodexSessionSource,
};
use tokn_session_core::AgentEvent;

fn meta() -> Value {
  json!({"type":"session_meta","ordinal":0,"payload":{
    "id":"thread","history_mode":"paginated","timestamp":"2026-10-30T00:00:00Z","cwd":"/test"
  }})
}

fn turn(ordinal: u64, text: &str) -> Vec<Value> {
  vec![
    json!({"type":"event_msg","ordinal":ordinal,"payload":{"type":"task_started","turn_id":format!("turn-{ordinal}")}}),
    json!({"type":"event_msg","ordinal":ordinal+1,"payload":{
      "type":"item_completed","thread_id":"thread","turn_id":format!("turn-{ordinal}"),
      "item":{"type":"AgentMessage","id":format!("answer-{ordinal}"),"phase":"final_answer",
        "content":[{"type":"Text","text":text}]}
    }}),
  ]
}

fn encode(path: &Path) -> std::path::PathBuf {
  let compressed = path.with_file_name("packed.jsonl.zst");
  let mut output = zstd::stream::write::Encoder::new(File::create(&compressed).unwrap(), 0).unwrap();
  std::io::copy(&mut File::open(path).unwrap(), &mut output).unwrap();
  output.finish().unwrap();
  compressed
}

#[test]
fn plain_and_compressed_sources_larger_than_128_mib_support_recent_windows_and_streaming() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let plain = root.path().join("large.jsonl");
  let mut output = BufWriter::new(File::create(&plain).unwrap());
  let header = format!("{}\n", meta());
  output.write_all(header.as_bytes()).unwrap();
  let padding = format!(
    "{}\n",
    json!({"type":"future-large-record","ordinal":1,"payload":{"padding":"x".repeat(64 * 1024)}})
  );
  let mut bytes = header.len();
  let mut padding_rows = 0;
  while bytes <= 128 * 1024 * 1024 {
    output.write_all(padding.as_bytes()).unwrap();
    bytes += padding.len();
    padding_rows += 1;
  }
  for row in turn(2, "latest answer") {
    writeln!(output, "{row}").unwrap();
  }
  drop(output);
  let decoded_bytes = fs::metadata(&plain).unwrap().len();
  let compressed = encode(&plain);
  let mut selected = Vec::new();
  for path in [&plain, &compressed] {
    let scratch = CodexScratchBudget::new(decoded_bytes);
    let limits = CodexHistoryLimits {
      max_record_bytes: 128 * 1024,
      max_snapshot_bytes: 4096,
      scratch: scratch.clone(),
      ..Default::default()
    };
    let mut window = CodexHistoryReader::new_window_with_limits(path.clone(), false, limits.clone(), 1);
    let update = window.poll(&source).unwrap().unwrap();
    assert!(update.source_start.unwrap() > 128 * 1024 * 1024);
    assert!(window.stats().source_bytes_read < 4096);
    assert_eq!(scratch.used_bytes(), 0, "temporary decoding is released after a poll");
    selected.push(
      serde_json::to_value(
        update
          .records
          .into_iter()
          .flat_map(|record| record.events)
          .collect::<Vec<_>>(),
      )
      .unwrap(),
    );

    let mut full = CodexHistoryReader::with_limits(path.clone(), false, limits.clone());
    assert!(full.poll(&source).err().unwrap().contains("snapshot size limit"));
    let mut projection = CodexHistoryProjectionReader::with_limits(
      path.clone(),
      CodexHistoryLimits {
        scratch: CodexScratchBudget::new(0),
        max_snapshot_bytes: 1,
        ..limits
      },
    );
    let mut unknown = 0;
    let mut messages = Vec::new();
    let update = projection
      .poll(&source, |_, record| {
        for event in record.events {
          match event {
            AgentEvent::Unknown(_) => unknown += 1,
            AgentEvent::Message(message) => messages.push(message.text),
            _ => {}
          }
        }
      })
      .unwrap()
      .unwrap();
    assert!(update.reset);
    assert_eq!(
      unknown, padding_rows,
      "unknown records remain visible through streaming"
    );
    assert_eq!(messages, ["latest answer"]);
    assert_eq!(projection.stats().source_bytes_read, decoded_bytes);
    let before = projection.stats();
    assert!(
      projection
        .poll(&source, |_, _| panic!("unchanged history reparsed"))
        .unwrap()
        .is_none()
    );
    assert_eq!(projection.stats(), before);
  }
  assert_eq!(selected[0], selected[1]);
}

#[test]
fn record_snapshot_scratch_and_work_budgets_are_independent() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let plain = root.path().join("head.jsonl");
  let mut rows = vec![meta()];
  rows.extend(turn(1, &"old".repeat(2048)));
  rows.extend(turn(3, "recent"));
  fs::write(&plain, rows.iter().map(|row| format!("{row}\n")).collect::<String>()).unwrap();
  let compressed = encode(&plain);
  let limits = CodexHistoryLimits {
    max_record_bytes: 16 * 1024,
    max_snapshot_bytes: 1024,
    scratch: CodexScratchBudget::new(0),
    ..Default::default()
  };
  let mut window = CodexHistoryReader::new_window_with_limits(plain.clone(), false, limits.clone(), 1);
  assert!(
    window.poll(&source).unwrap().is_some(),
    "a large earlier row does not consume the selected snapshot budget"
  );
  let mut packed = CodexHistoryReader::new_window_with_limits(compressed.clone(), false, limits.clone(), 1);
  assert!(packed.poll(&source).err().unwrap().contains("scratch disk budget"));
  assert_eq!(limits.scratch.used_bytes(), 0);
  let mut projection = CodexHistoryProjectionReader::with_limits(compressed.clone(), limits.clone());
  assert_eq!(
    projection
      .poll(&source, |_, _| {})
      .unwrap()
      .unwrap()
      .reference
      .message_count,
    2
  );

  let small_record = CodexHistoryLimits {
    max_record_bytes: 512,
    max_snapshot_bytes: 32 * 1024,
    ..limits.clone()
  };
  let mut projection = CodexHistoryProjectionReader::with_limits(compressed.clone(), small_record.clone());
  assert!(
    projection
      .poll(&source, |_, _| {})
      .err()
      .unwrap()
      .contains("record size limit")
  );
  let mut older_window = CodexHistoryReader::new_window_with_limits(plain, false, small_record, 2);
  assert!(older_window.poll(&source).err().unwrap().contains("record size limit"));

  let mut expired = CodexHistoryProjectionReader::with_limits(
    compressed,
    CodexHistoryLimits {
      max_duration: Duration::ZERO,
      ..limits
    },
  );
  assert!(
    expired
      .poll(&source, |_, _| panic!("expired work published records"))
      .err()
      .unwrap()
      .contains("work deadline")
  );
}

#[test]
fn append_budget_limits_each_materialized_batch_instead_of_total_history() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let path = root.path().join("head.jsonl");
  fs::write(&path, format!("{}\n", meta())).unwrap();
  let limits = CodexHistoryLimits {
    max_snapshot_bytes: 1024,
    ..Default::default()
  };
  let mut reader = CodexHistoryReader::new_window_with_limits(path.clone(), false, limits, 1);
  reader.poll(&source).unwrap();
  for ordinal in (1..21).step_by(2) {
    let batch = turn(ordinal, "small batch")
      .iter()
      .map(|row| format!("{row}\n"))
      .collect::<String>();
    assert!(batch.len() < 1024);
    fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(batch.as_bytes())
      .unwrap();
    let update = reader.poll(&source).unwrap().unwrap();
    assert!(!update.reset);
    assert_eq!(update.records.len(), 2);
  }
  assert!(fs::metadata(&path).unwrap().len() > 1024);
  let oversized = turn(21, &"x".repeat(1024))
    .iter()
    .map(|row| format!("{row}\n"))
    .collect::<String>();
  fs::OpenOptions::new()
    .append(true)
    .open(&path)
    .unwrap()
    .write_all(oversized.as_bytes())
    .unwrap();
  assert!(reader.poll(&source).err().unwrap().contains("snapshot size limit"));
  fs::write(&path, format!("{}\n", meta())).unwrap();
  assert!(
    reader.poll(&source).unwrap().unwrap().reset,
    "a failed batch invalidates its cursor"
  );
}

#[test]
fn dependency_widening_releases_scratch_before_retrying() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let plain = root.path().join("head.jsonl");
  let mut rows = vec![meta()];
  rows.extend(turn(1, "first"));
  rows.push(json!({"type":"response_item","ordinal":3,"payload":{"type":"function_call","name":"exec_command","call_id":"call-old","arguments":"{\"cmd\":\"pwd\"}"}}));
  rows.extend(turn(4, "second"));
  rows.push(json!({"type":"response_item","ordinal":6,"payload":{"type":"function_call_output","call_id":"call-old","output":"/tmp"}}));
  fs::write(&plain, rows.iter().map(|row| format!("{row}\n")).collect::<String>()).unwrap();
  let compressed = encode(&plain);
  let scratch = CodexScratchBudget::new(fs::metadata(&plain).unwrap().len());
  let mut reader = CodexHistoryReader::new_window_with_limits(
    compressed,
    false,
    CodexHistoryLimits {
      scratch: scratch.clone(),
      ..Default::default()
    },
    1,
  );
  assert!(reader.poll(&source).unwrap().unwrap().records.len() > 3);
  assert!(reader.stats().lineage_resolutions >= 2);
  assert_eq!(scratch.used_bytes(), 0);
}

#[test]
fn a_short_compressed_candidate_does_not_hide_the_matching_history_prefix() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let parent = root.path().join("parent.jsonl");
  let retained = format!(
    "{}\n{}\n",
    meta(),
    json!({"type":"future-record","ordinal":1,"payload":{"padding":"x".repeat(128 * 1024)}})
  );
  fs::write(&parent, &retained).unwrap();
  let short = root.path().join("short.jsonl.zst");
  let mut output = zstd::stream::write::Encoder::new(File::create(short).unwrap(), 0).unwrap();
  writeln!(output, "{}", meta()).unwrap();
  output.finish().unwrap();
  let mut head_meta = meta();
  head_meta["ordinal"] = json!(2);
  head_meta["payload"]["history_base"] =
    json!({"thread_id":"thread","end_ordinal_exclusive":2,"end_byte_offset":retained.len()});
  let head = root.path().join("head.jsonl");
  let rows = std::iter::once(head_meta).chain(turn(3, "continued"));
  fs::write(&head, rows.map(|row| format!("{row}\n")).collect::<String>()).unwrap();
  assert_eq!(source.history_segments(&head).unwrap()[0].path, parent);
  let mut reader = CodexHistoryProjectionReader::new(head, 256 * 1024);
  assert_eq!(
    reader
      .poll(&source, |_, _| {})
      .unwrap()
      .unwrap()
      .reference
      .message_count,
    1
  );
}
