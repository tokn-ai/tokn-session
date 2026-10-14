use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokn_session_codex::{CodexHistoryReader, CodexSessionSource, history_header};
use tokn_session_core::{AgentEvent, NormalizedRecord, SessionHeader, SessionRef};

const SNAPSHOT_LIMIT: usize = 4 * 1024 * 1024;

fn meta(id: &str, ordinal: u64, base: Option<Value>) -> Value {
  json!({"type":"session_meta","ordinal":ordinal,"payload":{
    "id":id,"history_mode":"paginated","history_base":base,
    "timestamp":"2026-10-14T00:00:00Z","cwd":"/test"
  }})
}

fn message(id: &str, ordinal: u64, text: &str) -> Value {
  json!({"type":"event_msg","ordinal":ordinal,"payload":{
    "type":"item_completed","thread_id":id,"turn_id":format!("turn-{ordinal}"),
    "item":{"type":"UserMessage","id":format!("message-{ordinal}"),"content":[{"type":"text","text":text}]}
  }})
}

fn turn(id: &str, ordinal: u64, index: usize) -> Vec<Value> {
  vec![
    json!({"type":"event_msg","ordinal":ordinal,"payload":{
      "type":"task_started","turn_id":format!("turn-{index}")
    }}),
    message(id, ordinal + 1, &format!("prompt-{index}")),
    json!({"type":"event_msg","ordinal":ordinal + 2,"payload":{
      "type":"item_completed","thread_id":id,"turn_id":format!("turn-{index}"),
      "item":{"type":"AgentMessage","id":format!("answer-{index}"),"phase":"final_answer",
        "content":[{"type":"Text","text":format!("answer-{index}")}]}
    }}),
  ]
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
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  let decoded = bytes(rows);
  let stored = if name.ends_with(".zst") {
    zstd::stream::encode_all(decoded.as_slice(), 0).unwrap()
  } else {
    decoded
  };
  fs::write(&path, stored).unwrap();
  path
}

fn base(id: &str, rows: &[Value]) -> Value {
  json!({
    "thread_id":id,
    "end_ordinal_exclusive":rows.last().unwrap()["ordinal"].as_u64().unwrap() + 1,
    "end_byte_offset":bytes(rows).len()
  })
}

fn texts(records: &[NormalizedRecord]) -> Vec<String> {
  records
    .iter()
    .flat_map(|record| &record.events)
    .filter_map(|event| match event {
      AgentEvent::Message(message) => Some(message.text.clone()),
      _ => None,
    })
    .collect()
}

fn record_ids(records: &[NormalizedRecord]) -> Vec<String> {
  records.iter().map(|record| record.record_id.clone()).collect()
}

fn header(reference: SessionRef) -> SessionHeader {
  SessionHeader {
    id: reference.id,
    parent_session_id: reference.parent_session_id,
    agent_path: reference.agent_path,
    agent_nickname: reference.agent_nickname,
    agent_role: reference.agent_role,
    title: reference.title,
    preview: reference.preview,
    path: reference.path,
    cwd: reference.cwd,
    timestamp: reference.timestamp,
    updated_at: None,
    updated_at_ms: None,
  }
}

#[test]
fn compressed_standalone_rollout_loads_lists_and_hydrates_through_its_plain_alias() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let rows = [meta("thread", 0, None), message("thread", 4, "first prompt")];
  let compressed = write(root.path(), "archived/head.jsonl.zst", &rows);
  let plain = compressed.with_extension("");

  assert_eq!(source.session_paths().unwrap(), [compressed.clone()]);
  let owning = history_header(&compressed).unwrap();
  assert_eq!(owning.native()["payload"]["id"], "thread");
  let relations = source.list_session_relations().unwrap();
  assert_eq!(relations.len(), 1);
  assert_eq!(relations[0].path, compressed);
  assert_eq!(relations[0].message_count, 0);
  let hydrated = source.hydrate_session_header(header(relations[0].clone())).unwrap();
  assert_eq!(hydrated.preview.as_deref(), Some("first prompt"));
  let listed = source.list_sessions().unwrap();
  assert_eq!(listed.len(), 1);
  assert_eq!(listed[0].preview.as_deref(), Some("first prompt"));

  let loaded = source.load_session_records_path(&plain, true, SNAPSHOT_LIMIT).unwrap();
  assert_eq!(texts(&loaded.records), ["first prompt"]);
  assert_eq!(loaded.reference.path, compressed);
  assert!(loaded.records.iter().all(|record| record.native.is_some()));
  let session = source.load_session(plain.to_str().unwrap()).unwrap();
  assert_eq!(session.reference.path, compressed);
  assert_eq!(session.reference.message_count, 1);
  assert!(
    !plain.exists(),
    "reading compressed storage must not restore the plain rollout"
  );
}

#[test]
fn nested_mixed_history_resolves_compressed_physical_segment_alias() {
  const OWNER: &str = "01a0c8ab-3b96-7c40-adae-51c20a43c2cb";
  const SEGMENT: &str = "01a0c923-943a-7820-b93f-15e26e7e7cd4";
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let original = [meta(OWNER, 0, None), message(OWNER, 1, "original")];
  let original_path = write(root.path(), "old/original.jsonl", &original);
  let retained = [
    meta(OWNER, 2, Some(base(OWNER, &original))),
    message(OWNER, 3, "retained"),
  ];
  let mut middle = retained.to_vec();
  middle.push(message(OWNER, 4, "reverted"));
  let middle_path = write(
    root.path(),
    &format!("rollout-2026-09-22T20-42-27-{OWNER}_{SEGMENT}.jsonl.zst"),
    &middle,
  );
  let head = write(
    root.path(),
    "new/head.jsonl.zst",
    &[
      meta(OWNER, 4, Some(base(SEGMENT, &retained))),
      message(OWNER, 5, "current"),
    ],
  );

  let segments = source.history_segments(&head).unwrap();
  assert_eq!(
    segments.iter().map(|segment| &segment.path).collect::<Vec<_>>(),
    [&original_path, &middle_path, &head]
  );
  assert_eq!(segments[1].end_byte_offset, Some(bytes(&retained).len() as u64));
  let loaded = source.load_session_records_path(&head, true, SNAPSHOT_LIMIT).unwrap();
  assert_eq!(loaded.reference.id, OWNER);
  assert_eq!(texts(&loaded.records), ["original", "retained", "current"]);
  assert_eq!(source.list_session_relations().unwrap().len(), 1);

  fs::rename(
    middle_path,
    root
      .path()
      .join(format!("rollout-2026-09-22T20-42-27-{SEGMENT}_{SEGMENT}.jsonl.zst")),
  )
  .unwrap();
  assert!(source.history_segments(&head).unwrap_err().contains("unavailable"));
}

#[test]
fn compressed_before_turn_cutoff_verifies_excluded_ordinal_at_exact_decoded_boundary() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let retained = [meta("thread", 0, None), message("thread", 4, "retained")];
  let mut original = retained.to_vec();
  original.push(message("thread", 9, "excluded"));
  let old = write(root.path(), "old.jsonl.zst", &original);
  let mut decoy = retained.to_vec();
  decoy.push(message("thread", 8, "wrong excluded ordinal"));
  write(root.path(), "decoy.jsonl.zst", &decoy);
  let mut cutoff = base("thread", &retained);
  cutoff["end_ordinal_exclusive"] = json!(9);
  let head = write(
    root.path(),
    "head.jsonl",
    &[
      meta("thread", 9, Some(cutoff.clone())),
      message("thread", 15, "current"),
    ],
  );

  assert_eq!(source.history_segments(&head).unwrap()[0].path, old);
  let loaded = source.load_session_records_path(&head, false, SNAPSHOT_LIMIT).unwrap();
  assert_eq!(texts(&loaded.records), ["retained", "current"]);
  for invalid_offset in [bytes(&retained).len() - 1, bytes(&retained).len() + 1] {
    let mut invalid = cutoff.clone();
    invalid["end_byte_offset"] = json!(invalid_offset);
    write(root.path(), "head.jsonl", &[meta("thread", 9, Some(invalid))]);
    assert!(source.history_segments(&head).is_err());
    assert!(source.load_session_records_path(&head, false, SNAPSHOT_LIMIT).is_err());
  }
}

#[test]
fn concatenated_zstd_frames_load_fully_and_retain_cross_frame_prefixes() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let retained = [meta("thread", 0, None), message("thread", 1, "second-frame history")];
  let excluded = message("thread", 2, "third-frame excluded");
  let compressed = root.path().join("old.jsonl.zst");
  let mut frames = Vec::new();
  for row in retained.iter().chain(std::iter::once(&excluded)) {
    frames.extend(zstd::stream::encode_all(bytes(std::slice::from_ref(row)).as_slice(), 0).unwrap());
  }
  fs::write(&compressed, frames).unwrap();
  let standalone = source
    .load_session_records_path(&compressed, true, SNAPSHOT_LIMIT)
    .unwrap();
  assert_eq!(
    texts(&standalone.records),
    ["second-frame history", "third-frame excluded"]
  );

  let head = write(
    root.path(),
    "head.jsonl",
    &[
      meta("thread", 2, Some(base("thread", &retained))),
      message("thread", 3, "current"),
    ],
  );
  let segments = source.history_segments(&head).unwrap();
  assert_eq!(segments[0].path, compressed);
  assert_eq!(segments[0].end_byte_offset, Some(bytes(&retained).len() as u64));
  let inherited = source.load_session_records_path(&head, true, SNAPSHOT_LIMIT).unwrap();
  assert_eq!(texts(&inherited.records), ["second-frame history", "current"]);
  assert_eq!(inherited.records[1].record_id, standalone.records[1].record_id);
}

#[test]
fn corrupt_and_truncated_compressed_storage_never_publish_a_partial_snapshot() {
  for inherited in [false, true] {
    for truncated in [false, true] {
      let root = TempDir::new().unwrap();
      let source = CodexSessionSource::new(Some(root.path().into()));
      let prefix = [meta("thread", 0, None), message("thread", 1, "history")];
      let compressed = write(root.path(), "old.jsonl.zst", &prefix);
      let path = if inherited {
        write(
          root.path(),
          "head.jsonl",
          &[
            meta("thread", 2, Some(base("thread", &prefix))),
            message("thread", 3, "suffix"),
          ],
        )
      } else {
        compressed.clone()
      };
      let mut reader = CodexHistoryReader::new(path.clone(), true, SNAPSHOT_LIMIT);
      let good = reader.poll(&source).unwrap().unwrap();
      assert!(good.reset);
      let mut broken = fs::read(&compressed).unwrap();
      if truncated {
        broken.truncate(broken.len() - 1);
      } else {
        broken[..4].fill(0);
      }
      fs::write(&compressed, broken).unwrap();
      assert!(reader.poll(&source).is_err());
      assert!(source.load_session_records_path(&path, true, SNAPSHOT_LIMIT).is_err());
      assert!(source.load_session_path(&path).is_err());

      write(root.path(), "old.jsonl.zst", &prefix);
      let repaired = reader.poll(&source).unwrap().unwrap();
      assert!(repaired.reset);
      assert_eq!(record_ids(&repaired.records), record_ids(&good.records));
    }
  }
}

#[test]
fn snapshot_limit_counts_decoded_bytes_including_compressed_inherited_segments() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let prefix = [meta("thread", 0, None), message("thread", 1, &"x".repeat(128 * 1024))];
  let compressed = write(root.path(), "old.jsonl.zst", &prefix);
  let compressed_size = fs::metadata(&compressed).unwrap().len() as usize;
  assert!(compressed_size < 1024);
  let too_small = compressed_size + 1024;
  let error = source
    .load_session_records_path(&compressed, false, too_small)
    .unwrap_err();
  assert!(error.contains("size limit"), "{error}");

  let head = write(
    root.path(),
    "head.jsonl",
    &[
      meta("thread", 2, Some(base("thread", &prefix))),
      message("thread", 3, "suffix"),
    ],
  );
  let error = source
    .load_session_records_path(&head, false, bytes(&prefix).len())
    .unwrap_err();
  assert!(error.contains("size limit"), "{error}");
  assert_eq!(
    texts(
      &source
        .load_session_records_path(&head, false, SNAPSHOT_LIMIT)
        .unwrap()
        .records
    ),
    ["x".repeat(128 * 1024), "suffix".into()]
  );
}

#[test]
fn lazy_windows_select_latest_turns_and_expand_across_compressed_prefixes() {
  for head_name in ["head.jsonl", "head.jsonl.zst"] {
    let root = TempDir::new().unwrap();
    let source = CodexSessionSource::new(Some(root.path().into()));
    let mut prefix = vec![meta("thread", 0, None)];
    prefix.extend(turn("thread", 1, 0));
    prefix.push(json!({"type":"unknown","ordinal":4,"payload":{"data":"x".repeat(1024 * 1024)}}));
    prefix.extend(turn("thread", 5, 1));
    write(root.path(), "old.jsonl.zst", &prefix);
    let mut current = vec![meta("thread", 8, Some(base("thread", &prefix)))];
    current.extend(turn("thread", 9, 2));
    let head = write(root.path(), head_name, &current);
    let mut reader = CodexHistoryReader::new_window(head, false, 64 * 1024, 1);

    let latest = reader.poll(&source).unwrap().unwrap();
    assert!(latest.reset);
    assert!(latest.source_start.unwrap() > bytes(&prefix).len() as u64);
    assert_eq!(texts(&latest.records), ["prompt-2", "answer-2"]);
    assert_eq!(latest.records.len(), 3);
    assert!(reader.stats().rows_parsed < 10);
    assert!(reader.poll(&source).unwrap().is_none());
    reader.expand(Some(1));
    let expanded = reader.poll(&source).unwrap().unwrap();
    assert!(expanded.reset);
    assert_eq!(
      texts(&expanded.records),
      ["prompt-1", "answer-1", "prompt-2", "answer-2"]
    );
    assert_eq!(expanded.records.len(), 6);
    assert_eq!(record_ids(&expanded.records[3..]), record_ids(&latest.records));
  }
}

#[test]
fn plain_suffix_remains_incremental_after_compressed_history() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let prefix = [meta("thread", 0, None), message("thread", 1, &"x".repeat(1024 * 1024))];
  write(root.path(), "old.jsonl.zst", &prefix);
  let head = write(
    root.path(),
    "head.jsonl",
    &[
      meta("thread", 2, Some(base("thread", &prefix))),
      message("thread", 3, "current"),
    ],
  );
  let mut reader = CodexHistoryReader::new(head.clone(), true, SNAPSHOT_LIMIT);
  let first = reader.poll(&source).unwrap().unwrap();
  let before = reader.stats();
  let appended = bytes(&[message("thread", 4, "appended")]);
  fs::OpenOptions::new()
    .append(true)
    .open(&head)
    .unwrap()
    .write_all(&appended)
    .unwrap();
  let delta = reader.poll(&source).unwrap().unwrap();
  assert!(!delta.reset);
  assert_eq!(texts(&delta.records), ["appended"]);
  assert_eq!(
    reader.stats().source_bytes_read - before.source_bytes_read,
    appended.len() as u64
  );
  assert_eq!(reader.stats().lineage_resolutions, before.lineage_resolutions);
  let loaded = source.load_session_records_path(&head, true, SNAPSHOT_LIMIT).unwrap();
  assert_eq!(
    record_ids(&loaded.records[..first.records.len()]),
    record_ids(&first.records)
  );
  assert_eq!(loaded.records.last().unwrap().record_id, delta.records[0].record_id);
  assert!(reader.poll(&source).unwrap().is_none());
}

#[test]
fn compressed_replacement_of_plain_prefix_rebuilds_without_changing_record_ids() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let prefix = [meta("thread", 0, None), message("thread", 1, "history")];
  let plain = write(root.path(), "old.jsonl", &prefix);
  let head = write(
    root.path(),
    "head.jsonl",
    &[
      meta("thread", 2, Some(base("thread", &prefix))),
      message("thread", 3, "current"),
    ],
  );
  let mut reader = CodexHistoryReader::new(head.clone(), true, SNAPSHOT_LIMIT);
  let before = reader.poll(&source).unwrap().unwrap();
  let compressed = write(root.path(), "old.jsonl.zst", &prefix);
  fs::remove_file(&plain).unwrap();
  let replaced = reader.poll(&source).unwrap().unwrap();
  assert!(replaced.reset);
  assert_eq!(texts(&replaced.records), ["history", "current"]);
  assert_eq!(record_ids(&replaced.records), record_ids(&before.records));
  assert_eq!(source.history_segments(&head).unwrap()[0].path, compressed);
  assert!(!plain.exists());
  assert!(reader.poll(&source).unwrap().is_none());
}

#[test]
fn plain_and_compressed_twins_prefer_plain_without_ambiguous_history() {
  let root = TempDir::new().unwrap();
  let source = CodexSessionSource::new(Some(root.path().into()));
  let prefix = [meta("thread", 0, None), message("thread", 1, "history")];
  let old = write(root.path(), "old.jsonl", &prefix);
  write(root.path(), "old.jsonl.zst", &prefix);
  let current = [
    meta("thread", 2, Some(base("thread", &prefix))),
    message("thread", 3, "current"),
  ];
  let head = write(root.path(), "head.jsonl", &current);
  let compressed_head = write(root.path(), "head.jsonl.zst", &current);

  assert_eq!(source.session_paths().unwrap(), [head.clone(), old.clone()]);
  let segments = source.history_segments(&head).unwrap();
  assert_eq!(
    segments.iter().map(|segment| &segment.path).collect::<Vec<_>>(),
    [&old, &head]
  );
  assert_eq!(source.list_session_relations().unwrap().len(), 1);
  let loaded = source
    .load_session_records_path(&compressed_head, true, SNAPSHOT_LIMIT)
    .unwrap();
  assert_eq!(loaded.reference.path, head);
  assert_eq!(texts(&loaded.records), ["history", "current"]);

  fs::write(root.path().join("old.jsonl.zst"), b"stale compressed storage").unwrap();
  fs::write(compressed_head, b"stale compressed storage").unwrap();
  assert_eq!(source.list_sessions().unwrap().len(), 1);
  assert_eq!(source.history_segments(&head).unwrap()[0].path, old);
}
