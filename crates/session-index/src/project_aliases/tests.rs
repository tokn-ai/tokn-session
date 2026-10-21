use std::{
  fs,
  path::{Path, PathBuf},
};

use rusqlite::{Connection, params};

use crate::{APPLICATION_ID, MIGRATION_LEDGER_SQL, MIGRATIONS, SessionIndex, migration_checksum};

fn legacy_index(path: &Path) -> Connection {
  let connection = Connection::open(path).unwrap();
  connection
    .pragma_update(None, "application_id", APPLICATION_ID)
    .unwrap();
  connection.execute_batch(MIGRATION_LEDGER_SQL).unwrap();
  for migration in MIGRATIONS.iter().take(11) {
    connection.execute_batch(migration.sql).unwrap();
    connection
      .execute(
        "INSERT INTO schema_migrations VALUES(?1, ?2, 0)",
        params![migration.version, migration_checksum(migration.sql)],
      )
      .unwrap();
  }
  connection
}

fn anchor(connection: &Connection, folder: &Path, time: i64) {
  connection
    .execute(
      "INSERT OR IGNORE INTO projects VALUES(?1, ?2, ?2)",
      params![folder.to_str().unwrap(), time],
    )
    .unwrap();
}

fn legacy_alias(connection: &Connection, folder: &Path, root: &Path) {
  connection
    .execute(
      "INSERT INTO project_aliases(cwd, project_key) VALUES(?1, ?2)",
      params![folder.to_str().unwrap(), root.to_str().unwrap()],
    )
    .unwrap();
}

fn worktree(repository: &Path, checkout: &Path, name: &str) -> PathBuf {
  let registration = repository.join(".git/worktrees").join(name);
  fs::create_dir_all(&registration).unwrap();
  fs::create_dir_all(checkout).unwrap();
  fs::write(checkout.join(".git"), format!("gitdir: {}", registration.display())).unwrap();
  fs::write(registration.join("commondir"), "../..").unwrap();
  fs::write(registration.join("gitdir"), checkout.join(".git").to_str().unwrap()).unwrap();
  registration
}

#[test]
fn non_git_folders_do_not_inherit_home_or_repository_ancestors() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let home = root.join("home");
  let project = home.join("projects/app");
  let nested = project.join("notes");
  fs::create_dir_all(home.join(".git")).unwrap();
  fs::create_dir_all(project.join(".git")).unwrap();
  fs::create_dir_all(&nested).unwrap();
  let ordinary = home.join("scratch");
  fs::create_dir_all(&ordinary).unwrap();
  let index = SessionIndex::open_in_memory().unwrap();
  let keys = [&project, &nested, &ordinary].map(|path| path.to_str().unwrap().to_owned());
  let groups = index.project_groups(&keys, 10).unwrap();
  for key in &keys {
    assert_eq!(groups[key].project_key, *key);
  }
}

#[test]
fn legacy_ancestor_aliases_split_once_and_preserve_each_folder_anchor() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let home = root.join("home");
  let plain = home.join("scratch");
  let repository = home.join("app");
  let nested = repository.join("notes");
  let missing = home.join("removed");
  fs::create_dir_all(home.join(".git")).unwrap();
  fs::create_dir_all(&plain).unwrap();
  fs::create_dir_all(repository.join(".git")).unwrap();
  fs::create_dir_all(&nested).unwrap();
  let path = root.join("index.sqlite");
  let connection = legacy_index(&path);
  anchor(&connection, &home, 1);
  for (folder, time) in [(&plain, 10), (&repository, 20), (&nested, 30), (&missing, 40)] {
    anchor(&connection, folder, time);
    legacy_alias(&connection, folder, &home);
  }
  drop(connection);
  let keys = [&plain, &repository, &nested, &missing].map(|folder| folder.to_str().unwrap().to_owned());
  for now in [100, 200] {
    let index = SessionIndex::open(&path).unwrap();
    let groups = index.project_groups(&keys, now).unwrap();
    for (key, time) in keys.iter().zip([10, 20, 30, 40]) {
      assert_eq!(groups[key].project_key, *key);
      assert_eq!(groups[key].order_ms, time);
    }
    let connection = index.connection().unwrap();
    let count: i64 = connection
      .query_row(
        "SELECT count(*) FROM project_aliases WHERE provenance = 'legacy'",
        [],
        |row| row.get(0),
      )
      .unwrap();
    assert_eq!(count, 0);
  }
}

#[test]
fn exact_legacy_worktree_registration_is_proof_but_its_subfolder_is_not() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let repository = root.join("repository");
  let checkout = root.join("checkout");
  let nested = checkout.join("notes");
  let registration = worktree(&repository, &checkout, "task");
  let path = root.join("index.sqlite");
  let connection = legacy_index(&path);
  anchor(&connection, &repository, 1);
  for folder in [&checkout, &nested] {
    anchor(&connection, folder, 10);
    legacy_alias(&connection, folder, &repository);
  }
  fs::remove_dir_all(&checkout).unwrap();
  drop(connection);
  let index = SessionIndex::open(&path).unwrap();
  let checkout_key = checkout.to_str().unwrap().to_owned();
  let nested_key = nested.to_str().unwrap().to_owned();
  let keys = [checkout_key.clone(), nested_key.clone()];
  let groups = index.project_groups(&keys, 20).unwrap();
  assert_eq!(groups[&checkout_key].project_key, repository.to_str().unwrap());
  assert_eq!(groups[&nested_key].project_key, nested_key);
  drop(index);
  fs::remove_dir_all(registration).unwrap();
  let index = SessionIndex::open(&path).unwrap();
  assert_eq!(
    index.project_groups(&keys, 30).unwrap()[&checkout_key].project_key,
    repository.to_str().unwrap()
  );
}

#[test]
fn gitfile_without_common_directory_keeps_the_checkout_folder() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let folder = root.join("submodule");
  let administrative = root.join("repository/.git/modules/child");
  fs::create_dir_all(&folder).unwrap();
  fs::create_dir_all(&administrative).unwrap();
  fs::write(folder.join(".git"), format!("gitdir: {}", administrative.display())).unwrap();
  let key = folder.to_str().unwrap().to_owned();
  let index = SessionIndex::open_in_memory().unwrap();
  assert_eq!(
    index.project_groups(std::slice::from_ref(&key), 10).unwrap()[&key].project_key,
    key
  );
}

#[test]
fn bare_common_directory_recovers_its_exact_registered_worktree() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let common = root.join("repository.git");
  let checkout = root.join("checkout");
  let registration = common.join("worktrees/task");
  fs::create_dir_all(&registration).unwrap();
  fs::create_dir(common.join("objects")).unwrap();
  fs::write(common.join("HEAD"), "ref: refs/heads/main\n").unwrap();
  fs::write(registration.join("gitdir"), checkout.join(".git").to_str().unwrap()).unwrap();
  let path = root.join("index.sqlite");
  let connection = legacy_index(&path);
  anchor(&connection, &common, 1);
  anchor(&connection, &checkout, 10);
  legacy_alias(&connection, &checkout, &common);
  drop(connection);
  let index = SessionIndex::open(path).unwrap();
  let key = checkout.to_str().unwrap().to_owned();
  assert_eq!(
    index.project_groups(std::slice::from_ref(&key), 20).unwrap()[&key].project_key,
    common.to_str().unwrap()
  );
}

#[cfg(unix)]
#[test]
fn upgrade_preserves_a_proven_canonical_registration_spelling_and_identity() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let folder = root.join("folder");
  let spelling = root.join("alias");
  fs::create_dir(&folder).unwrap();
  std::os::unix::fs::symlink(&folder, &spelling).unwrap();
  let path = root.join("index.sqlite");
  let connection = legacy_index(&path);
  anchor(&connection, &spelling, 10);
  legacy_alias(&connection, &folder, &spelling);
  connection
    .execute(
      "INSERT INTO registered_projects VALUES('saved', ?1, ?2, 'Custom name', 30)",
      params![spelling.to_str().unwrap(), folder.to_str().unwrap()],
    )
    .unwrap();
  drop(connection);
  let index = SessionIndex::open(path).unwrap();
  let project = index.get_registered_project("saved").unwrap().unwrap();
  assert_eq!(project.project_id, "saved");
  assert_eq!(project.name, "Custom name");
  assert_eq!(project.created_at_ms, 30);
  assert_eq!(project.folder_path, folder.to_str().unwrap());
  assert_eq!(project.group_key, spelling.to_str().unwrap());
  let groups = index
    .project_groups(std::slice::from_ref(&project.folder_path), 100)
    .unwrap();
  assert_eq!(groups[&project.folder_path].order_ms, 10);
  fs::create_dir(folder.join(".git")).unwrap();
  let refreshed = index.get_registered_project("saved").unwrap().unwrap();
  assert_eq!(refreshed, project);
  let groups = index
    .project_groups(std::slice::from_ref(&project.folder_path), 200)
    .unwrap();
  assert_eq!(groups[&project.folder_path].project_key, project.group_key);
  assert_eq!(groups[&project.folder_path].order_ms, 10);
}

#[test]
fn old_process_legacy_writes_cannot_freeze_new_registration_groups() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let parent = root.join("parent");
  let folder = parent.join("folder");
  fs::create_dir_all(parent.join(".git")).unwrap();
  fs::create_dir_all(&folder).unwrap();
  let index = SessionIndex::open_in_memory().unwrap();
  {
    let connection = index.connection().unwrap();
    anchor(&connection, &parent, 1);
    anchor(&connection, &folder, 10);
    legacy_alias(&connection, &folder, &parent);
    connection
      .execute(
        "INSERT INTO registered_projects VALUES('late', ?1, ?2, 'Late name', 30)",
        params![parent.to_str().unwrap(), folder.to_str().unwrap()],
      )
      .unwrap();
  }
  let project = index.list_registered_projects().unwrap().remove(0);
  assert_eq!(project.group_key, folder.to_str().unwrap());
  assert_eq!(project.name, "Late name");
  assert_eq!(project.created_at_ms, 30);
  assert_eq!(index.get_registered_project("late").unwrap().unwrap(), project);
  let groups = index
    .project_groups(std::slice::from_ref(&project.folder_path), 100)
    .unwrap();
  assert_eq!(groups[&project.folder_path].order_ms, 10);
}

#[test]
fn current_direct_marker_replaces_cached_worktree_and_registration_group() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let first = root.join("first");
  let second = root.join("second");
  let checkout = root.join("checkout");
  let old_registration = worktree(&first, &checkout, "task");
  let key = checkout.to_str().unwrap().to_owned();
  let index = SessionIndex::open_in_memory().unwrap();
  index.project_groups(std::slice::from_ref(&key), 10).unwrap();
  index
    .register_project(crate::RegisteredProjectRecord {
      project_id: "saved".into(),
      group_key: first.to_str().unwrap().into(),
      folder_path: key.clone(),
      name: "Preserved name".into(),
      created_at_ms: 20,
    })
    .unwrap();
  // A present but undecodable marker cannot prove this is now a non-Git folder.
  fs::write(checkout.join(".git"), "gitdir: /missing-administrative-directory").unwrap();
  assert_eq!(
    index.get_registered_project("saved").unwrap().unwrap().group_key,
    first.to_str().unwrap()
  );
  // Stale registration metadata must not override a live non-Git folder.
  fs::remove_file(checkout.join(".git")).unwrap();
  assert_eq!(index.get_registered_project("saved").unwrap().unwrap().group_key, key);
  assert_eq!(
    index.project_groups(std::slice::from_ref(&key), 30).unwrap()[&key].project_key,
    key
  );
  fs::remove_dir_all(old_registration).unwrap();
  worktree(&second, &checkout, "replacement");
  let project = index.get_registered_project("saved").unwrap().unwrap();
  assert_eq!(project.group_key, second.to_str().unwrap());
  assert_eq!(project.project_id, "saved");
  assert_eq!(project.name, "Preserved name");
  assert_eq!(project.created_at_ms, 20);
  let groups = index.project_groups(std::slice::from_ref(&key), 100).unwrap();
  assert_eq!(groups[&key].project_key, second.to_str().unwrap());
  assert_eq!(groups[&key].order_ms, 10);
}

#[test]
fn ordinary_worktrees_folder_is_not_git_registration_metadata() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let project = root.join("project");
  let fake_registration = project.join("worktrees/task");
  let unrelated = root.join("unrelated");
  fs::create_dir_all(project.join(".git")).unwrap();
  fs::create_dir_all(&fake_registration).unwrap();
  fs::write(
    fake_registration.join("gitdir"),
    unrelated.join(".git").to_str().unwrap(),
  )
  .unwrap();
  let keys = [project, unrelated].map(|folder| folder.to_str().unwrap().to_owned());
  let index = SessionIndex::open_in_memory().unwrap();
  let groups = index.project_groups(&keys, 10).unwrap();
  assert_eq!(groups[&keys[1]].project_key, keys[1]);
}

#[cfg(unix)]
#[test]
fn relative_folder_identity_never_uses_the_viewer_working_directory() {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  fs::create_dir(root.join(".git")).unwrap();
  let current = std::env::current_dir().unwrap();
  let mut relative = PathBuf::new();
  for _ in current
    .components()
    .filter(|component| matches!(component, std::path::Component::Normal(_)))
  {
    relative.push("..");
  }
  relative.push(root.strip_prefix("/").unwrap());
  let key = relative.to_str().unwrap().to_owned();
  let index = SessionIndex::open_in_memory().unwrap();
  let groups = index.project_groups(std::slice::from_ref(&key), 10).unwrap();
  assert_eq!(groups[&key].project_key, key);
}
