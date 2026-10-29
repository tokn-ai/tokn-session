use std::{path::Path, sync::Arc};

use rusqlite::types::Value;

use super::*;
use crate::{APPLICATION_ID, SessionIndex};

const OLD_TABLES: &[&str] = &[
  "sources",
  "sessions",
  "projects",
  "project_aliases",
  "registered_projects",
  "managed_session_admissions",
  "project_folder_origins",
  "project_folder_providers",
  "project_folder_cwds",
  "project_discovery_revisions",
];
type TableRows = Vec<(String, Vec<Vec<Value>>)>;

fn fixture(path: &Path) -> Connection {
  let connection = Connection::open(path).unwrap();
  connection
    .pragma_update(None, "application_id", APPLICATION_ID)
    .unwrap();
  connection.execute_batch(MIGRATION_LEDGER_SQL).unwrap();
  for migration in MIGRATIONS.iter().take(12) {
    connection.execute_batch(migration.sql).unwrap();
    connection
      .execute(
        "INSERT INTO schema_migrations VALUES(?1, ?2, ?3)",
        params![
          migration.version,
          migration_checksum(migration.sql),
          1000 + migration.version,
        ],
      )
      .unwrap();
  }
  connection.execute_batch(EARLY_SQL).unwrap();
  connection
    .execute("INSERT INTO schema_migrations VALUES(13, ?1, 1013)", [EARLY_CHECKSUM])
    .unwrap();
  connection.execute_batch(
    "INSERT INTO projects VALUES('/repository', 10, 20);
     INSERT INTO project_aliases(cwd, project_key, provenance) VALUES('/repository/manual', '/repository', 'registered_folder');
     INSERT INTO project_aliases(cwd, project_key, provenance) VALUES('/repository/automatic', '/repository', 'registered_folder');
     INSERT INTO registered_projects VALUES('manual', '/repository', '/repository/manual', 'Saved manual name', 30);
     INSERT INTO registered_projects VALUES('automatic', '/repository', '/repository/automatic', 'Saved discovered name', 40);
     INSERT INTO project_folder_origins VALUES('manual', 1), ('automatic', 0);
     INSERT INTO project_folder_providers VALUES('manual', 'pi', 'present', 1),
       ('automatic', 'codex', 'unknown', 1), ('automatic', 'pi', 'absent', 0);
     INSERT INTO project_folder_cwds VALUES('pi', '/manual-alias', 'manual'), ('codex', '/automatic-alias', 'automatic');
     INSERT INTO project_discovery_revisions VALUES('codex', 17), ('pi', 23);
     INSERT INTO sources(id, provider, source_key, cursor, scanned_at_ms, created_at_ms, updated_at_ms)
       VALUES(1, 'pi', 'fixture', 'body.v3.saved', 40, 40, 50);
     INSERT INTO sessions(source_id, session_id, source_path, title, cwd,
       attention_marker, attention_revision, seen_attention_revision, seen_at_ms,
       first_indexed_at_ms, indexed_at_ms)
       VALUES(1, 'owned', '/history/owned.jsonl', 'Saved session title', '/repository/manual',
         'saved-marker', 7, 5, 60, 40, 50);
     INSERT INTO managed_session_admissions VALUES('pi', 'fixture', 'owned', 'manual');",
  ).unwrap();
  connection
}

fn ledger(connection: &Connection) -> Vec<(i64, String, i64)> {
  connection
    .prepare("SELECT version, checksum, applied_at_ms FROM schema_migrations ORDER BY version")
    .unwrap()
    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn data(connection: &Connection) -> TableRows {
  OLD_TABLES
    .iter()
    .map(|table| {
      let mut statement = connection
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
      let columns = statement.column_count();
      let rows = statement
        .query_map([], |row| (0..columns).map(|column| row.get(column)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
      (table.to_string(), rows)
    })
    .collect()
}

#[test]
fn immutable_sql_identifies_only_the_observed_early_migration() {
  assert_eq!(EARLY_SQL.len(), 1066);
  assert_eq!(migration_checksum(EARLY_SQL), EARLY_CHECKSUM);
  assert_eq!(migration_checksum(MIGRATION_013), "ea992d0d2c6bcf53");
  let suffix = MIGRATION_013.strip_prefix(EARLY_SQL).unwrap();
  assert!(suffix.starts_with("\nCREATE TABLE project_folder_revision ("));
  assert_eq!(format!("{EARLY_SQL}{suffix}"), MIGRATION_013);
  assert_eq!(suffix.matches("CREATE TRIGGER ").count(), 9);
  assert!(!is_early_project_discovery(12, EARLY_CHECKSUM));
  assert!(!is_early_project_discovery(13, "unknown"));
}

#[test]
fn early_discovery_upgrade_preserves_every_saved_row_and_original_application_time() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("early.sqlite");
  let connection = fixture(&path);
  let before = data(&connection);
  let before_ledger = ledger(&connection);
  drop(connection);

  let index = SessionIndex::open(&path).unwrap();
  assert_eq!(index.project_folder_revision().unwrap(), 0);
  let connection = index.connection().unwrap();
  assert_eq!(data(&connection), before);
  let after = ledger(&connection);
  assert_eq!(after[..12], before_ledger[..12]);
  assert_eq!(after[12], (13, migration_checksum(MIGRATION_013), 1013));
  let canonical = SessionIndex::open_in_memory().unwrap();
  assert_eq!(
    owned_schema(&connection, i64::MAX, i64::MAX).unwrap(),
    owned_schema(&canonical.connection().unwrap(), i64::MAX, i64::MAX).unwrap()
  );
  drop(connection);
  drop(index);
  let reopened = SessionIndex::open(&path).unwrap();
  assert_eq!(data(&reopened.connection().unwrap()), before);
  assert_eq!(ledger(&reopened.connection().unwrap()), after);
  let connection = reopened.connection().unwrap();
  connection
    .execute("UPDATE sources SET cursor = 'body.v3.new' WHERE id = 1", [])
    .unwrap();
  assert_eq!(
    connection
      .query_row("SELECT revision FROM project_folder_revision", [], |row| row
        .get::<_, i64>(0))
      .unwrap(),
    0
  );
  connection
    .execute(
      "UPDATE registered_projects SET name = 'Renamed' WHERE project_id = 'manual'",
      [],
    )
    .unwrap();
  connection
    .execute(
      "UPDATE project_folder_origins SET manual = 1 WHERE project_id = 'automatic'",
      [],
    )
    .unwrap();
  connection
    .execute(
      "UPDATE project_folder_providers SET state = 'present' WHERE project_id = 'automatic' AND provider = 'codex'",
      [],
    )
    .unwrap();
  assert_eq!(
    connection
      .query_row("SELECT revision FROM project_folder_revision", [], |row| row
        .get::<_, i64>(0))
      .unwrap(),
    3
  );
}

#[test]
fn concurrent_opens_add_missing_revision_tracking_once() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("concurrent.sqlite");
  let connection = fixture(&path);
  let before = data(&connection);
  drop(connection);
  let barrier = Arc::new(std::sync::Barrier::new(2));
  let open = |barrier: Arc<std::sync::Barrier>, path: std::path::PathBuf| {
    std::thread::spawn(move || {
      barrier.wait();
      SessionIndex::open(path).map(|_| ()).map_err(|error| error.to_string())
    })
  };
  let first = open(barrier.clone(), path.clone());
  let second = open(barrier, path.clone());
  first.join().unwrap().unwrap();
  second.join().unwrap().unwrap();
  let index = SessionIndex::open(path).unwrap();
  assert_eq!(index.project_folder_revision().unwrap(), 0);
  let connection = index.connection().unwrap();
  assert_eq!(data(&connection), before);
  assert_eq!(ledger(&connection)[12], (13, migration_checksum(MIGRATION_013), 1013));
}

#[test]
fn unknown_hash_or_unrelated_ledger_drift_rejects_before_adoption() {
  for version in [1, 13] {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unknown.sqlite");
    let connection = fixture(&path);
    connection
      .execute(
        "UPDATE schema_migrations SET checksum = 'unknown' WHERE version = ?1",
        [version],
      )
      .unwrap();
    let before = data(&connection);
    let before_ledger = ledger(&connection);
    let before_schema = owned_schema(&connection, i64::MAX, i64::MAX).unwrap();
    drop(connection);
    assert!(matches!(SessionIndex::open(&path),
      Err(SessionIndexError::MigrationChecksumMismatch { version: rejected, .. }) if rejected == version));
    let connection = Connection::open(&path).unwrap();
    assert_eq!(data(&connection), before);
    assert_eq!(ledger(&connection), before_ledger);
    assert_eq!(owned_schema(&connection, i64::MAX, i64::MAX).unwrap(), before_schema);
  }
}

#[test]
fn malformed_or_partially_completed_schema_rejects_without_changes() {
  for mutation in [
    "DROP INDEX project_folder_providers_by_provider;",
    "DROP INDEX project_folder_providers_by_provider;
     CREATE INDEX project_folder_providers_by_provider ON project_folder_providers(state);",
    "ALTER TABLE project_folder_origins ADD COLUMN unexpected TEXT;",
    "CREATE INDEX custom_index ON sources(cursor);",
    "CREATE TRIGGER unexpected_ledger_trigger AFTER UPDATE ON schema_migrations
     BEGIN DELETE FROM project_folder_cwds; END;",
    "CREATE TABLE project_folder_revision(singleton INTEGER PRIMARY KEY, revision INTEGER NOT NULL);",
    "CREATE TRIGGER revise_registered_projects_insert AFTER INSERT ON registered_projects BEGIN SELECT 1; END;",
    "CREATE VIEW unexpected_view AS SELECT * FROM project_folder_providers;",
  ] {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("malformed.sqlite");
    let connection = fixture(&path);
    connection.execute_batch(mutation).unwrap();
    let before = data(&connection);
    let before_ledger = ledger(&connection);
    let before_schema = owned_schema(&connection, i64::MAX, i64::MAX).unwrap();
    drop(connection);
    assert!(
      matches!(
        SessionIndex::open(&path),
        Err(SessionIndexError::MigrationHistoryInvalid(_))
      ),
      "{mutation}"
    );
    let connection = Connection::open(&path).unwrap();
    assert_eq!(data(&connection), before, "{mutation}");
    assert_eq!(ledger(&connection), before_ledger, "{mutation}");
    assert_eq!(
      owned_schema(&connection, i64::MAX, i64::MAX).unwrap(),
      before_schema,
      "{mutation}"
    );
  }
}

#[test]
fn early_discovery_must_be_the_final_recorded_migration() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("impossible.sqlite");
  let connection = fixture(&path);
  connection
    .execute("INSERT INTO schema_migrations VALUES(14, 'unknown', 1014)", [])
    .unwrap();
  let before = data(&connection);
  let before_ledger = ledger(&connection);
  let before_schema = owned_schema(&connection, i64::MAX, i64::MAX).unwrap();
  drop(connection);
  assert!(matches!(
    SessionIndex::open(&path),
    Err(SessionIndexError::MigrationHistoryInvalid(_))
  ));
  let connection = Connection::open(&path).unwrap();
  assert_eq!(data(&connection), before);
  assert_eq!(ledger(&connection), before_ledger);
  assert_eq!(owned_schema(&connection, i64::MAX, i64::MAX).unwrap(), before_schema);
}
