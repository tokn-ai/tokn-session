use super::*;
use tempfile::TempDir;

/// Read-only local diagnostic; prints timings/counts, never session contents.
/// TOKN_PROFILE_SOURCE points to JSON containing source_path and session_id.
#[test]
#[ignore = "requires an explicitly selected local Codex source"]
fn profile_local_codex_open() {
  let metadata = std::env::var("TOKN_PROFILE_SOURCE").expect("Set TOKN_PROFILE_SOURCE");
  let metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(metadata).unwrap()).unwrap();
  let path = PathBuf::from(metadata["source_path"].as_str().unwrap());
  let root = std::env::var_os("TOKN_PROFILE_ROOT")
    .map(PathBuf::from)
    .expect("Set TOKN_PROFILE_ROOT");
  let source = tokn_session_codex::CodexSessionSource::new(Some(root.clone()));
  let start = std::time::Instant::now();
  let segments = source.history_segments(&path).unwrap();
  eprintln!("lineage_ms={} segments={}", start.elapsed().as_millis(), segments.len());
  let start = std::time::Instant::now();
  let mut history =
    tokn_session_codex::CodexHistoryReader::new(path.clone(), false, crate::service_protocol::MAX_SNAPSHOT_BYTES);
  let decoded = history.poll(&source).unwrap().unwrap();
  eprintln!(
    "decode_ms={} records={}",
    start.elapsed().as_millis(),
    decoded.records.len()
  );
  drop(decoded);
  drop(history);
  let start = std::time::Instant::now();
  let reader = SessionReader::new_with_mode(
    CatalogEntry {
      key: "profile".into(),
      provider: Provider::Codex,
      header: serde_json::from_value(serde_json::json!({"id": metadata["session_id"], "path": path})).unwrap(),
    },
    false,
    root,
    None,
    true,
  )
  .unwrap();
  eprintln!(
    "lazy_reader_ms={} records={} events={}",
    start.elapsed().as_millis(),
    reader.snapshot.records.len(),
    reader.snapshot.records.events
  );
  let latest = reader.snapshot.records.window_start(None, None);
  eprintln!("initial_delivery_events={}", reader.snapshot.records.events - latest);
  if let Some(history) = &reader.codex_history {
    eprintln!("read_stats={:?}", history.stats());
  }
}

struct CodexHistoryFixture {
  directory: TempDir,
  base_path: PathBuf,
  head_path: PathBuf,
  base: String,
}

impl CodexHistoryFixture {
  fn new() -> Self {
    let directory = TempDir::new().unwrap();
    let base_path = directory.path().join("rollout-base-linked.jsonl");
    let head_path = directory.path().join("rollout-head-linked.jsonl");
    let base = format!(
      "{}\n{}\n",
      serde_json::json!({"ordinal": 0, "type": "session_meta", "payload": {
        "id": "linked", "history_mode": "paginated", "cwd": "/tmp"
      }}),
      Self::message(1, "old hello")
    );
    std::fs::write(&base_path, &base).unwrap();
    let head = format!(
      "{}\n{}\n",
      serde_json::json!({"ordinal": 2, "type": "session_meta", "payload": {
        "id": "linked", "history_mode": "paginated", "cwd": "/tmp",
        "history_base": {"thread_id": "linked", "end_ordinal_exclusive": 2, "end_byte_offset": base.len()}
      }}),
      Self::message(3, "new message")
    );
    std::fs::write(&head_path, head).unwrap();
    Self {
      directory,
      base_path,
      head_path,
      base,
    }
  }

  fn message(ordinal: u64, text: &str) -> String {
    serde_json::json!({"ordinal": ordinal, "type": "event_msg", "payload": {
      "type": "item_completed", "thread_id": "linked", "turn_id": format!("turn-{ordinal}"),
      "item": {"type": "UserMessage", "id": format!("message-{ordinal}"),
        "content": [{"type": "text", "text": text}]}
    }})
    .to_string()
  }

  fn reader(&self, native: bool) -> SessionReader {
    SessionReader::new(
      CatalogEntry {
        key: "linked".into(),
        provider: Provider::Codex,
        header: serde_json::from_value(serde_json::json!({"id": "linked", "path": self.head_path})).unwrap(),
      },
      native,
      self.directory.path().into(),
    )
    .unwrap()
  }

  fn messages(reader: &SessionReader) -> Vec<String> {
    reader
      .snapshot
      .records
      .iter()
      .flat_map(|record| record.record.events)
      .filter_map(|event| {
        if let tokn_session_core::AgentEvent::Message(message) = event {
          Some(message.text)
        } else {
          None
        }
      })
      .collect()
  }
}

#[test]
fn linked_codex_history_survives_appends_and_buffers_partial_rows() {
  use std::io::Write;
  let fixture = CodexHistoryFixture::new();
  let mut reader = fixture.reader(true);
  assert_eq!(CodexHistoryFixture::messages(&reader), ["old hello", "new message"]);
  assert!(
    reader
      .snapshot
      .records
      .iter()
      .all(|record| record.path == fixture.head_path)
  );
  assert!(
    reader
      .snapshot
      .records
      .iter()
      .all(|record| record.record.native.is_some())
  );
  let initial = reader.snapshot.clone();
  let mut file = std::fs::OpenOptions::new()
    .append(true)
    .open(&fixture.head_path)
    .unwrap();
  writeln!(file, "{}", CodexHistoryFixture::message(4, "next message")).unwrap();
  assert!(reader.poll().unwrap());
  assert_eq!(reader.snapshot.generation, initial.generation);
  assert_eq!(
    CodexHistoryFixture::messages(&reader),
    ["old hello", "new message", "next message"]
  );
  assert!(initial.records.same_journal(&reader.snapshot.records));
  write!(file, "{}", CodexHistoryFixture::message(5, "partial message")).unwrap();
  assert!(!reader.poll().unwrap());
  writeln!(file).unwrap();
  assert!(reader.poll().unwrap());
  assert_eq!(reader.snapshot.generation, initial.generation);
  assert_eq!(
    CodexHistoryFixture::messages(&reader).last(),
    Some(&"partial message".to_owned())
  );
}

#[test]
fn linked_codex_prefix_edits_reset_and_missing_history_preserves_last_good_snapshot() {
  let fixture = CodexHistoryFixture::new();
  let mut reader = fixture.reader(false);
  let initial = reader.snapshot.clone();
  std::fs::write(&fixture.base_path, fixture.base.replace("old hello", "new hello")).unwrap();
  // File timestamps can otherwise coalesce writes on some test filesystems.
  std::fs::File::open(&fixture.base_path)
    .unwrap()
    .set_modified(SystemTime::now() + std::time::Duration::from_secs(1))
    .unwrap();
  assert!(reader.poll().unwrap());
  assert_ne!(reader.snapshot.generation, initial.generation);
  assert_eq!(CodexHistoryFixture::messages(&reader), ["new hello", "new message"]);
  let last_good = reader.snapshot.clone();
  std::fs::remove_file(&fixture.base_path).unwrap();
  assert!(reader.poll().is_err());
  assert_eq!(reader.snapshot.generation, last_good.generation);
  assert_eq!(reader.snapshot.revision, last_good.revision);
  assert_eq!(CodexHistoryFixture::messages(&reader), ["new hello", "new message"]);
}

#[test]
fn linked_codex_failed_batch_does_not_advance_the_published_snapshot() {
  use std::io::Write;
  let fixture = CodexHistoryFixture::new();
  let mut reader = fixture.reader(false);
  let initial = reader.snapshot.clone();
  let head = std::fs::read_to_string(&fixture.head_path).unwrap();
  let good_row = CodexHistoryFixture::message(4, "survives retry");
  let mut file = std::fs::OpenOptions::new()
    .append(true)
    .open(&fixture.head_path)
    .unwrap();
  writeln!(file, "{good_row}\nnot json").unwrap();
  assert!(reader.poll().is_err());
  assert!(reader.poll().is_err());
  assert_eq!(reader.snapshot.revision, initial.revision);
  assert_eq!(CodexHistoryFixture::messages(&reader), ["old hello", "new message"]);
  std::fs::write(&fixture.head_path, format!("{head}{good_row}\n")).unwrap();
  assert!(reader.poll().unwrap());
  assert_ne!(reader.snapshot.generation, initial.generation);
  assert_eq!(
    CodexHistoryFixture::messages(&reader),
    ["old hello", "new message", "survives retry"]
  );
}

#[test]
fn linked_codex_parent_growth_does_not_refresh_or_replace_the_child_snapshot() {
  use std::io::Write;
  let fixture = CodexHistoryFixture::new();
  let mut reader = fixture.reader(false);
  let initial = reader.snapshot.clone();
  let mut parent = std::fs::OpenOptions::new()
    .append(true)
    .open(&fixture.base_path)
    .unwrap();
  writeln!(parent, "{}", CodexHistoryFixture::message(2, "later parent work")).unwrap();
  assert!(!reader.poll().unwrap());
  assert_eq!(reader.snapshot.generation, initial.generation);
  assert_eq!(reader.snapshot.revision, initial.revision);
  assert!(initial.records.same_journal(&reader.snapshot.records));
  assert_eq!(CodexHistoryFixture::messages(&reader), ["old hello", "new message"]);
}

struct Fixture {
  directory: TempDir,
  path: PathBuf,
  database: rusqlite::Connection,
}

#[test]
fn plain_codex_rejected_batch_retries_without_losing_or_duplicating_messages() {
  use std::io::Write;
  let directory = TempDir::new().unwrap();
  let path = directory.path().join("rollout-plain.jsonl");
  let message =
    |text: &str| serde_json::json!({"type":"event_msg", "payload":{"type":"user_message", "message":text}}).to_string();
  let initial = format!(
    "{}\n{}\n",
    serde_json::json!({"type":"session_meta", "payload":{"id":"linked", "cwd":"/tmp"}}),
    message("initial message")
  );
  std::fs::write(&path, &initial).unwrap();
  let entry = CatalogEntry {
    key: "plain".into(),
    provider: Provider::Codex,
    header: serde_json::from_value(serde_json::json!({"id":"linked", "path":path})).unwrap(),
  };
  let mut reader = SessionReader::new(entry, false, directory.path().into()).unwrap();
  let before = reader.snapshot.clone();
  let good = message("recovered message");
  writeln!(
    std::fs::OpenOptions::new().append(true).open(&path).unwrap(),
    "{good}\ninvalid"
  )
  .unwrap();
  assert!(reader.poll().is_err());
  assert!(reader.poll().is_err(), "retry must inspect the rejected batch again");
  assert_eq!(reader.snapshot.revision, before.revision);
  assert_eq!(CodexHistoryFixture::messages(&reader), ["initial message"]);
  std::fs::write(&path, format!("{initial}{good}\n")).unwrap();
  assert!(reader.poll().unwrap());
  assert_eq!(
    CodexHistoryFixture::messages(&reader),
    ["initial message", "recovered message"]
  );
  assert!(!reader.poll().unwrap());
}

#[test]
fn cancelled_reader_skips_initial_source_access_and_later_polls() {
  let fixture = Fixture::new();
  let entry = fixture.reader(false).snapshot.entry;
  let cancelled = CancellationToken::new();
  cancelled.cancel();
  let mut missing = entry.clone();
  missing.header.path = fixture.directory.path().join("missing.db");
  let error = SessionReader::new_cancellable(missing, false, fixture.directory.path().into(), cancelled)
    .err()
    .unwrap();
  assert!(
    error.contains("cancelled"),
    "cancellation wins over missing source: {error}"
  );
  let cancel = CancellationToken::new();
  let mut reader =
    SessionReader::new_cancellable(entry, false, fixture.directory.path().into(), cancel.clone()).unwrap();
  let before = reader.snapshot.clone();
  cancel.cancel();
  assert!(reader.poll().unwrap_err().contains("cancelled"));
  assert_eq!(reader.snapshot.revision, before.revision);
  assert_eq!(reader.database_reads, 1);
}

impl Fixture {
  fn new() -> Self {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("opencode.db");
    let database = rusqlite::Connection::open(&path).unwrap();
    database.execute_batch(r#"
      pragma journal_mode = wal;
      create table session (id text primary key, parent_id text, directory text, title text, time_created integer, time_updated integer);
      create table message (id text primary key, session_id text, time_created integer, data text);
      create table part (id text primary key, message_id text, session_id text, time_created integer, data text);
      insert into session values ('one', null, '/tmp', null, 1, 1), ('other', null, '/tmp', null, 1, 1);
      insert into message values ('m1', 'one', 1, '{"role":"user"}');
      insert into part values ('p1', 'm1', 'one', 1, '{"type":"text","text":"hello"}');
    "#).unwrap();
    Self {
      directory,
      path,
      database,
    }
  }

  fn reader(&self, native: bool) -> SessionReader {
    let source = OpenCodeSessionSource::new(Some(self.path.clone()));
    let header = source
      .list_session_headers()
      .unwrap()
      .into_iter()
      .find(|h| h.id == "one")
      .unwrap();
    SessionReader::new(
      CatalogEntry {
        key: "one".into(),
        provider: Provider::OpenCode,
        header,
      },
      native,
      self.directory.path().into(),
    )
    .unwrap()
  }

  fn poll(&self, reader: &mut SessionReader) -> Result<bool, String> {
    // Force reconciliation, including the case where a platform's mtime
    // granularity coalesces test writes. No timing-dependent sleeps needed.
    reader.poll_database(versions(&self.path, true))
  }
}

#[test]
fn unrelated_writes_and_wal_checkpoint_do_not_publish_or_reset() {
  let fixture = Fixture::new();
  for native in [false, true] {
    let mut reader = fixture.reader(native);
    let initial = reader.snapshot.clone();
    assert_eq!(reader.database_reads, 1);
    assert!(!reader.poll().unwrap());
    assert_eq!(reader.database_reads, 1, "unchanged versions must not read SQLite rows");
    fixture
      .database
      .execute_batch("update session set time_updated = time_updated + 1 where id = 'other'")
      .unwrap();
    assert!(!fixture.poll(&mut reader).unwrap());
    fixture
      .database
      .execute_batch("pragma wal_checkpoint(truncate)")
      .unwrap();
    assert!(!fixture.poll(&mut reader).unwrap());
    assert_eq!(reader.snapshot.generation, initial.generation);
    assert_eq!(reader.snapshot.revision, initial.revision);
    assert!(initial.records.same_journal(&reader.snapshot.records));
  }
}

#[test]
fn append_keeps_generation_and_reuses_history_despite_session_timestamp_change() {
  let fixture = Fixture::new();
  let mut reader = fixture.reader(false);
  let initial = reader.snapshot.clone();
  fixture
    .database
    .execute_batch(
      r#"
    insert into message values ('m2', 'one', 2, '{"role":"user"}');
    insert into part values ('p2', 'm2', 'one', 2, '{"type":"text","text":"world"}');
    update session set time_updated = 2 where id = 'one';
  "#,
    )
    .unwrap();
  assert!(fixture.poll(&mut reader).unwrap());
  assert_eq!(reader.snapshot.generation, initial.generation);
  assert_eq!(reader.snapshot.records.len(), 3);
  assert!(initial.records.same_journal(&reader.snapshot.records));
  assert_eq!(reader.snapshot.entry.header.timestamp.as_deref(), Some("1"));
  assert_eq!(reader.snapshot.entry.header.updated_at.as_deref(), Some("2"));
  assert!(!fixture.poll(&mut reader).unwrap());
}

#[test]
fn native_only_changes_are_visible_only_when_requested() {
  let fixture = Fixture::new();
  let mut plain = fixture.reader(false);
  let mut native = fixture.reader(true);
  let initial = native.snapshot.generation.clone();
  fixture
    .database
    .execute_batch(r#"update message set data = '{"role":"user","future_field":42}' where id = 'm1'"#)
    .unwrap();
  assert!(!fixture.poll(&mut plain).unwrap());
  assert!(fixture.poll(&mut native).unwrap());
  assert_ne!(native.snapshot.generation, initial);
  let initial = native.snapshot.generation.clone();
  fixture
    .database
    .execute_batch("update session set time_updated = 2 where id = 'one'")
    .unwrap();
  let plain_generation = plain.snapshot.generation.clone();
  assert!(fixture.poll(&mut plain).unwrap(), "metadata-only commit");
  assert_eq!(plain.snapshot.generation, plain_generation);
  assert!(fixture.poll(&mut native).unwrap());
  assert_ne!(native.snapshot.generation, initial);
}

#[test]
fn edits_deletions_and_reordering_reset_even_without_timestamp_changes() {
  let fixture = Fixture::new();
  let mut reader = fixture.reader(false);
  for sql in [
    // Same-length edit in this session plus a timestamp change elsewhere.
    r#"update part set data = '{"type":"text","text":"edits"}' where id = 'p1'; update session set time_updated = 2 where id = 'other';"#,
    r#"insert into message values ('m0', 'one', 0, '{"role":"user"}')"#,
    "delete from message where id = 'm1'",
  ] {
    let generation = reader.snapshot.generation.clone();
    fixture.database.execute_batch(sql).unwrap();
    assert!(fixture.poll(&mut reader).unwrap());
    assert_ne!(reader.snapshot.generation, generation);
    let fresh = fixture.reader(false);
    assert_eq!(
      serde_json::to_value(reader.snapshot.records.iter().map(|r| r.record).collect::<Vec<_>>()).unwrap(),
      serde_json::to_value(fresh.snapshot.records.iter().map(|r| r.record).collect::<Vec<_>>()).unwrap()
    );
  }
}

#[test]
fn malformed_update_preserves_last_good_snapshot_and_recovers() {
  let fixture = Fixture::new();
  let mut reader = fixture.reader(false);
  let initial = reader.snapshot.clone();
  fixture
    .database
    .execute_batch("update part set data = 'invalid'")
    .unwrap();
  assert!(fixture.poll(&mut reader).is_err());
  assert_eq!(reader.snapshot.generation, initial.generation);
  assert_eq!(reader.snapshot.revision, initial.revision);
  fixture
    .database
    .execute_batch(r#"update part set data = '{"type":"text","text":"hello"}'"#)
    .unwrap();
  assert!(!fixture.poll(&mut reader).unwrap());
}

#[cfg(unix)]
#[test]
fn replaced_database_resets_even_when_contents_match() {
  let fixture = Fixture::new();
  let mut reader = fixture.reader(false);
  let initial = reader.snapshot.generation.clone();
  fixture
    .database
    .execute_batch("pragma wal_checkpoint(truncate)")
    .unwrap();
  let replacement = fixture.directory.path().join("replacement.db");
  std::fs::copy(&fixture.path, &replacement).unwrap();
  drop(fixture.database);
  std::fs::rename(replacement, &fixture.path).unwrap();
  assert!(reader.poll().unwrap());
  assert_ne!(reader.snapshot.generation, initial);
}

#[test]
fn zcode_uses_its_identity_and_cached_sqlite_reconciliation() {
  let fixture = Fixture::new();
  let mut entry = fixture.reader(false).snapshot.entry;
  entry.provider = Provider::ZCode;
  let mut reader = SessionReader::new(entry, true, fixture.path.clone()).unwrap();
  assert!(
    reader
      .snapshot
      .records
      .iter()
      .all(|record| record.topic == "zcode.one" && record.session.provider == Provider::ZCode)
  );
  assert!(
    reader
      .snapshot
      .records
      .iter()
      .flat_map(|record| record.record.events)
      .all(|event| serde_json::to_value(event).unwrap()["provider"] == "zcode")
  );
  let initial = reader.snapshot.generation.clone();
  fixture
    .database
    .execute_batch(r#"update part set data = '{"type":"text","text":"changed"}' where id = 'p1'"#)
    .unwrap();
  assert!(fixture.poll(&mut reader).unwrap());
  assert_ne!(reader.snapshot.generation, initial);
  assert!(!fixture.poll(&mut reader).unwrap());
}

#[test]
fn grouped_files_buffer_partial_rows_reset_edits_and_preserve_native() {
  use std::io::Write;
  use tokn_session_client::AgentClient;
  for provider in [Provider::WorkBuddy, Provider::Dsh] {
    let root = TempDir::new().unwrap();
    let (relative, contents, append) = match provider {
      Provider::WorkBuddy => (
        "projects/work/session.jsonl",
        "{\"type\":\"message\",\"id\":\"one\",\"sessionId\":\"session\",\"role\":\"user\",\"content\":\"hello\",\"timestamp\":1}\n",
        "{\"type\":\"future\",\"id\":\"one\",\"value\":42}",
      ),
      _ => (
        "session/session.jsonl",
        "{\"type\":\"session\",\"version\":0,\"id\":\"session\",\"createdAt\":1,\"delegationDepth\":0}\n{\"type\":\"future\",\"seq\":0,\"value\":\"hello\"}\n",
        "{\"type\":\"future\",\"seq\":1,\"value\":42}",
      ),
    };
    let path = root.path().join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, contents).unwrap();
    let header = AgentClient::list_session_headers(
      tokn_session_relay::providers::source(provider),
      Some(root.path().into()),
    )
    .unwrap()
    .remove(0);
    let entry = CatalogEntry {
      key: "session".into(),
      provider,
      header,
    };
    let mut reader = SessionReader::new(entry, true, root.path().into()).unwrap();
    let initial = reader.snapshot.clone();
    assert!(initial.records.iter().any(|r| r.record.native.is_some()));
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(append.as_bytes()).unwrap();
    assert!(
      !reader.poll_grouped_file(versions(&path, false)).unwrap(),
      "no complete new row"
    );
    file.write_all(b"\n").unwrap();
    assert!(reader.poll_grouped_file(versions(&path, false)).unwrap());
    assert_eq!(initial.generation, reader.snapshot.generation);
    assert_eq!(initial.records.len() + 1, reader.snapshot.records.len());
    let ids: std::collections::HashSet<_> = reader.snapshot.records.iter().map(|r| r.record.record_id).collect();
    assert_eq!(
      ids.len(),
      reader.snapshot.records.len(),
      "duplicate native IDs must not collapse"
    );
    std::fs::write(&path, format!("{}{append}\n", contents.replace("hello", "edits"))).unwrap();
    assert!(reader.poll_grouped_file(versions(&path, false)).unwrap());
    assert_ne!(initial.generation, reader.snapshot.generation);
    let last_good = reader.snapshot.revision;
    std::fs::write(&path, "invalid\n").unwrap();
    assert!(reader.poll_grouped_file(versions(&path, false)).is_err());
    assert_eq!(reader.snapshot.revision, last_good);
  }
}

#[test]
fn assembled_dsh_output_resets_prior_stream_batches() {
  use tokn_session_client::{AgentClient, Source};
  let root = TempDir::new().unwrap();
  let path = root.path().join("session.jsonl");
  let fixture = include_str!("../../../dsh/fixtures/basic/session.jsonl");
  let split = fixture.find("{\"type\":\"assistant/message\"").unwrap();
  std::fs::write(&path, &fixture[..split]).unwrap();
  let header = AgentClient::list_session_headers(Source::Dsh, Some(root.path().into()))
    .unwrap()
    .remove(0);
  let entry = CatalogEntry {
    key: "dsh".into(),
    provider: Provider::Dsh,
    header,
  };
  let mut reader = SessionReader::new(entry, false, root.path().into()).unwrap();
  let initial = reader.snapshot.generation.clone();
  std::fs::write(&path, fixture).unwrap();
  assert!(reader.poll_grouped_file(versions(&path, false)).unwrap());
  assert_ne!(reader.snapshot.generation, initial);
  let history = AgentClient::load_session(Source::Dsh, Some(root.path().into()), "dsh-fixture").unwrap();
  let events: Vec<_> = reader
    .snapshot
    .records
    .iter()
    .flat_map(|record| record.record.events)
    .collect();
  assert_eq!(
    serde_json::to_value(events).unwrap(),
    serde_json::to_value(history.events).unwrap()
  );
}

#[test]
fn workbuddy_catalog_wal_updates_followed_presentation() {
  use tokn_session_client::{AgentClient, Source};
  let root = TempDir::new().unwrap();
  let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../workbuddy/fixtures");
  let path = root.path().join("projects/fixture-workspace/wb-shell-command.jsonl");
  std::fs::create_dir_all(path.parent().unwrap()).unwrap();
  std::fs::copy(
    fixtures.join("projects/fixture-workspace/wb-shell-command.jsonl"),
    &path,
  )
  .unwrap();
  std::fs::copy(fixtures.join("workbuddy.db"), root.path().join("workbuddy.db")).unwrap();
  let header = AgentClient::list_session_headers(Source::WorkBuddy, Some(root.path().into()))
    .unwrap()
    .into_iter()
    .find(|h| h.id == "wb-shell-command")
    .unwrap();
  let entry = CatalogEntry {
    key: "wb".into(),
    provider: Provider::WorkBuddy,
    header,
  };
  let mut reader = SessionReader::new(entry, false, root.path().into()).unwrap();
  let initial = reader.snapshot.generation.clone();
  let database = rusqlite::Connection::open(root.path().join("workbuddy.db")).unwrap();
  database.execute_batch("pragma journal_mode=wal; update sessions set title = 'Changed catalog title', custom_title = 'Changed catalog title' where id = 'wb-shell-command';").unwrap();
  assert!(reader.poll().unwrap());
  assert_eq!(
    reader.snapshot.entry.header.title.as_deref(),
    Some("Changed catalog title")
  );
  assert_eq!(reader.snapshot.generation, initial);
}

#[test]
fn lazy_codex_prepend_preserves_existing_event_positions_across_pages() {
  use std::collections::BTreeMap;
  use std::io::Write;
  let directory = TempDir::new().unwrap();
  let path = directory.path().join("rollout-lazy.jsonl");
  let turn = |index| {
    format!(
      "{}\n{}\n",
      serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":format!("turn-{index}")}}),
      serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":format!("prompt-{index}")}})
    )
  };
  let mut body = format!(
    "{}\n",
    serde_json::json!({"type":"session_meta","payload":{"id":"lazy","cwd":"/tmp"}})
  );
  for index in 0..8 {
    body.push_str(&turn(index));
  }
  std::fs::write(&path, body).unwrap();
  let mut reader = SessionReader::new_with_mode(
    CatalogEntry {
      key: "lazy".into(),
      provider: Provider::Codex,
      header: serde_json::from_value(serde_json::json!({"id":"lazy","path":path})).unwrap(),
    },
    false,
    directory.path().into(),
    None,
    true,
  )
  .unwrap();
  let positions = |reader: &SessionReader| {
    let mut result = BTreeMap::new();
    for index in 0..reader.snapshot.records.len() {
      let (record, start) = reader.snapshot.records.read(index).unwrap();
      for (offset, event) in record.record.events.into_iter().enumerate() {
        if let tokn_session_core::AgentEvent::Message(message) = event {
          result.insert(message.text, reader.snapshot.event_base + start + offset);
        }
      }
    }
    result
  };
  let generation = reader.snapshot.generation.clone();
  assert_eq!(positions(&reader).len(), 1);
  assert!(!reader.ensure_history(Some((None, None))).unwrap());
  for expected in [4, 7, 8] {
    let before = positions(&reader);
    assert!(
      reader
        .ensure_history(Some((None, Some(reader.snapshot.event_base))))
        .unwrap()
    );
    let after = positions(&reader);
    assert_eq!(after.len(), expected);
    assert_eq!(reader.snapshot.generation, generation);
    for (message, position) in before {
      assert_eq!(after[&message], position);
    }
  }
  assert!(!reader.snapshot.has_earlier);
  assert!(reader.snapshot.event_base > 0);
  std::fs::OpenOptions::new()
    .append(true)
    .open(path)
    .unwrap()
    .write_all(turn(8).as_bytes())
    .unwrap();
  let before = positions(&reader);
  assert!(reader.poll().unwrap());
  let after = positions(&reader);
  assert_eq!(after.len(), 9);
  for (message, position) in before {
    assert_eq!(after[&message], position);
  }
}
