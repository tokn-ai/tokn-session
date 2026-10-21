//! Narrow compatibility for the verified, briefly deployed combined migration.
use rusqlite::{Connection, Transaction, params};

use super::{
  MIGRATION_010, MIGRATION_011, MIGRATION_LEDGER_SQL, MIGRATIONS, Result, SessionIndexError, migration_checksum,
};

const COMBINED_PROJECT_CHECKSUM: &str = "746531f0e095371f";

pub(super) fn is_combined_project_migration(version: i64, checksum: &str) -> bool {
  version == 10 && checksum == COMBINED_PROJECT_CHECKSUM
}

/// The caller must first validate every ledger entry, including entries after
/// migration 10. This transaction also owns schema validation and normalization.
pub(super) fn adopt_combined_project_migration(transaction: &Transaction<'_>) -> Result<()> {
  let reference = Connection::open_in_memory()?;
  reference.execute_batch(MIGRATION_LEDGER_SQL)?;
  // Freeze the reference at the compatibility boundary. Future migrations
  // must run after adoption rather than changing this schema's definition.
  for migration in MIGRATIONS.iter().take(11) {
    reference.execute_batch(migration.sql)?;
  }
  let expected = owned_schema(&reference, i64::MAX, i64::MAX)?;
  let sql_bytes = expected
    .iter()
    .filter_map(|object| object.3.as_ref())
    .map(String::len)
    .max()
    .unwrap_or(0) as i64;
  // Include one extra row/byte so an unexpected object or a longer definition
  // cannot compare equal while bounding materialized data from the old schema.
  let actual = owned_schema(transaction, expected.len() as i64 + 1, sql_bytes + 1)?;
  if actual != expected {
    return Err(SessionIndexError::MigrationHistoryInvalid(
      "combined migration 10 does not match the verified project/admission schema".into(),
    ));
  }

  transaction.execute(
    "UPDATE schema_migrations SET checksum = ?1 WHERE version = 10",
    [migration_checksum(MIGRATION_010)],
  )?;
  // Both schemas were committed by the old combined migration, so retain
  // that original timestamp rather than inventing a second application time.
  transaction.execute(
    "INSERT INTO schema_migrations(version, checksum, applied_at_ms)
     SELECT 11, ?1, applied_at_ms FROM schema_migrations WHERE version = 10",
    [migration_checksum(MIGRATION_011)],
  )?;
  Ok(())
}

type SchemaObject = (String, String, String, Option<String>);

fn owned_schema(connection: &Connection, row_limit: i64, sql_limit: i64) -> Result<Vec<SchemaObject>> {
  let mut statement = connection.prepare(
    "SELECT substr(type, 1, ?1), substr(name, 1, ?1), substr(tbl_name, 1, ?1), substr(sql, 1, ?1)
     FROM main.sqlite_schema
     WHERE tbl_name COLLATE NOCASE IN ('registered_projects', 'managed_session_admissions', 'sessions', 'schema_migrations')
       OR name COLLATE NOCASE IN ('registered_projects_by_group', 'revoke_deleted_managed_admission')
     ORDER BY type, name, tbl_name
     LIMIT ?2",
  )?;
  let rows = statement.query_map(params![sql_limit, row_limit], |row| {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
  })?;
  rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

#[cfg(test)]
mod tests {
  use std::{path::Path, sync::Arc};

  use rusqlite::types::Value;

  use super::*;
  use crate::{APPLICATION_ID, SessionIndex, SessionKey};

  fn fixture(path: &Path, combined: bool) -> Connection {
    let connection = Connection::open(path).unwrap();
    connection
      .pragma_update(None, "application_id", APPLICATION_ID)
      .unwrap();
    connection.execute_batch(MIGRATION_LEDGER_SQL).unwrap();
    for migration in MIGRATIONS.iter().take(10) {
      connection.execute_batch(migration.sql).unwrap();
      connection
        .execute(
          "INSERT INTO schema_migrations VALUES(?1, ?2, ?3)",
          params![
            migration.version,
            migration_checksum(migration.sql),
            1000 + migration.version
          ],
        )
        .unwrap();
    }
    connection
      .execute_batch(
        "INSERT INTO projects VALUES('/repository', 10, 20);
       INSERT INTO project_aliases VALUES('/repository/worktree', '/repository');
       INSERT INTO registered_projects VALUES('project', '/repository', '/repository/worktree', 'Saved project', 30);
       INSERT INTO sources(id, provider, source_key, cursor, scanned_at_ms, created_at_ms, updated_at_ms)
         VALUES(1, 'pi', 'fixture', 'body.v3.existing', 40, 40, 50);
       INSERT INTO sessions(source_id, session_id, source_path, title, cwd,
         attention_marker, attention_revision, seen_attention_revision, seen_at_ms,
         first_indexed_at_ms, indexed_at_ms)
         VALUES(1, 'owned', '/history/owned.jsonl', 'Saved title', '/repository/worktree',
           'saved-marker', 7, 5, 60, 40, 50);",
      )
      .unwrap();
    if combined {
      connection.execute_batch(MIGRATION_011).unwrap();
      connection
        .execute(
          "UPDATE schema_migrations SET checksum = ?1 WHERE version = 10",
          [COMBINED_PROJECT_CHECKSUM],
        )
        .unwrap();
      connection
        .execute(
          "INSERT INTO managed_session_admissions VALUES('pi', 'fixture', 'owned', 'project')",
          [],
        )
        .unwrap();
    }
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

  type TableRows = Vec<(String, Vec<Vec<Value>>)>;

  fn data(connection: &Connection, tables: &[&str]) -> TableRows {
    tables
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

  const EXISTING_DATA: &[&str] = &[
    "projects",
    "project_aliases",
    "registered_projects",
    "sources",
    "sessions",
  ];

  fn assert_folder_upgrade(connection: &Connection) {
    let project: (String, String, String, String, i64) = connection
      .query_row(
        "SELECT project_id, group_key, folder_path, name, created_at_ms FROM registered_projects",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
      )
      .unwrap();
    assert_eq!(
      project,
      (
        "project".into(),
        "/repository/worktree".into(),
        "/repository/worktree".into(),
        "Saved project".into(),
        30
      )
    );
    let alias: (String, String) = connection
      .query_row(
        "SELECT project_key, provenance FROM project_aliases WHERE cwd = '/repository/worktree'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .unwrap();
    assert_eq!(alias, ("/repository/worktree".into(), "registered_folder".into()));
  }

  #[test]
  fn immutable_project_migration_checksums_identify_only_the_two_observed_schemas() {
    assert_eq!(migration_checksum(MIGRATION_010), "40d63f219f2bcf7a");
    assert_eq!(
      migration_checksum(&format!("{MIGRATION_010}{MIGRATION_011}")),
      COMBINED_PROJECT_CHECKSUM
    );
    assert!(!is_combined_project_migration(10, "7d54311201dd7665"));
    assert!(!is_combined_project_migration(11, COMBINED_PROJECT_CHECKSUM));
  }

  #[test]
  fn original_project_migration_upgrades_without_replacing_saved_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("original.sqlite");
    let connection = fixture(&path, false);
    let before = data(&connection, &["sources", "sessions"]);
    let before_ledger = ledger(&connection);
    drop(connection);

    let index = SessionIndex::open(&path).unwrap();
    assert_eq!(index.list_registered_projects().unwrap()[0].name, "Saved project");
    assert_eq!(index.project_order(&[], 999).unwrap()["/repository"], 20);
    let key = SessionKey::new("pi", "fixture", "owned");
    let row = index.session(&key).unwrap().unwrap();
    assert_eq!(row.seen_attention_revision, 5);
    assert_eq!(row.attention_revision, 7);
    assert_eq!(row.seen_at_ms, Some(60));
    assert_eq!(row.attention_marker.as_deref(), Some("saved-marker"));
    assert!(index.managed_session_admissions().unwrap().is_empty());
    let connection = index.connection().unwrap();
    assert_eq!(data(&connection, &["sources", "sessions"]), before);
    assert_folder_upgrade(&connection);
    assert_eq!(ledger(&connection)[..10], before_ledger);
    drop(connection);
    index.admit_managed_session(&key, "project").unwrap();
    drop(index);
    let reopened = SessionIndex::open(&path).unwrap();
    assert!(reopened.has_managed_session_admission(&key).unwrap());
    assert_eq!(ledger(&reopened.connection().unwrap()).len(), 12);
  }

  #[test]
  fn verified_combined_migration_preserves_admissions_and_original_application_time() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("combined.sqlite");
    let connection = fixture(&path, true);
    let before = data(&connection, &["sources", "sessions"]);
    let admission = data(&connection, &["managed_session_admissions"]);
    let before_ledger = ledger(&connection);
    drop(connection);

    let index = SessionIndex::open(&path).unwrap();
    let connection = index.connection().unwrap();
    assert_eq!(data(&connection, &["sources", "sessions"]), before);
    assert_folder_upgrade(&connection);
    assert_eq!(data(&connection, &["managed_session_admissions"]), admission);
    let after = ledger(&connection);
    assert_eq!(after.len(), 12);
    assert_eq!(after[..9], before_ledger[..9]);
    assert_eq!(after[9], (10, migration_checksum(MIGRATION_010), 1010));
    assert_eq!(after[10], (11, migration_checksum(MIGRATION_011), 1010));
    drop(connection);
    drop(index);
    let reopened = SessionIndex::open(&path).unwrap();
    assert_eq!(ledger(&reopened.connection().unwrap()), after);
    assert!(
      reopened
        .has_managed_session_admission(&SessionKey::new("pi", "fixture", "owned"))
        .unwrap()
    );
  }

  #[test]
  fn concurrent_opens_adopt_a_combined_migration_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("concurrent.sqlite");
    drop(fixture(&path, true));
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
    assert!(
      index
        .has_managed_session_admission(&SessionKey::new("pi", "fixture", "owned"))
        .unwrap()
    );
    let applied = ledger(&index.connection().unwrap());
    assert_eq!(applied.len(), 12);
    assert_eq!(applied[10], (11, migration_checksum(MIGRATION_011), 1010));
  }

  #[test]
  fn malformed_combined_schema_is_rejected_without_ledger_or_data_changes() {
    for mutation in [
      "DROP TRIGGER revoke_deleted_managed_admission;",
      "DROP INDEX registered_projects_by_group;
       CREATE INDEX registered_projects_by_group ON registered_projects(name);",
      "CREATE INDEX unexpected_project_index ON registered_projects(name);",
      "CREATE TRIGGER unexpected_session_trigger AFTER UPDATE ON SeSsIoNs
       BEGIN DELETE FROM managed_session_admissions; END;",
      "CREATE TRIGGER unexpected_ledger_trigger AFTER UPDATE ON ScHeMa_MiGrAtIoNs
       BEGIN DELETE FROM registered_projects; END;",
      "ALTER TABLE registered_projects ADD COLUMN unexpected TEXT;",
      "DROP TABLE managed_session_admissions;
       CREATE TABLE managed_session_admissions (
         provider TEXT NOT NULL, source_key TEXT NOT NULL, session_id TEXT NOT NULL,
         project_id TEXT NOT NULL, PRIMARY KEY(provider, source_key, session_id));",
      "DROP TRIGGER revoke_deleted_managed_admission;
       CREATE TRIGGER revoke_deleted_managed_admission AFTER DELETE ON sources
       BEGIN DELETE FROM managed_session_admissions; END;",
    ] {
      let directory = tempfile::tempdir().unwrap();
      let path = directory.path().join("malformed.sqlite");
      let connection = fixture(&path, true);
      connection.execute_batch(mutation).unwrap();
      let before = data(&connection, EXISTING_DATA);
      let admissions = data(&connection, &["managed_session_admissions"]);
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
      assert_eq!(ledger(&connection), before_ledger, "{mutation}");
      assert_eq!(data(&connection, EXISTING_DATA), before, "{mutation}");
      assert_eq!(
        data(&connection, &["managed_session_admissions"]),
        admissions,
        "{mutation}"
      );
      assert_eq!(
        owned_schema(&connection, i64::MAX, i64::MAX).unwrap(),
        before_schema,
        "{mutation}"
      );
    }
  }

  #[test]
  fn unrelated_ledger_drift_rejects_before_combined_schema_adoption() {
    for version in [1, 11] {
      let directory = tempfile::tempdir().unwrap();
      let path = directory.path().join("drift.sqlite");
      let connection = fixture(&path, true);
      if version == 11 {
        connection
          .execute("INSERT INTO schema_migrations VALUES(11, 'unknown', 1111)", [])
          .unwrap();
      } else {
        connection
          .execute(
            "UPDATE schema_migrations SET checksum = 'unknown' WHERE version = 1",
            [],
          )
          .unwrap();
      }
      let before_ledger = ledger(&connection);
      let before = data(&connection, EXISTING_DATA);
      let admissions = data(&connection, &["managed_session_admissions"]);
      drop(connection);
      assert!(
        matches!(SessionIndex::open(&path), Err(SessionIndexError::MigrationChecksumMismatch { version: rejected, .. }) if rejected == version)
      );
      let connection = Connection::open(&path).unwrap();
      assert_eq!(ledger(&connection), before_ledger);
      assert_eq!(data(&connection, EXISTING_DATA), before);
      assert_eq!(data(&connection, &["managed_session_admissions"]), admissions);
    }
  }

  #[test]
  fn unobserved_migration_ten_checksum_is_rejected_without_adoption() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unobserved.sqlite");
    let connection = fixture(&path, true);
    connection
      .execute(
        "UPDATE schema_migrations SET checksum = '7d54311201dd7665' WHERE version = 10",
        [],
      )
      .unwrap();
    let before = ledger(&connection);
    drop(connection);
    assert!(matches!(
      SessionIndex::open(&path),
      Err(SessionIndexError::MigrationChecksumMismatch { version: 10, .. })
    ));
    assert_eq!(ledger(&Connection::open(path).unwrap()), before);
  }

  #[test]
  fn combined_migration_followed_by_a_recorded_eleven_is_not_a_supported_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("impossible.sqlite");
    let connection = fixture(&path, true);
    connection
      .execute(
        "INSERT INTO schema_migrations VALUES(11, ?1, 1111)",
        [migration_checksum(MIGRATION_011)],
      )
      .unwrap();
    let before_ledger = ledger(&connection);
    let before = data(&connection, EXISTING_DATA);
    let admissions = data(&connection, &["managed_session_admissions"]);
    drop(connection);
    assert!(matches!(
      SessionIndex::open(&path),
      Err(SessionIndexError::MigrationHistoryInvalid(_))
    ));
    let connection = Connection::open(path).unwrap();
    assert_eq!(ledger(&connection), before_ledger);
    assert_eq!(data(&connection, EXISTING_DATA), before);
    assert_eq!(data(&connection, &["managed_session_admissions"]), admissions);
  }
}
