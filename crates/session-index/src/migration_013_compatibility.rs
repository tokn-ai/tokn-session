//! Narrow adoption of the verified development migration 13 without revision
//! tracking. Existing discovery data is preserved; only its missing suffix runs.
use rusqlite::{Connection, Transaction, params};

use super::{MIGRATION_013, MIGRATION_LEDGER_SQL, MIGRATIONS, Result, SessionIndexError, migration_checksum};

const EARLY_CHECKSUM: &str = "41f6b99e276aa87d";
const EARLY_SQL: &str = include_str!("migration_013_compatibility/fixtures/early_013.sql");

type SchemaObject = (String, String, String, Option<String>);

pub(super) fn is_early_project_discovery(version: i64, checksum: &str) -> bool {
  version == 13 && checksum == EARLY_CHECKSUM
}

/// The caller validates the entire ledger first, requires this to be its final
/// entry, and holds the same immediate transaction through verification/adoption.
pub(super) fn adopt_early_project_discovery(transaction: &Transaction<'_>) -> Result<()> {
  let additive_sql = MIGRATION_013.strip_prefix(EARLY_SQL).ok_or_else(|| {
    SessionIndexError::MigrationHistoryInvalid("migration 13 no longer extends its verified early schema".into())
  })?;
  let reference = Connection::open_in_memory()?;
  reference.execute_batch(MIGRATION_LEDGER_SQL)?;
  // The compatibility boundary is fixed. Later migrations run only after this
  // adoption and must not silently alter the definition of the recognized DB.
  for migration in MIGRATIONS.iter().take(12) {
    reference.execute_batch(migration.sql)?;
  }
  reference.execute_batch(EARLY_SQL)?;
  verify_schema(transaction, &reference, "early migration 13")?;

  transaction.execute_batch(additive_sql)?;
  reference.execute_batch(additive_sql)?;
  verify_schema(transaction, &reference, "completed migration 13")?;
  // Preserve the original application time and every earlier ledger entry.
  let changed = transaction.execute(
    "UPDATE schema_migrations SET checksum = ?1 WHERE version = 13 AND checksum = ?2",
    params![migration_checksum(MIGRATION_013), EARLY_CHECKSUM],
  )?;
  if changed != 1 {
    return Err(SessionIndexError::MigrationHistoryInvalid(
      "early migration 13 ledger entry changed during adoption".into(),
    ));
  }
  Ok(())
}

fn verify_schema(connection: &Connection, reference: &Connection, boundary: &str) -> Result<()> {
  let expected = owned_schema(reference, i64::MAX, i64::MAX)?;
  let max_field_bytes = expected
    .iter()
    .flat_map(|object| {
      [
        object.0.len(),
        object.1.len(),
        object.2.len(),
        object.3.as_ref().map_or(0, String::len),
      ]
    })
    .max()
    .unwrap_or(0) as i64;
  // An extra row and character prevent longer/extra objects from comparing
  // equal while bounding allocation from an unexpected database definition.
  let actual = owned_schema(connection, expected.len() as i64 + 1, max_field_bytes + 1)?;
  if actual != expected {
    return Err(SessionIndexError::MigrationHistoryInvalid(format!(
      "{boundary} does not match the verified project discovery schema"
    )));
  }
  Ok(())
}

/// Compare every app-owned object, including triggers on the migration ledger
/// or tables unrelated to folders. Unknown/custom definitions are not adopted.
fn owned_schema(connection: &Connection, row_limit: i64, field_limit: i64) -> Result<Vec<SchemaObject>> {
  let mut statement = connection.prepare(
    "SELECT substr(type, 1, ?1), substr(name, 1, ?1), substr(tbl_name, 1, ?1), substr(sql, 1, ?1)
     FROM main.sqlite_schema WHERE lower(substr(name, 1, 7)) != 'sqlite_'
     ORDER BY type, name, tbl_name LIMIT ?2",
  )?;
  statement
    .query_map(params![field_limit, row_limit], |row| {
      Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?
    .collect::<rusqlite::Result<_>>()
    .map_err(Into::into)
}

#[cfg(test)]
mod tests;
