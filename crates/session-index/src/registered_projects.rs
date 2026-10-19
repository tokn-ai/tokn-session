use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{Result, SessionIndex, SessionIndexError, remember_project};

/// One explicitly registered launch folder. The caller owns canonical path
/// resolution; the index preserves its identity independently of sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredProjectRecord {
  pub project_id: String,
  pub group_key: String,
  pub folder_path: String,
  pub name: String,
  pub created_at_ms: i64,
}

impl SessionIndex {
  /// Registers one canonical folder without renaming a previous registration.
  /// The existing group anchor keeps its original discovery and sort times.
  pub fn register_project(&self, project: RegisteredProjectRecord) -> Result<RegisteredProjectRecord> {
    validate_project(&project)?;
    let mut connection = self.connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(existing) = transaction
      .query_row(
        "SELECT project_id, group_key, folder_path, name, created_at_ms
         FROM registered_projects WHERE folder_path = ?1",
        [&project.folder_path],
        project_from_row,
      )
      .optional()?
    {
      transaction.commit()?;
      return Ok(existing);
    }
    let existing_group: Option<String> = transaction
      .query_row(
        "SELECT project_key FROM project_aliases WHERE cwd = ?1",
        [&project.folder_path],
        |row| row.get(0),
      )
      .optional()?;
    if existing_group.is_some_and(|group| group != project.group_key) {
      return Err(SessionIndexError::InvalidProject(
        "folder already belongs to a different saved project group".into(),
      ));
    }
    remember_project(&transaction, &project.group_key, project.created_at_ms)?;
    // The launch folder can be a canonical spelling of an older inferred
    // cwd. Bind it to the selected group without overwriting a saved alias.
    transaction.execute(
      "INSERT INTO project_aliases(cwd, project_key) VALUES(?1, ?2)
       ON CONFLICT(cwd) DO NOTHING",
      params![project.folder_path, project.group_key],
    )?;
    transaction.execute(
      "INSERT INTO registered_projects(project_id, group_key, folder_path, name, created_at_ms)
       VALUES(?1, ?2, ?3, ?4, ?5)",
      params![
        project.project_id,
        project.group_key,
        project.folder_path,
        project.name,
        project.created_at_ms,
      ],
    )?;
    transaction.commit()?;
    Ok(project)
  }

  /// Lists registrations even when no provider sessions currently exist.
  pub fn list_registered_projects(&self) -> Result<Vec<RegisteredProjectRecord>> {
    let connection = self.connection()?;
    let mut statement = connection.prepare(
      "SELECT project_id, group_key, folder_path, name, created_at_ms
       FROM registered_projects ORDER BY created_at_ms DESC, project_id ASC",
    )?;
    let rows = statement.query_map([], project_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
  }

  /// Looks up one saved identity without consulting provider storage.
  pub fn get_registered_project(&self, project_id: &str) -> Result<Option<RegisteredProjectRecord>> {
    let connection = self.connection()?;
    connection
      .query_row(
        "SELECT project_id, group_key, folder_path, name, created_at_ms
         FROM registered_projects WHERE project_id = ?1",
        [project_id],
        project_from_row,
      )
      .optional()
      .map_err(Into::into)
  }
}

fn validate_project(project: &RegisteredProjectRecord) -> Result<()> {
  for (field, value, max_bytes) in [
    ("project_id", project.project_id.as_str(), 128),
    ("group_key", project.group_key.as_str(), 4096),
    ("folder_path", project.folder_path.as_str(), 4096),
  ] {
    if value.trim().is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
      return Err(SessionIndexError::InvalidProject(format!(
        "{field} is empty, too long, or contains control characters"
      )));
    }
  }
  if project.name.trim().is_empty() || project.name.chars().count() > 160 || project.name.chars().any(char::is_control)
  {
    return Err(SessionIndexError::InvalidProject(
      "name must contain 1–160 characters without control characters".into(),
    ));
  }
  Ok(())
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegisteredProjectRecord> {
  Ok(RegisteredProjectRecord {
    project_id: row.get(0)?,
    group_key: row.get(1)?,
    folder_path: row.get(2)?,
    name: row.get(3)?,
    created_at_ms: row.get(4)?,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn project(id: &str, folder: &str, created: i64) -> RegisteredProjectRecord {
    RegisteredProjectRecord {
      project_id: id.into(),
      group_key: "/repository".into(),
      folder_path: folder.into(),
      name: format!("Project {id}"),
      created_at_ms: created,
    }
  }

  #[test]
  fn empty_project_registrations_survive_reopen_and_do_not_rename_duplicates() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let index = SessionIndex::open(&path).unwrap();
    index.project_order(&["/repository".into()], 10).unwrap();
    let first = index
      .register_project(project("first", "/repository/main", 20))
      .unwrap();
    assert_eq!(
      index
        .register_project(project("duplicate", "/repository/main", 30))
        .unwrap(),
      first
    );
    let second = index
      .register_project(project("second", "/repository/task", 40))
      .unwrap();
    assert_eq!(second.group_key, first.group_key);
    assert_eq!(
      index.project_order(&["/repository".into()], 50).unwrap()["/repository"],
      10
    );
    assert!(index.list_present_sessions().unwrap().is_empty());
    drop(index);
    let index = SessionIndex::open(path).unwrap();
    assert_eq!(index.list_registered_projects().unwrap(), [second, first.clone()]);
    assert_eq!(index.get_registered_project("first").unwrap(), Some(first));
    assert!(index.get_registered_project("missing").unwrap().is_none());
  }

  #[test]
  fn concurrent_folder_registration_returns_the_same_winning_record() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let first = SessionIndex::open(&path).unwrap();
    let second = SessionIndex::open(&path).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let ready = barrier.clone();
    let first = std::thread::spawn(move || {
      ready.wait();
      first
        .register_project(project("first", "/repository/main", 10))
        .unwrap()
    });
    let second = std::thread::spawn(move || {
      barrier.wait();
      second
        .register_project(project("second", "/repository/main", 20))
        .unwrap()
    });
    assert_eq!(first.join().unwrap(), second.join().unwrap());
    assert_eq!(
      SessionIndex::open(path)
        .unwrap()
        .list_registered_projects()
        .unwrap()
        .len(),
      1
    );
  }

  #[test]
  fn invalid_registration_rolls_back_without_creating_a_group_anchor() {
    let index = SessionIndex::open_in_memory().unwrap();
    let mut invalid = project("invalid", "/repository/main", 10);
    invalid.name = "\n".into();
    assert!(matches!(
      index.register_project(invalid),
      Err(SessionIndexError::InvalidProject(_))
    ));
    assert!(index.list_registered_projects().unwrap().is_empty());
    assert!(index.project_order(&[], 20).unwrap().is_empty());
  }

  #[test]
  fn registration_never_overwrites_an_existing_folder_alias() {
    let index = SessionIndex::open_in_memory().unwrap();
    let mut first = project("first", "/repository/main", 10);
    first.group_key = "/existing-group".into();
    index.register_project(first).unwrap();
    // An inferred alias can exist before a folder is explicitly registered.
    index
      .connection()
      .unwrap()
      .execute("DELETE FROM registered_projects", [])
      .unwrap();
    assert!(matches!(
      index.register_project(project("new", "/repository/main", 20)),
      Err(SessionIndexError::InvalidProject(_))
    ));
    assert!(index.list_registered_projects().unwrap().is_empty());
    let groups = index.project_groups(&["/repository/main".into()], 30).unwrap();
    assert_eq!(groups["/repository/main"].project_key, "/existing-group");
    assert_eq!(groups["/repository/main"].order_ms, 10);
  }
}
