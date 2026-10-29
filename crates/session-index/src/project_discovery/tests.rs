use super::*;
use crate::{SessionMetadata, SourceReplacement};

fn observed(id: &str, cwd: &str, folder: &str) -> ProjectFolderObservation {
  ProjectFolderObservation {
    cwd: cwd.into(),
    project: RegisteredProjectRecord {
      project_id: id.into(),
      group_key: "/repository".into(),
      folder_path: folder.into(),
      name: format!("Project {id}"),
      created_at_ms: 10,
    },
  }
}

fn reconcile(index: &SessionIndex, provider: &str, observations: &[ProjectFolderObservation], complete: bool) -> bool {
  let checkpoint = index.project_discovery_checkpoint(provider).unwrap();
  index
    .reconcile_project_folders(provider, &checkpoint, observations, complete)
    .unwrap()
}

#[test]
fn unchanged_discovery_does_not_write_but_new_aliases_fence_older_checkpoints() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("index.sqlite");
  let writer = SessionIndex::open(&path).unwrap();
  let reader = SessionIndex::open(&path).unwrap();
  let first = observed("first", "/alias", "/repository/checkout");
  reconcile(&writer, "codex", &[first.clone()], true);
  let checkpoint = writer.project_discovery_checkpoint("codex").unwrap();
  let version = reader.data_version().unwrap();
  assert!(!reconcile(&writer, "codex", &[first.clone()], true));
  assert_eq!(writer.project_discovery_checkpoint("codex").unwrap(), checkpoint);
  assert_eq!(reader.data_version().unwrap(), version);
  let alias = observed("duplicate", "/new-alias", "/repository/checkout");
  assert!(!reconcile(&writer, "codex", &[first, alias], true));
  assert!(writer.project_discovery_checkpoint("codex").unwrap().revision > checkpoint.revision);
  assert!(reader.data_version().unwrap() > version);
  assert!(matches!(
    writer.reconcile_project_folders("codex", &checkpoint, &[], true),
    Err(SessionIndexError::ProjectDiscoveryConflict { .. })
  ));
}

#[test]
fn provider_union_preserves_identity_until_every_confirmed_reference_is_gone() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("index.sqlite");
  let index = SessionIndex::open(&path).unwrap();
  let first = observed("first", "/checkout-alias", "/repository/checkout");
  assert!(reconcile(&index, "codex", &[first.clone()], true));
  assert!(!reconcile(&index, "codex", &[first.clone()], true));
  assert!(reconcile(
    &index,
    "pi",
    &[observed("duplicate", "/repository/checkout", "/repository/checkout")],
    true
  ));
  let folder = index.get_project_folder("first").unwrap().unwrap();
  assert_eq!(folder.project, first.project);
  assert_eq!(folder.origin, ProjectOrigin::Discovered);
  assert_eq!(folder.discovery_state, ProjectDiscoveryState::Present);
  assert_eq!(folder.providers, ["codex", "pi"]);
  reconcile(&index, "codex", &[], true);
  let folder = index.get_project_folder("first").unwrap().unwrap();
  assert!(folder.has_agent_sessions);
  assert_eq!(folder.providers, ["pi"]);
  reconcile(&index, "pi", &[], true);
  let folder = index.get_project_folder("first").unwrap().unwrap();
  assert!(!folder.has_agent_sessions);
  assert_eq!(folder.discovery_state, ProjectDiscoveryState::Absent);
  assert_eq!(folder.project, first.project);
  drop(index);
  let index = SessionIndex::open(path).unwrap();
  assert_eq!(
    index.project_folder_for_cwd("codex", "/checkout-alias").unwrap(),
    Some(first.project.clone())
  );
  reconcile(
    &index,
    "pi",
    &[observed("new-id", "/repository/checkout", "/repository/checkout")],
    true,
  );
  let folder = index.get_project_folder("first").unwrap().unwrap();
  assert!(folder.has_agent_sessions);
  assert_eq!(folder.project, first.project);
  assert_eq!(index.list_project_folders().unwrap().len(), 1);
}

#[test]
fn incomplete_discovery_preserves_last_known_presence_without_resurrecting_absent_folders() {
  let index = SessionIndex::open_in_memory().unwrap();
  let known = observed("known", "/known", "/known");
  reconcile(&index, "codex", &[known.clone()], true);
  reconcile(&index, "codex", &[], false);
  let folder = index.get_project_folder("known").unwrap().unwrap();
  assert_eq!(folder.discovery_state, ProjectDiscoveryState::Unknown);
  assert!(folder.has_agent_sessions);
  reconcile(&index, "codex", &[], true);
  reconcile(&index, "codex", &[], false);
  let folder = index.get_project_folder("known").unwrap().unwrap();
  assert_eq!(folder.discovery_state, ProjectDiscoveryState::Unknown);
  assert!(!folder.has_agent_sessions);
  assert!(folder.providers.is_empty());
  // Positive evidence can be learned even when unrelated histories failed.
  reconcile(&index, "codex", &[known], false);
  assert_eq!(
    index.get_project_folder("known").unwrap().unwrap().discovery_state,
    ProjectDiscoveryState::Present
  );
}

#[test]
fn manual_registration_promotes_discovery_and_preserves_names_ids_order_and_admissions() {
  let index = SessionIndex::open_in_memory().unwrap();
  let folder = observed("known", "/known", "/known");
  reconcile(&index, "codex", &[folder.clone()], true);
  let key = crate::SessionKey::new("codex", "/history", "session");
  let mut session = SessionMetadata::new(key.clone(), "/history");
  session.cwd = Some("/known".into());
  index
    .replace_source(SourceReplacement::new(
      SourceState::new(key.source_key(), "1", 11),
      vec![session],
    ))
    .unwrap();
  index.admit_managed_session(&key, "known").unwrap();
  let mut manual = observed("manual-id", "/known", "/known").project;
  manual.name = "A different requested name".into();
  manual.created_at_ms = 90;
  assert_eq!(index.register_project(manual).unwrap(), folder.project);
  reconcile(&index, "codex", &[], true);
  let record = index.get_project_folder("known").unwrap().unwrap();
  assert_eq!(record.origin, ProjectOrigin::Manual);
  assert!(!record.has_agent_sessions);
  assert_eq!(record.project, folder.project);
  assert_eq!(index.project_order(&[], 100).unwrap()["/repository"], 10);
  assert!(index.has_managed_session_admission(&key).unwrap());
}

#[test]
fn resolving_an_alias_elsewhere_never_retargets_an_existing_folder() {
  let index = SessionIndex::open_in_memory().unwrap();
  let old = observed("old", "/alias", "/old-target");
  reconcile(&index, "codex", &[old.clone()], true);
  // A missing alias is still associated with its former canonical identity.
  assert_eq!(
    index.project_folder_for_cwd("codex", "/alias").unwrap(),
    Some(old.project.clone())
  );
  let new = observed("new", "/alias", "/new-target");
  reconcile(&index, "codex", &[new.clone()], true);
  assert_eq!(
    index.project_folder_for_cwd("codex", "/alias").unwrap(),
    Some(new.project)
  );
  let previous = index.get_project_folder("old").unwrap().unwrap();
  assert_eq!(previous.project, old.project);
  assert_eq!(previous.discovery_state, ProjectDiscoveryState::Absent);
  assert_eq!(index.list_project_folders().unwrap().len(), 2);
}

#[test]
fn source_changes_and_other_reconciliation_transactions_fence_stale_folder_updates() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("index.sqlite");
  let first = SessionIndex::open(&path).unwrap();
  let second = SessionIndex::open(&path).unwrap();
  let stale = first.project_discovery_checkpoint("codex").unwrap();
  let source = SourceKey::new("codex", "/history");
  second
    .replace_source(SourceReplacement::new(
      SourceState::new(source.clone(), "1", 1),
      Vec::new(),
    ))
    .unwrap();
  assert!(matches!(
    first.reconcile_project_folders("codex", &stale, &[], true),
    Err(SessionIndexError::ProjectDiscoveryConflict { .. })
  ));
  let observed_folder = observed("known", "/known", "/known");
  reconcile(&first, "codex", &[observed_folder], true);
  let stale = first.project_discovery_checkpoint("codex").unwrap();
  // Even a same-cursor source update has a new committed generation.
  second
    .replace_source(SourceReplacement::new(SourceState::new(source, "1", 1), Vec::new()))
    .unwrap();
  assert!(matches!(
    first.reconcile_project_folders("codex", &stale, &[], true),
    Err(SessionIndexError::ProjectDiscoveryConflict { .. })
  ));
  let stale = first.project_discovery_checkpoint("codex").unwrap();
  reconcile(&second, "codex", &[], false);
  assert!(matches!(
    first.reconcile_project_folders("codex", &stale, &[], true),
    Err(SessionIndexError::ProjectDiscoveryConflict { .. })
  ));
  assert!(first.get_project_folder("known").unwrap().unwrap().has_agent_sessions);
}

#[test]
fn migration_preserves_existing_manual_folders_and_their_admission_references() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("index.sqlite");
  let connection = Connection::open(&path).unwrap();
  connection
    .pragma_update(None, "application_id", crate::APPLICATION_ID)
    .unwrap();
  connection.execute_batch(crate::MIGRATION_LEDGER_SQL).unwrap();
  for migration in crate::MIGRATIONS.iter().filter(|migration| migration.version <= 12) {
    connection.execute_batch(migration.sql).unwrap();
    connection
      .execute(
        "INSERT INTO schema_migrations VALUES(?1, ?2, 1)",
        params![migration.version, crate::migration_checksum(migration.sql)],
      )
      .unwrap();
  }
  connection
    .execute("INSERT INTO projects VALUES('/old', 1, 1)", [])
    .unwrap();
  connection
    .execute(
      "INSERT INTO registered_projects VALUES('old', '/old', '/old', 'Saved name', 3)",
      [],
    )
    .unwrap();
  connection
    .execute(
      "INSERT INTO managed_session_admissions VALUES('pi', '/history', 'session', 'old')",
      [],
    )
    .unwrap();
  drop(connection);
  let index = SessionIndex::open(path).unwrap();
  let folder = index.get_project_folder("old").unwrap().unwrap();
  assert_eq!(folder.origin, ProjectOrigin::Manual);
  assert_eq!(folder.project.name, "Saved name");
  assert_eq!(folder.project.created_at_ms, 3);
  assert_eq!(folder.discovery_state, ProjectDiscoveryState::Absent);
  assert!(!folder.has_agent_sessions);
  let connection = index.connection().unwrap();
  assert_eq!(
    connection
      .query_row("SELECT project_id FROM managed_session_admissions", [], |row| row
        .get::<_, String>(0))
      .unwrap(),
    "old"
  );
}

#[test]
fn add_only_bootstrap_preserves_known_states_and_registry_revision_ignores_source_writes() {
  let index = SessionIndex::open_in_memory().unwrap();
  let present = observed("present", "/present", "/present");
  let absent = observed("absent", "/absent", "/absent");
  reconcile(&index, "codex", &[present.clone(), absent], true);
  reconcile(&index, "codex", &[present.clone()], true);
  let revision = index.project_folder_revision().unwrap();
  index
    .replace_source(SourceReplacement::new(
      SourceState::new(SourceKey::new("codex", "/history"), "cursor", 1),
      Vec::new(),
    ))
    .unwrap();
  assert_eq!(index.project_folder_revision().unwrap(), revision);
  let checkpoint = index.project_discovery_checkpoint("codex").unwrap();
  index
    .add_project_folder_observations("codex", &checkpoint, &[observed("new", "/new", "/new")])
    .unwrap();
  assert_eq!(
    index.get_project_folder("present").unwrap().unwrap().discovery_state,
    ProjectDiscoveryState::Present
  );
  assert_eq!(
    index.get_project_folder("absent").unwrap().unwrap().discovery_state,
    ProjectDiscoveryState::Absent
  );
  assert!(index.project_folder_revision().unwrap() > revision);
  let revision = index.project_folder_revision().unwrap();
  index.register_project(present.project).unwrap();
  assert!(index.project_folder_revision().unwrap() > revision);
  let revision = index.project_folder_revision().unwrap();
  index.project_order(&["/new-group".into()], 2).unwrap();
  index
    .connection()
    .unwrap()
    .execute(
      "UPDATE registered_projects SET group_key = '/new-group' WHERE project_id = 'present'",
      [],
    )
    .unwrap();
  assert!(index.project_folder_revision().unwrap() > revision);
}
