//! Explicit local launch folders, separate from inferred session grouping.
use std::{
  path::{Path, PathBuf},
  time::{SystemTime, UNIX_EPOCH},
};

use tokn_session_index::RegisteredProjectRecord;

use crate::{
  ViewerService,
  relay::RelayMode,
  workspace::{ProjectWorkspaceSnapshot, RegisterProjectRequest, RegisteredProject},
};

const MAX_FOLDER_PATH_BYTES: usize = 4096;
const MAX_PROJECT_NAME_CHARS: usize = 160;
const EXTERNAL_REGISTRY_REASON: &str =
  "Projects belong to this computer's local reader. Switch from External reader mode to manage them.";

impl ViewerService {
  /// Lists saved launch folders independently of provider discovery and
  /// pagination, including projects that have no sessions yet.
  pub fn list_registered_projects(&self) -> Result<ProjectWorkspaceSnapshot, String> {
    self.check_project_share_scope()?;
    if self.relay.status().settings.mode == RelayMode::External {
      return Ok(ProjectWorkspaceSnapshot {
        available: false,
        unavailable_reason: Some(EXTERNAL_REGISTRY_REASON.into()),
        projects: Vec::new(),
      });
    }
    let projects = self
      .session_index
      .list_registered_projects()
      .map_err(|error| format!("Could not list registered projects: {error}"))?
      .into_iter()
      .map(project_snapshot)
      .collect();
    Ok(ProjectWorkspaceSnapshot {
      available: true,
      unavailable_reason: None,
      projects,
    })
  }

  /// Registers an existing directory. Symlink aliases resolve to one stable
  /// registration, while separate worktrees retain their own launch folders.
  pub fn register_project(&self, request: RegisterProjectRequest) -> Result<RegisteredProject, String> {
    self.check_local_project_registry()?;
    let folder_path = canonical_folder(&request.folder_path)?;
    let name = project_name(request.name.as_deref(), Path::new(&folder_path))?;
    let created_at_ms = current_time_ms();
    let known_groups = self
      .session_index
      .project_order(&[], created_at_ms)
      .map_err(|error| format!("Could not read existing project groups: {error}"))?;
    // Preserve a previously discovered non-Git folder's original cwd spelling
    // when registration first gives it a canonical launch path. Existing
    // canonical identities and repository/worktree aliases remain preferred.
    let original_folder = Path::new(&request.folder_path).components().collect::<PathBuf>();
    let original_folder = original_folder.to_str().unwrap_or(&folder_path);
    let group_folder = if !known_groups.contains_key(&folder_path) && known_groups.contains_key(original_folder) {
      original_folder.to_owned()
    } else {
      folder_path.clone()
    };
    let groups = self
      .session_index
      .project_groups(std::slice::from_ref(&group_folder), created_at_ms)
      .map_err(|error| format!("Could not resolve the project's repository group: {error}"))?;
    let group_key = groups
      .get(&group_folder)
      .ok_or("Could not resolve the project's repository group")?
      .project_key
      .clone();
    let project = self
      .session_index
      .register_project(RegisteredProjectRecord {
        project_id: uuid::Uuid::new_v4().to_string(),
        group_key,
        folder_path,
        name,
        created_at_ms,
      })
      .map_err(|error| format!("Could not register the project: {error}"))?;
    Ok(project_snapshot(project))
  }

  /// Resolves a registered identity for a new local session and revalidates
  /// its exact launch directory. A missing folder or redirected symlink must
  /// never silently launch in a different working directory.
  pub fn validated_project(&self, project_id: &str) -> Result<RegisteredProject, String> {
    self.check_local_project_registry()?;
    if project_id.is_empty() || project_id.len() > 128 || project_id.chars().any(char::is_control) {
      return Err("Invalid registered project identity".into());
    }
    let record = self
      .session_index
      .get_registered_project(project_id)
      .map_err(|error| format!("Could not read the registered project: {error}"))?
      .ok_or("The registered project was not found")?;
    let project = project_snapshot(record);
    if !project.available {
      return Err("The registered project folder is unavailable or now resolves to a different location".into());
    }
    Ok(project)
  }

  fn check_project_share_scope(&self) -> Result<(), String> {
    if self.session_scope.is_some() {
      return Err("Project registration is unavailable in a session share".into());
    }
    Ok(())
  }

  fn check_local_project_registry(&self) -> Result<(), String> {
    self.check_project_share_scope()?;
    if self.relay.status().settings.mode == RelayMode::External {
      return Err(EXTERNAL_REGISTRY_REASON.into());
    }
    Ok(())
  }
}

fn canonical_folder(folder_path: &str) -> Result<String, String> {
  if folder_path.trim().is_empty()
    || folder_path.len() > MAX_FOLDER_PATH_BYTES
    || folder_path.chars().any(char::is_control)
  {
    return Err("Folder path is empty, too long, or contains control characters".into());
  }
  let folder = Path::new(folder_path);
  if !folder.is_absolute() {
    return Err("Choose an absolute folder path".into());
  }
  let canonical = folder
    .canonicalize()
    .map_err(|error| format!("Could not open the project folder: {error}"))?;
  if !canonical.is_dir() {
    return Err("The project path must be an existing directory".into());
  }
  let canonical = canonical.to_str().ok_or("The project folder path is not valid UTF-8")?;
  if canonical.len() > MAX_FOLDER_PATH_BYTES || canonical.chars().any(char::is_control) {
    return Err("The resolved project folder path is too long or contains control characters".into());
  }
  Ok(canonical.to_owned())
}

fn project_name(name: Option<&str>, folder: &Path) -> Result<String, String> {
  let name = name.map(str::trim).filter(|name| !name.is_empty());
  let name = name.unwrap_or_else(|| folder.file_name().and_then(|name| name.to_str()).unwrap_or("Project"));
  if name.chars().count() > MAX_PROJECT_NAME_CHARS || name.chars().any(char::is_control) {
    return Err("Project name must contain at most 160 characters without control characters".into());
  }
  Ok(name.to_owned())
}

fn folder_is_available(folder_path: &str) -> bool {
  canonical_folder(folder_path).is_ok_and(|canonical| PathBuf::from(canonical) == Path::new(folder_path))
}

fn project_snapshot(record: RegisteredProjectRecord) -> RegisteredProject {
  let available = folder_is_available(&record.folder_path);
  RegisteredProject {
    project_id: record.project_id,
    group_key: record.group_key,
    folder_path: record.folder_path,
    name: record.name,
    created_at_ms: record.created_at_ms,
    available,
  }
}

fn current_time_ms() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis()
    .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::model::{SessionLocator, ViewerProvider, encode_session_key};

  fn register(service: &ViewerService, folder: &Path) -> RegisteredProject {
    service
      .register_project(RegisterProjectRequest {
        folder_path: folder.to_str().unwrap().into(),
        name: None,
      })
      .unwrap()
  }

  #[test]
  fn empty_projects_persist_and_missing_folders_cannot_launch() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("new-project");
    std::fs::create_dir(&folder).unwrap();
    let database = directory.path().join("index.sqlite");
    let service = ViewerService::native(&database).unwrap();
    let before = service.session_index_progress();
    let project = register(&service, &folder);
    assert!(project.available);
    assert_eq!(project.name, "new-project");
    assert_eq!(project.folder_path, folder.canonicalize().unwrap().to_str().unwrap());
    let duplicate = service
      .register_project(RegisterProjectRequest {
        folder_path: project.folder_path.clone(),
        name: Some("A replacement name".into()),
      })
      .unwrap();
    assert_eq!(duplicate.project_id, project.project_id);
    assert_eq!(duplicate.name, project.name);
    assert!(service.session_index.list_present_sessions().unwrap().is_empty());
    assert_eq!(service.session_index_progress().revision, before.revision);
    drop(service);
    std::fs::remove_dir(&folder).unwrap();
    let reopened = ViewerService::native(database).unwrap();
    let projects = reopened.list_registered_projects().unwrap();
    assert!(projects.available);
    assert_eq!(projects.projects.len(), 1);
    assert_eq!(projects.projects[0].project_id, project.project_id);
    assert!(!projects.projects[0].available);
    assert!(reopened.validated_project(&project.project_id).is_err());
    assert!(reopened.validated_project("missing").is_err());
  }

  #[test]
  fn registration_rejects_invalid_paths_and_names_before_storing_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let file = directory.path().join("file");
    std::fs::write(&file, "not a directory").unwrap();
    for folder_path in [
      "".into(),
      "relative".into(),
      file.to_str().unwrap().into(),
      "x".repeat(4097),
    ] {
      assert!(
        service
          .register_project(RegisterProjectRequest {
            folder_path,
            name: None
          })
          .is_err()
      );
    }
    for name in ["a".repeat(161), "line\nbreak".into()] {
      assert!(
        service
          .register_project(RegisterProjectRequest {
            folder_path: directory.path().to_str().unwrap().into(),
            name: Some(name),
          })
          .is_err()
      );
    }
    assert!(service.list_registered_projects().unwrap().projects.is_empty());
    assert!(service.session_index.project_order(&[], 0).unwrap().is_empty());
  }

  #[cfg(unix)]
  #[test]
  fn aliases_deduplicate_but_redirected_saved_paths_are_unavailable() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    let alias = directory.path().join("alias");
    let other = directory.path().join("other");
    std::fs::create_dir(&folder).unwrap();
    std::fs::create_dir(&other).unwrap();
    std::os::unix::fs::symlink(&folder, &alias).unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let first = register(&service, &folder);
    assert_eq!(register(&service, &alias).project_id, first.project_id);
    std::fs::remove_dir(&folder).unwrap();
    std::os::unix::fs::symlink(&other, &folder).unwrap();
    assert!(!service.list_registered_projects().unwrap().projects[0].available);
    assert!(service.validated_project(&first.project_id).is_err());
  }

  #[cfg(unix)]
  #[test]
  fn registering_a_known_non_git_alias_keeps_its_inferred_group_and_anchor() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    let alias = directory.path().join("alias");
    std::fs::create_dir(&folder).unwrap();
    std::os::unix::fs::symlink(&folder, &alias).unwrap();
    let original_group = alias.to_str().unwrap().to_owned();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    service
      .session_index
      .project_order(std::slice::from_ref(&original_group), 10)
      .unwrap();
    let project = register(&service, &alias);
    assert_eq!(project.group_key, original_group);
    assert_eq!(project.folder_path, folder.canonicalize().unwrap().to_str().unwrap());
    let groups = service
      .session_index
      .project_groups(std::slice::from_ref(&project.folder_path), 20)
      .unwrap();
    assert_eq!(groups[&project.folder_path].project_key, original_group);
    assert_eq!(groups[&project.folder_path].order_ms, 10);
  }

  #[test]
  fn worktrees_keep_distinct_launch_folders_and_existing_group_anchors() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repository");
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    for (folder, label) in [(&first, "first"), (&second, "second")] {
      let registration = repository.join(".git/worktrees").join(label);
      std::fs::create_dir_all(&registration).unwrap();
      std::fs::create_dir(folder).unwrap();
      std::fs::write(folder.join(".git"), format!("gitdir: {}", registration.display())).unwrap();
      std::fs::write(registration.join("commondir"), "../..").unwrap();
      std::fs::write(registration.join("gitdir"), folder.join(".git").to_str().unwrap()).unwrap();
    }
    let root = repository.canonicalize().unwrap().to_str().unwrap().to_owned();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    service
      .session_index
      .project_order(std::slice::from_ref(&root), 10)
      .unwrap();
    let first = register(&service, &first);
    let second = register(&service, &second);
    assert_ne!(first.project_id, second.project_id);
    assert_ne!(first.folder_path, second.folder_path);
    assert_eq!(first.group_key, root);
    assert_eq!(second.group_key, root);
    let groups = service
      .session_index
      .project_groups(&[first.folder_path.clone(), second.folder_path.clone()], 20)
      .unwrap();
    assert_eq!(groups[&first.folder_path].order_ms, 10);
    assert_eq!(groups[&second.folder_path].order_ms, 10);
    assert_eq!(
      service.validated_project(&second.project_id).unwrap().folder_path,
      second.folder_path
    );
  }

  #[test]
  fn project_registry_is_not_a_session_share_capability() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let project = register(&service, directory.path());
    let key = encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "shared".into(),
      source_path: directory.path().join("shared.jsonl"),
    })
    .unwrap();
    let shared = service.scoped_to_sessions(&[key]).unwrap();
    assert!(shared.list_registered_projects().is_err());
    assert!(
      shared
        .register_project(RegisterProjectRequest {
          folder_path: directory.path().to_str().unwrap().into(),
          name: None,
        })
        .is_err()
    );
    assert!(shared.validated_project(&project.project_id).is_err());
  }

  #[tokio::test]
  async fn external_reader_never_exposes_or_mutates_this_computers_registry() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let project = register(&service, directory.path());
    service
      .relay
      .configure(crate::relay::RelaySettings {
        mode: RelayMode::External,
        ..Default::default()
      })
      .unwrap();
    let snapshot = service.list_registered_projects().unwrap();
    assert!(!snapshot.available);
    assert!(snapshot.unavailable_reason.is_some());
    assert!(snapshot.projects.is_empty());
    assert!(service.validated_project(&project.project_id).is_err());
    assert!(
      service
        .register_project(RegisterProjectRequest {
          folder_path: directory.path().to_str().unwrap().into(),
          name: None,
        })
        .is_err()
    );
  }
}
