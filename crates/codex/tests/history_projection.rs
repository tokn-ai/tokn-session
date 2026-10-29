use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokn_session_codex::{CodexHistoryProjectionReader, CodexHistoryReader, CodexSessionSource};
use tokn_session_core::{AgentEvent, NormalizedRecord, SessionHistoryStatus};

fn meta(id: &str, ordinal: u64, base: Option<Value>) -> Value {
  json!({"type":"session_meta","ordinal":ordinal,"payload":{
    "id":id,"history_mode":"paginated","history_base":base,
    "timestamp":"2026-10-30T00:00:00Z","cwd":"/test"
  }})
}

fn message(id: &str, ordinal: u64, text: &str) -> Value {
  json!({"type":"event_msg","ordinal":ordinal,"payload":{
    "type":"item_completed","thread_id":id,"turn_id":format!("turn-{ordinal}"),
    "item":{"type":"AgentMessage","id":format!("answer-{ordinal}"),"phase":"final_answer",
      "content":[{"type":"Text","text":text}]}
  }})
}

fn bytes(rows: &[Value]) -> Vec<u8> {
  rows
    .iter()
    .map(|row| format!("{row}\n"))
    .collect::<String>()
    .into_bytes()
}

fn write(root: &Path, name: &str, rows: &[Value]) -> PathBuf {
  let path = root.join(name);
  let decoded = bytes(rows);
  let stored = if name.ends_with(".zst") {
    zstd::stream::encode_all(decoded.as_slice(), 0).unwrap()
  } else {
    decoded
  };
  fs::write(&path, stored).unwrap();
  path
}

fn append(path: &Path, bytes: &[u8]) {
  fs::OpenOptions::new()
    .append(true)
    .open(path)
    .unwrap()
    .write_all(bytes)
    .unwrap();
}

fn base(id: &str, rows: &[Value]) -> Value {
  json!({
    "thread_id":id,
    "end_ordinal_exclusive":rows.last().unwrap()["ordinal"].as_u64().unwrap() + 1,
    "end_byte_offset":bytes(rows).len()
  })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Totals {
  messages: usize,
  latest: String,
  resets: usize,
}

impl Totals {
  fn consume(&mut self, reset: bool, record: NormalizedRecord) {
    assert!(record.native.is_none(), "projections must not retain the source JSON");
    if reset {
      *self = Self {
        resets: self.resets + 1,
        ..Self::default()
      };
    }
    for event in record.events {
      if let AgentEvent::Message(message) = event {
        self.messages += 1;
        self.latest = message.text;
      }
    }
  }
}

#[test]
fn inherited_projection_exceeds_snapshot_budget_without_losing_lifetime_totals() {
  const BUDGET: usize = 4096;
  for compressed in [false, true] {
    let root = TempDir::new().unwrap();
    let source = CodexSessionSource::new(Some(root.path().into()));
    let mut previous = None;
    let mut previous_id = String::new();
    let mut ordinal = 0;
    let mut source_bytes = 0;
    let mut head = PathBuf::new();
    let mut head_rows = Vec::new();
    for segment in 0..5 {
      let id = format!("segment-{segment}");
      let inherited = previous.as_ref().map(|rows: &Vec<Value>| base(&previous_id, rows));
      let mut rows = vec![meta(&id, ordinal, inherited)];
      for _ in 0..4 {
        ordinal += 1;
        rows.push(message(&id, ordinal, &format!("answer-{ordinal}")));
      }
      ordinal += 1;
      assert!(bytes(&rows).len() < BUDGET);
      source_bytes += bytes(&rows).len();
      let extension = if compressed { "jsonl.zst" } else { "jsonl" };
      head = write(root.path(), &format!("segment-{segment}.{extension}"), &rows);
      previous_id = id;
      head_rows = rows.clone();
      previous = Some(rows);
    }
    assert!(source_bytes > BUDGET);
    let mut snapshot = CodexHistoryReader::new(head.clone(), false, BUDGET);
    assert!(snapshot.poll(&source).err().unwrap().contains("Codex history exceeds"));

    let mut projection = CodexHistoryProjectionReader::new(head.clone(), BUDGET);
    let mut totals = Totals::default();
    let update = projection
      .poll(&source, |reset, record| totals.consume(reset, record))
      .unwrap()
      .unwrap();
    assert!(update.reset);
    assert_eq!(update.reference.id, "segment-4");
    assert_eq!(update.reference.message_count, 20);
    assert_eq!(update.history_status, SessionHistoryStatus::Complete);
    assert_eq!(
      totals,
      Totals {
        messages: 20,
        latest: "answer-24".into(),
        resets: 1
      }
    );
    assert_eq!(projection.stats().source_bytes_read, source_bytes as u64);
    assert!(
      projection
        .poll(&source, |_, _| panic!("unchanged projection emitted a record"))
        .unwrap()
        .is_none()
    );

    let next = message("segment-4", ordinal, "new answer");
    if compressed {
      head_rows.push(next);
      write(root.path(), head.file_name().unwrap().to_str().unwrap(), &head_rows);
    } else {
      append(&head, &bytes(&[next]));
    }
    let update = projection
      .poll(&source, |reset, record| totals.consume(reset, record))
      .unwrap()
      .unwrap();
    assert_eq!(update.reset, compressed);
    assert_eq!(totals.messages, 21);
    assert_eq!(totals.latest, "new answer");
    assert_eq!(totals.resets, if compressed { 2 } else { 1 });
  }
}

#[test]
fn projection_keeps_exact_cutoffs_and_reads_only_appended_fragments() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let retained = vec![meta("parent", 0, None), message("parent", 1, "retained")];
  let mut parent_rows = retained.clone();
  parent_rows.push(message("parent", 2, "excluded"));
  let parent = write(root.path(), "parent.jsonl", &parent_rows);
  let head = write(
    root.path(),
    "head.jsonl",
    &[
      meta("child", 2, Some(base("parent", &retained))),
      message("child", 3, "current"),
    ],
  );
  let mut reader = CodexHistoryProjectionReader::new(head.clone(), 4096);
  let mut totals = Totals::default();
  reader
    .poll(&source, |reset, record| totals.consume(reset, record))
    .unwrap();
  assert_eq!(totals.messages, 2);
  append(&parent, &bytes(&[message("parent", 3, "also excluded")]));
  assert!(
    reader
      .poll(&source, |_, _| panic!("growth beyond cutoff must be ignored"))
      .unwrap()
      .is_none()
  );

  let before = reader.stats();
  let next = bytes(&[message("child", 4, "completed fragment")]);
  append(&head, &next[..next.len() / 2]);
  assert!(
    reader
      .poll(&source, |_, _| panic!("partial row emitted"))
      .unwrap()
      .is_none()
  );
  append(&head, &next[next.len() / 2..]);
  let update = reader
    .poll(&source, |reset, record| totals.consume(reset, record))
    .unwrap()
    .unwrap();
  assert!(!update.reset);
  assert_eq!(totals.messages, 3);
  assert_eq!(totals.latest, "completed fragment");
  assert_eq!(
    reader.stats().source_bytes_read - before.source_bytes_read,
    next.len() as u64
  );
  assert_eq!(reader.stats().lineage_resolutions, before.lineage_resolutions);

  write(
    root.path(),
    "head.jsonl",
    &[meta("child", 0, None), message("child", 1, "replacement")],
  );
  let update = reader
    .poll(&source, |reset, record| totals.consume(reset, record))
    .unwrap()
    .unwrap();
  assert!(update.reset);
  assert_eq!(totals.messages, 1);
  assert_eq!(totals.latest, "replacement");
  assert_eq!(totals.resets, 2);
}

#[test]
fn failed_projection_discards_its_cursor_and_rebuilds_after_repair() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let mut rows = vec![meta("thread", 0, None), message("thread", 1, "published")];
  let head = write(root.path(), "head.jsonl", &rows);
  let mut reader = CodexHistoryProjectionReader::new(head.clone(), 4096);
  let mut published = Totals::default();
  reader
    .poll(&source, |reset, record| published.consume(reset, record))
    .unwrap();
  rows.push(message("thread", 2, "staged"));
  append(&head, &bytes(&rows[2..]));
  append(&head, b"{malformed}\n");
  let mut staging = published.clone();
  assert!(
    reader
      .poll(&source, |reset, record| staging.consume(reset, record))
      .err()
      .unwrap()
      .contains("invalid Codex history")
  );
  assert_eq!(staging.messages, 2, "the sink sees records before final validation");
  assert_eq!(published.messages, 1, "callers must discard staged results on errors");

  write(root.path(), "head.jsonl", &rows);
  let update = reader
    .poll(&source, |reset, record| published.consume(reset, record))
    .unwrap()
    .unwrap();
  assert!(update.reset);
  assert_eq!(published.messages, 2);
  assert_eq!(published.latest, "staged");
  assert_eq!(published.resets, 2);
}

#[test]
fn projection_bounds_complete_and_incomplete_rows_before_allocating_the_source() {
  const RECORD_LIMIT: usize = 512;
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let rows = vec![meta("thread", 0, None), message("thread", 1, "small")];
  let head = write(root.path(), "head.jsonl", &rows);
  let oversized = bytes(&[message("thread", 2, &"x".repeat(2048))]);
  append(&head, &oversized);
  let mut reader = CodexHistoryProjectionReader::new(head.clone(), RECORD_LIMIT);
  assert!(
    reader
      .poll(&source, |_, _| {})
      .err()
      .unwrap()
      .contains("history row exceeds")
  );
  assert_eq!(
    reader.stats().source_bytes_read,
    bytes(&rows).len() as u64 + RECORD_LIMIT as u64 + 1
  );

  write(root.path(), "head.jsonl", &rows);
  reader.poll(&source, |_, _| {}).unwrap();
  let before = reader.stats().source_bytes_read;
  append(&head, &oversized[..RECORD_LIMIT]);
  assert!(
    reader
      .poll(&source, |_, _| panic!("partial row emitted"))
      .unwrap()
      .is_none()
  );
  append(&head, &oversized[RECORD_LIMIT..]);
  assert!(
    reader
      .poll(&source, |_, _| {})
      .err()
      .unwrap()
      .contains("history row exceeds")
  );
  assert_eq!(reader.stats().source_bytes_read - before, RECORD_LIMIT as u64 + 1);
}

#[test]
fn projection_validates_all_inherited_ordinals_even_when_the_tail_is_valid() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let parent = vec![
    meta("parent", 0, None),
    message("parent", 4, "first"),
    message("parent", 3, "backwards"),
  ];
  write(root.path(), "parent.jsonl.zst", &parent);
  let head = write(
    root.path(),
    "head.jsonl",
    &[
      meta("child", 4, Some(base("parent", &parent))),
      message("child", 5, "valid tail"),
    ],
  );
  let mut reader = CodexHistoryProjectionReader::new(head, 4096);
  assert!(reader.poll(&source, |_, _| {}).err().unwrap().contains("ordinal"));
}

#[test]
fn projection_preserves_spawned_thread_filtering() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let parent = vec![meta("parent", 0, None), message("parent", 1, "parent message")];
  write(root.path(), "parent.jsonl.zst", &parent);
  let mut owner = meta("child", 2, Some(base("parent", &parent)));
  owner["payload"]["source"] = json!({"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}});
  owner["payload"]["parent_thread_id"] = json!("parent");
  let head = write(
    root.path(),
    "head.jsonl",
    &[
      owner,
      message("child", 3, "copied message"),
      json!({"type":"inter_agent_communication_metadata","ordinal":4,"payload":{"trigger_turn":true}}),
      message("child", 5, "child message"),
    ],
  );
  let mut reader = CodexHistoryProjectionReader::new(head, 4096);
  let mut totals = Totals::default();
  let update = reader
    .poll(&source, |reset, record| totals.consume(reset, record))
    .unwrap()
    .unwrap();
  assert_eq!(update.history_status, SessionHistoryStatus::FilteredSubagent);
  assert_eq!(totals.messages, 1);
  assert_eq!(totals.latest, "child message");
}
