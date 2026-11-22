//! Synthetic storage using the upstream application/schema IDs and frame format.
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokn_session_core::{AgentEvent, SessionHistoryStatus};
use tokn_session_dsh::{DshSessionSource, read_session_header_exact, storage_dependency_paths};

struct Fixture {
  root: tempfile::TempDir,
  path: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".segments")).unwrap();
    let path = root.path().join(".segments/sessions.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA application_id=1146310723; PRAGMA user_version=1;
      CREATE TABLE segments(segment_uuid TEXT PRIMARY KEY,parent_segment_uuid TEXT,parent_end_seq INTEGER,base_seq INTEGER NOT NULL);
      CREATE TABLE traces(trace_uuid TEXT PRIMARY KEY,header TEXT NOT NULL,tail_segment_uuid TEXT NOT NULL,inherited_count INTEGER NOT NULL);
      CREATE TABLE frames(segment_uuid TEXT NOT NULL,start_seq INTEGER NOT NULL,end_seq INTEGER NOT NULL,codec INTEGER NOT NULL,payload BLOB NOT NULL,PRIMARY KEY(segment_uuid,start_seq));").unwrap();
    db.execute("INSERT INTO segments VALUES('parent-segment',NULL,NULL,0)", [])
      .unwrap();
    db.execute("INSERT INTO segments VALUES('child-segment','parent-segment',2,2)", [])
      .unwrap();
    for (id, segment, cut, seeded) in [
      ("parent", "parent-segment", 0, false),
      ("child", "child-segment", 2, true),
    ] {
      let mut header = json!({"version":4,"id":id,"createdAt":if seeded {2} else {1},"cwd":"/project/fixture","isSeeded":seeded,"delegationDepth":if seeded {1} else {0}});
      if seeded {
        header["origin"] = json!("subagent");
        header["parentSession"] = json!("parent");
      }
      db.execute(
        "INSERT INTO traces VALUES(?1,?2,?3,?4)",
        params![id, header.to_string(), segment, cut],
      )
      .unwrap();
    }
    frame(
      &db,
      "parent-segment",
      0,
      &[user(0, "inherited parent prompt"), assistant(1, "inherited answer")],
      true,
    );
    frame(
      &db,
      "parent-segment",
      2,
      &[user(2, "parent later prompt"), assistant(3, "parent later answer")],
      false,
    );
    frame(
      &db,
      "child-segment",
      0,
      &[
        json!({"type":"session/end-seed","seq":2,"time":2,"data":{"inherited":true}}),
        user(3, "child task"),
      ],
      false,
    );
    frame(&db, "child-segment", 2, &[assistant(4, "child answer")], true);
    drop(db);
    Self { root, path }
  }

  fn db(&self) -> Connection {
    Connection::open(&self.path).unwrap()
  }
  fn source(&self) -> DshSessionSource {
    DshSessionSource::new(Some(self.root.path().to_owned()))
  }
  fn marker(&self, id: &str, database: &Path) -> PathBuf {
    let path = self
      .path
      .parent()
      .unwrap()
      .join(format!("{:x}.json", Sha256::digest(id.as_bytes())));
    std::fs::write(&path, json!({"id":id,"database":database}).to_string()).unwrap();
    path
  }
}

fn user(seq: u64, text: &str) -> Value {
  json!({"type":"user/message","seq":seq,"time":seq,"surfaceOp":"append","data":{"id":format!("user-{seq}"),"role":"user","source":{"kind":"user"},"content":[{"type":"text","text":text}]}})
}
fn assistant(seq: u64, text: &str) -> Value {
  json!({"type":"assistant/message","seq":seq,"time":seq,"surfaceOp":"append","data":{"turn":seq+1,"step":1,"message":{"id":format!("assistant-{seq}"),"role":"assistant","source":{"kind":"model"},"content":[{"type":"text","text":text}]},"stream":[]}})
}
fn frame(db: &Connection, segment: &str, start: i64, rows: &[Value], compressed: bool) {
  let bytes = rows
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n")
    .into_bytes();
  let payload = if compressed {
    zstd::stream::encode_all(bytes.as_slice(), 3).unwrap()
  } else {
    bytes
  };
  db.execute(
    "INSERT INTO frames VALUES(?1,?2,?3,?4,?5)",
    params![
      segment,
      start,
      start + rows.len() as i64,
      if compressed { 1 } else { 0 },
      payload
    ],
  )
  .unwrap();
}
fn messages(events: &[AgentEvent]) -> Vec<&str> {
  events
    .iter()
    .filter_map(|event| {
      if let AgentEvent::Message(event) = event {
        Some(event.text.as_str())
      } else {
        None
      }
    })
    .collect()
}

#[test]
fn reads_native_predecessor_segments_with_exact_fork_cut_and_no_parent_late_turns() {
  let fixture = Fixture::new();
  let before = std::fs::read(&fixture.path).unwrap();
  let references = fixture.source().list_session_relations().unwrap();
  assert_eq!(references.len(), 2);
  assert!(references.iter().all(|reference| reference.path == fixture.path));
  let parent = fixture.source().load_session("parent").unwrap();
  assert_eq!(
    messages(&parent.events),
    [
      "inherited parent prompt",
      "inherited answer",
      "parent later prompt",
      "parent later answer"
    ]
  );
  let child = fixture
    .source()
    .load_session_path_with_id(&fixture.path, "child")
    .unwrap();
  assert_eq!(messages(&child.events), ["child task", "child answer"]);
  assert_eq!(child.history_status, SessionHistoryStatus::FilteredSubagent);
  assert_eq!(child.reference.parent_session_id.as_deref(), Some("parent"));
  assert_eq!(child.reference.preview.as_deref(), Some("child task"));
  assert_eq!(child.reference.message_count, 2);
  let records = fixture
    .source()
    .load_session_records_path_with_id(&fixture.path, "child", true, 1024 * 1024)
    .unwrap();
  assert_eq!(records.records[0].native.as_ref().unwrap()["isSeeded"], true);
  assert_eq!(records.records.last().unwrap().native.as_ref().unwrap()["seq"], 4);
  assert_eq!(
    messages(
      &records
        .records
        .into_iter()
        .flat_map(|record| record.events)
        .collect::<Vec<_>>()
    ),
    ["child task", "child answer"]
  );
  assert!(
    fixture
      .source()
      .load_session_path(&fixture.path)
      .unwrap_err()
      .contains("multiple sessions")
  );
  assert!(read_session_header_exact(&fixture.path, "other").is_err());
  let header = read_session_header_exact(&fixture.path, "child").unwrap();
  assert_eq!(header.id, "child");
  assert!(header.updated_at_ms.is_some());
  assert_eq!(
    storage_dependency_paths(&fixture.path).unwrap(),
    [
      fixture.path.clone(),
      PathBuf::from(format!("{}-wal", fixture.path.display()))
    ]
  );
  assert_eq!(std::fs::read(&fixture.path).unwrap(), before);
}

#[test]
fn sqlite_claims_legacy_identity_without_importing_or_reading_old_bodies() {
  let fixture = Fixture::new();
  let legacy = fixture.root.path().join("legacy");
  std::fs::create_dir(&legacy).unwrap();
  std::fs::write(
    legacy.join("session.jsonl"),
    "{\"type\":\"session\",\"version\":0,\"id\":\"parent\",\"createdAt\":0,\"delegationDepth\":0}\nnot-json\n",
  )
  .unwrap();
  let references = fixture.source().list_sessions().unwrap();
  assert_eq!(references.len(), 2);
  assert!(references.iter().all(|reference| reference.path == fixture.path));
  assert_eq!(
    fixture
      .source()
      .list_session_relation_catalog()
      .unwrap()
      .references
      .len(),
    2
  );
}

#[test]
fn exact_headers_do_not_decode_frames_but_corrupt_body_reads_fail_closed() {
  for mutation in [
    "UPDATE frames SET codec=7 WHERE segment_uuid='child-segment' AND start_seq=0",
    "UPDATE frames SET payload=x'00' WHERE segment_uuid='child-segment' AND start_seq=0",
    "UPDATE frames SET end_seq=99 WHERE segment_uuid='child-segment' AND start_seq=0",
    "DELETE FROM frames WHERE segment_uuid='child-segment' AND start_seq=0",
    "UPDATE segments SET parent_segment_uuid='child-segment',parent_end_seq=0 WHERE segment_uuid='child-segment'",
    "UPDATE segments SET parent_end_seq=99 WHERE segment_uuid='child-segment'",
    "UPDATE traces SET inherited_count=1 WHERE trace_uuid='child'",
  ] {
    let fixture = Fixture::new();
    fixture.db().execute_batch(mutation).unwrap();
    assert_eq!(read_session_header_exact(&fixture.path, "child").unwrap().id, "child");
    let before = std::fs::read(&fixture.path).unwrap();
    assert!(fixture.source().load_session("child").is_err(), "{mutation}");
    assert_eq!(std::fs::read(&fixture.path).unwrap(), before);
  }
}

#[test]
fn rejects_wrong_schema_identity_decompression_limits_and_does_not_repair() {
  for mutation in [
    "PRAGMA application_id=0",
    "PRAGMA user_version=2",
    "UPDATE traces SET header='{}' WHERE trace_uuid='child'",
  ] {
    let fixture = Fixture::new();
    fixture.db().execute_batch(mutation).unwrap();
    let before = std::fs::read(&fixture.path).unwrap();
    assert!(read_session_header_exact(&fixture.path, "child").is_err());
    assert_eq!(std::fs::read(&fixture.path).unwrap(), before);
  }
  let fixture = Fixture::new();
  let db = fixture.db();
  db.execute(
    "DELETE FROM frames WHERE segment_uuid='child-segment' AND start_seq=2",
    [],
  )
  .unwrap();
  frame(&db, "child-segment", 2, &[assistant(4, &"large".repeat(100_000))], true);
  drop(db);
  assert!(
    fixture
      .source()
      .load_session_records_path_with_id(&fixture.path, "child", false, 4096)
      .unwrap_err()
      .contains("size limit")
  );
}

#[test]
fn native_routing_markers_are_bounded_identity_checked_and_cannot_redirect_database_reads() {
  let fixture = Fixture::new();
  let marker = fixture.marker("child", &fixture.path);
  assert_eq!(
    fixture.source().load_session_path(&marker).unwrap().reference.id,
    "child"
  );
  assert!(read_session_header_exact(&marker, "parent").is_err());
  let other = Fixture::new();
  fixture.marker("child", &other.path);
  assert!(
    fixture
      .source()
      .load_session_path(&marker)
      .unwrap_err()
      .contains("unexpected database")
  );
  std::fs::write(&marker, "x".repeat(64 * 1024 + 1)).unwrap();
  assert!(
    read_session_header_exact(&marker, "child")
      .unwrap_err()
      .contains("size")
  );
}

#[test]
fn shared_database_writes_are_not_activity_timestamps_of_every_trace() {
  let fixture = Fixture::new();
  for (id, created) in [("parent", 1), ("child", 2)] {
    assert_eq!(
      read_session_header_exact(&fixture.path, id).unwrap().updated_at_ms,
      Some(created)
    );
  }
  let database = fixture.db();
  database
    .execute_batch("PRAGMA journal_mode=wal; PRAGMA wal_autocheckpoint=0;")
    .unwrap();
  frame(&database, "parent-segment", 4, &[user(4, "new parent activity")], false);
  for (id, created) in [("parent", 1), ("child", 2)] {
    let header = read_session_header_exact(&fixture.path, id).unwrap();
    assert_eq!(header.updated_at_ms, Some(created));
    assert_eq!(header.timestamp.as_deref(), Some(created.to_string().as_str()));
  }
  let marker = fixture.marker("child", &fixture.path);
  assert_eq!(
    read_session_header_exact(&marker, "child").unwrap().updated_at_ms,
    Some(2)
  );
}
