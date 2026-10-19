use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{Result, SessionIndex, SessionIndexError, SessionKey};

impl SessionIndex {
  /// Admits one verified viewer-created session before its provider's complete
  /// catalog commits. The caller must first verify native identity and project
  /// ownership; this derived record never makes the provider catalog ready.
  pub fn admit_managed_session(&self, key: &SessionKey, project_id: &str) -> Result<()> {
    let mut connection = self.connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let present: bool = transaction.query_row(
      "SELECT EXISTS(SELECT 1 FROM sessions AS session
       INNER JOIN sources AS source ON source.id = session.source_id
       WHERE source.provider = ?1 AND source.source_key = ?2
         AND session.session_id = ?3 AND session.present = 1)",
      params![key.provider, key.source_key, key.session_id],
      |row| row.get(0),
    )?;
    if !present {
      return Err(SessionIndexError::InvalidAdmission(
        "session is not present in the index".into(),
      ));
    }
    let project_exists: bool = transaction.query_row(
      "SELECT EXISTS(SELECT 1 FROM registered_projects WHERE project_id = ?1)",
      [project_id],
      |row| row.get(0),
    )?;
    if !project_exists {
      return Err(SessionIndexError::InvalidAdmission("project is not registered".into()));
    }
    let existing: Option<String> = transaction
      .query_row(
        "SELECT project_id FROM managed_session_admissions
       WHERE provider = ?1 AND source_key = ?2 AND session_id = ?3",
        params![key.provider, key.source_key, key.session_id],
        |row| row.get(0),
      )
      .optional()?;
    if existing.as_deref().is_some_and(|existing| existing != project_id) {
      return Err(SessionIndexError::InvalidAdmission(
        "session is already admitted for another project".into(),
      ));
    }
    transaction.execute(
      "INSERT INTO managed_session_admissions(provider, source_key, session_id, project_id)
       VALUES(?1, ?2, ?3, ?4) ON CONFLICT(provider, source_key, session_id) DO NOTHING",
      params![key.provider, key.source_key, key.session_id, project_id],
    )?;
    transaction.commit()?;
    Ok(())
  }

  /// Tests the exact admitted key and current presence. Tombstones permanently
  /// retire admissions, so a later replacement must be explicitly re-admitted.
  pub fn has_managed_session_admission(&self, key: &SessionKey) -> Result<bool> {
    let connection = self.connection()?;
    connection
      .query_row(
        "SELECT EXISTS(SELECT 1 FROM managed_session_admissions AS admission
       INNER JOIN sources AS source
         ON source.provider = admission.provider AND source.source_key = admission.source_key
       INNER JOIN sessions AS session
         ON session.source_id = source.id AND session.session_id = admission.session_id
       WHERE admission.provider = ?1 AND admission.source_key = ?2
         AND admission.session_id = ?3 AND session.present = 1)",
        params![key.provider, key.source_key, key.session_id],
        |row| row.get(0),
      )
      .map_err(Into::into)
  }

  /// Returns only the small explicit admission set, avoiding one SQL lookup
  /// per partial catalog row when building a snapshot service inventory.
  pub fn managed_session_admissions(&self) -> Result<Vec<SessionKey>> {
    let connection = self.connection()?;
    let mut statement = connection.prepare(
      "SELECT admission.provider, admission.source_key, admission.session_id
       FROM managed_session_admissions AS admission
       INNER JOIN sources AS source
         ON source.provider = admission.provider AND source.source_key = admission.source_key
       INNER JOIN sessions AS session
         ON session.source_id = source.id AND session.session_id = admission.session_id
       WHERE session.present = 1
       ORDER BY admission.provider, admission.source_key, admission.session_id",
    )?;
    let rows = statement.query_map([], |row| {
      Ok(SessionKey::new(
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
      ))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{RegisteredProjectRecord, SessionMetadata, SourceReplacement, SourceState};

  #[test]
  fn admission_requires_exact_present_key_and_project_and_tombstones_revoke_it() {
    let index = SessionIndex::open_in_memory().unwrap();
    let key = SessionKey::new("pi", "path.v1./history", "owned");
    let source = key.source_key();
    let metadata = SessionMetadata::new(key.clone(), "/history");
    assert!(index.admit_managed_session(&key, "project").is_err());
    index
      .replace_source(SourceReplacement::baseline(
        SourceState::new(source.clone(), "one", 0),
        vec![metadata.clone()],
      ))
      .unwrap();
    assert!(index.admit_managed_session(&key, "project").is_err());
    index
      .register_project(RegisteredProjectRecord {
        project_id: "project".into(),
        group_key: "/project".into(),
        folder_path: "/project".into(),
        name: "Project".into(),
        created_at_ms: 0,
      })
      .unwrap();
    assert!(
      index
        .admit_managed_session(&SessionKey::new("pi", "path.v1./other", "owned"), "project")
        .is_err()
    );
    index.admit_managed_session(&key, "project").unwrap();
    index.admit_managed_session(&key, "project").unwrap();
    assert!(index.has_managed_session_admission(&key).unwrap());
    assert!(
      !index
        .has_managed_session_admission(&SessionKey::new("codex", &key.source_key, &key.session_id))
        .unwrap()
    );
    assert_eq!(index.managed_session_admissions().unwrap(), [key.clone()]);
    index
      .replace_source(SourceReplacement::baseline(
        SourceState::new(source.clone(), "append", 1),
        vec![metadata.clone()],
      ))
      .unwrap();
    assert!(
      index.has_managed_session_admission(&key).unwrap(),
      "ordinary source replacement keeps exact admission"
    );
    index
      .replace_source(SourceReplacement::baseline(
        SourceState::new(source.clone(), "two", 1),
        vec![],
      ))
      .unwrap();
    assert!(!index.has_managed_session_admission(&key).unwrap());
    index
      .replace_source(SourceReplacement::baseline(
        SourceState::new(source, "three", 2),
        vec![metadata],
      ))
      .unwrap();
    assert!(
      !index.has_managed_session_admission(&key).unwrap(),
      "revival requires explicit ownership proof again"
    );
    assert!(index.managed_session_admissions().unwrap().is_empty());
  }
}
