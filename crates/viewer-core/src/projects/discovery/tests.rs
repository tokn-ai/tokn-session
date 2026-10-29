use super::*;
use crate::workspace::{ProjectDiscoveryState, ProjectOrigin, RegisterProjectRequest};
use tokn_session_index::{SessionKey, SessionMetadata, SourceKey, SourceReplacement, SourceState};

fn index_cwds(service: &ViewerService, provider: ViewerProvider, cwds: &[&str], generation: i64) -> BTreeSet<String> {
  let sessions = cwds
    .iter()
    .enumerate()
    .map(|(index, cwd)| {
      let mut session = SessionMetadata::new(
        SessionKey::new(provider.as_str(), "project-test", index.to_string()),
        "unused-history",
      );
      session.cwd = Some((*cwd).into());
      session
    })
    .collect();
  service
    .session_index
    .replace_source(SourceReplacement::new(
      SourceState::new(
        SourceKey::new(provider.as_str(), "project-test"),
        generation.to_string(),
        generation,
      ),
      sessions,
    ))
    .unwrap();
  service
    .session_index
    .replace_source(SourceReplacement::new(
      SourceState::new(
        SourceKey::new(provider.as_str(), "catalog.v1"),
        "headers.v3",
        generation,
      ),
      Vec::new(),
    ))
    .unwrap();
  cwds.iter().map(|cwd| (*cwd).to_owned()).collect()
}

#[test]
fn warm_index_bootstraps_folders_and_manual_promotion_survives_provider_removal() {
  let directory = tempfile::tempdir().unwrap();
  let folder = directory.path().join("project");
  std::fs::create_dir(&folder).unwrap();
  let cwd = folder.to_str().unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  index_cwds(&service, ViewerProvider::Codex, &[cwd], 1);
  let snapshot = service.list_registered_projects().unwrap();
  let project = &snapshot.projects[0];
  assert_eq!(project.origin, ProjectOrigin::Discovered);
  assert!(project.available);
  let pinned = service
    .register_project(RegisterProjectRequest {
      folder_path: cwd.into(),
      name: Some("Replacement".into()),
    })
    .unwrap();
  assert_eq!(pinned.project_id, project.project_id);
  assert_eq!(pinned.name, project.name);
  assert_eq!(pinned.origin, ProjectOrigin::Manual);
  let empty = index_cwds(&service, ViewerProvider::Codex, &[], 2);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &empty, true)
    .unwrap();
  let project = service.validated_project(&pinned.project_id).unwrap();
  assert!(project.available);
  assert_eq!(project.discovery_state, ProjectDiscoveryState::Absent);
}

#[test]
fn providers_union_and_incomplete_discovery_preserve_previous_eligibility() {
  let directory = tempfile::tempdir().unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let cwd = directory.path().to_str().unwrap();
  for provider in [ViewerProvider::Codex, ViewerProvider::Pi] {
    let cwds = index_cwds(&service, provider, &[cwd], 1);
    service
      .reconcile_discovered_project_folders(provider, &cwds, true)
      .unwrap();
  }
  let project = service.list_registered_projects().unwrap().projects.remove(0);
  let empty = index_cwds(&service, ViewerProvider::Codex, &[], 2);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &empty, true)
    .unwrap();
  service
    .reconcile_discovered_project_folders(ViewerProvider::Pi, &empty, false)
    .unwrap();
  let retained = service.validated_project(&project.project_id).unwrap();
  assert_eq!(retained.discovery_state, ProjectDiscoveryState::Unknown);
  let empty = index_cwds(&service, ViewerProvider::Pi, &[], 2);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Pi, &empty, true)
    .unwrap();
  let absent = service.list_registered_projects().unwrap().projects.remove(0);
  assert!(!absent.available);
  assert_eq!(absent.discovery_state, ProjectDiscoveryState::Absent);
  assert!(
    absent
      .unavailable_reason
      .as_deref()
      .unwrap()
      .contains("No agent sessions")
  );
  assert!(service.validated_project(&project.project_id).is_err());
}

#[test]
fn stale_positive_is_filtered_and_omitted_new_indexed_cwd_cannot_prove_absence() {
  let directory = tempfile::tempdir().unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let cwd = directory.path().to_str().unwrap();
  let cwds = index_cwds(&service, ViewerProvider::Codex, &[cwd], 1);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  // A stale empty scan omitted metadata another process committed before this
  // reconciliation captured its source checkpoint.
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &BTreeSet::new(), true)
    .unwrap();
  let retained = service.list_registered_projects().unwrap().projects.remove(0);
  assert!(retained.available);
  assert_eq!(retained.discovery_state, ProjectDiscoveryState::Unknown);
  index_cwds(&service, ViewerProvider::Codex, &[], 2);
  // Conversely, an older positive observation cannot resurrect tombstoned cwd.
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  let absent = service.list_registered_projects().unwrap().projects.remove(0);
  assert!(!absent.available);
  assert_eq!(absent.discovery_state, ProjectDiscoveryState::Absent);
}

#[cfg(unix)]
#[test]
fn missing_alias_keeps_identity_and_retargeted_alias_gets_a_separate_folder() {
  let directory = tempfile::tempdir().unwrap();
  let first = directory.path().join("first");
  let second = directory.path().join("second");
  let alias = directory.path().join("alias");
  std::fs::create_dir(&first).unwrap();
  std::fs::create_dir(&second).unwrap();
  std::os::unix::fs::symlink(&first, &alias).unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let cwds = index_cwds(&service, ViewerProvider::Codex, &[alias.to_str().unwrap()], 1);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  let original = service.list_registered_projects().unwrap().projects.remove(0);
  std::fs::remove_file(&alias).unwrap();
  std::fs::remove_dir(&first).unwrap();
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  let missing = service.list_registered_projects().unwrap().projects.remove(0);
  assert_eq!(missing.project_id, original.project_id);
  assert_eq!(missing.folder_path, original.folder_path);
  assert!(!missing.available);
  std::os::unix::fs::symlink(&second, &alias).unwrap();
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  let projects = service.list_registered_projects().unwrap().projects;
  assert_eq!(projects.len(), 2);
  let new = projects
    .iter()
    .find(|project| project.project_id != original.project_id)
    .unwrap();
  assert_eq!(new.folder_path, second.canonicalize().unwrap().to_str().unwrap());
  assert!(new.available);
  assert_eq!(
    projects
      .iter()
      .find(|project| project.project_id == original.project_id)
      .unwrap()
      .discovery_state,
    ProjectDiscoveryState::Absent
  );
}

#[test]
fn registry_notifications_only_report_semantic_or_directory_availability_changes() {
  let directory = tempfile::tempdir().unwrap();
  let folder = directory.path().join("project");
  std::fs::create_dir(&folder).unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let mut changes = service.subscribe_project_registry_changes();
  let cwds = index_cwds(&service, ViewerProvider::Codex, &[folder.to_str().unwrap()], 1);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  assert!(changes.try_recv().is_ok());
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  assert!(matches!(changes.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
  std::fs::remove_dir(&folder).unwrap();
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  assert!(changes.try_recv().is_ok());
  assert!(!service.refresh_project_registry_snapshot().unwrap().projects[0].available);
  assert!(matches!(changes.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
}

#[test]
fn restarting_with_known_folders_preserves_presence_without_index_writes() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("index.sqlite");
  let service = ViewerService::native(&path).unwrap();
  let cwd = directory.path().to_str().unwrap();
  let cwds = index_cwds(&service, ViewerProvider::Codex, &[cwd], 1);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  let project = service.list_registered_projects().unwrap().projects.remove(0);
  drop(service);
  let reader = tokn_session_index::SessionIndex::open(&path).unwrap();
  let before = reader.data_version().unwrap();
  let service = ViewerService::native(&path).unwrap();
  assert_eq!(service.list_registered_projects().unwrap().projects, [project]);
  assert_eq!(reader.data_version().unwrap(), before);
}

#[test]
fn unresolved_parent_traversal_is_not_saved_as_an_unstable_folder_identity() {
  let directory = tempfile::tempdir().unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let cwd = directory.path().join("missing/../future");
  let cwds = index_cwds(&service, ViewerProvider::Codex, &[cwd.to_str().unwrap()], 1);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  assert!(service.list_registered_projects().unwrap().projects.is_empty());
}

#[test]
fn follower_poll_does_not_stat_directories_until_registry_metadata_changes() {
  let directory = tempfile::tempdir().unwrap();
  let folder = directory.path().join("project");
  std::fs::create_dir(&folder).unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let cwds = index_cwds(&service, ViewerProvider::Codex, &[folder.to_str().unwrap()], 1);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  let mut changes = service.subscribe_project_registry_changes();
  std::fs::remove_dir(&folder).unwrap();
  index_cwds(&service, ViewerProvider::Codex, &[folder.to_str().unwrap()], 2);
  service.refresh_project_registry_if_changed().unwrap();
  assert!(matches!(changes.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
  // Explicit reads still check filesystem availability, independently of the
  // cheap source-change follower poll.
  assert!(!service.list_registered_projects().unwrap().projects[0].available);
  assert!(changes.try_recv().is_ok());
}

#[tokio::test]
async fn external_fallback_catalog_skips_registry_mutation_without_failing() {
  let directory = tempfile::tempdir().unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let cwds = index_cwds(
    &service,
    ViewerProvider::Codex,
    &[directory.path().to_str().unwrap()],
    1,
  );
  service
    .relay
    .configure(crate::relay::RelaySettings {
      mode: RelayMode::External,
      ..Default::default()
    })
    .unwrap();
  assert!(
    !service
      .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
      .unwrap()
  );
  assert!(service.session_index.list_project_folders().unwrap().is_empty());
  assert!(!service.list_registered_projects().unwrap().available);
}

#[test]
fn legacy_cached_cwd_cannot_register_launch_folders_before_native_verification() {
  let directory = tempfile::tempdir().unwrap();
  let stale = directory.path().join("Project X");
  let actual = directory.path().join("Project  X");
  std::fs::create_dir(&stale).unwrap();
  std::fs::create_dir(&actual).unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  index_cwds(&service, ViewerProvider::Codex, &[stale.to_str().unwrap()], 1);
  service
    .session_index
    .replace_source(SourceReplacement::new(
      SourceState::new(SourceKey::new("codex", "catalog.v1"), "headers.v2", 1),
      Vec::new(),
    ))
    .unwrap();
  let manual = service
    .register_project(RegisterProjectRequest {
      folder_path: directory.path().to_str().unwrap().into(),
      name: None,
    })
    .unwrap();
  assert_eq!(service.list_registered_projects().unwrap().projects, [manual.clone()]);
  assert_eq!(service.list_registered_projects().unwrap().projects, [manual]);
  // A native reread restores the exact cwd and then publishes the new marker.
  // The earlier skipped bootstrap must remain eligible for a later retry.
  index_cwds(&service, ViewerProvider::Codex, &[actual.to_str().unwrap()], 2);
  let projects = service.list_registered_projects().unwrap().projects;
  assert_eq!(projects.len(), 2);
  let discovered = projects
    .iter()
    .find(|project| project.origin == ProjectOrigin::Discovered)
    .unwrap();
  assert_eq!(discovered.folder_path, actual.canonicalize().unwrap().to_str().unwrap());
  assert!(discovered.available);
  assert!(projects.iter().all(|project| Path::new(&project.folder_path) != stale));
}

#[test]
fn malformed_indexed_cwd_never_proves_previous_folder_absence() {
  let directory = tempfile::tempdir().unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let cwds = index_cwds(
    &service,
    ViewerProvider::Codex,
    &[directory.path().to_str().unwrap()],
    1,
  );
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &cwds, true)
    .unwrap();
  let saved = service.list_registered_projects().unwrap().projects.remove(0);
  let invalid = index_cwds(&service, ViewerProvider::Codex, &["relative-folder"], 2);
  service
    .reconcile_discovered_project_folders(ViewerProvider::Codex, &invalid, true)
    .unwrap();
  let retained = service.validated_project(&saved.project_id).unwrap();
  assert_eq!(retained.discovery_state, ProjectDiscoveryState::Unknown);
  assert!(retained.available);
  assert_eq!(service.list_registered_projects().unwrap().projects.len(), 1);
}

#[test]
fn native_cwd_validation_preserves_spaces_and_rejects_lossy_replacements() {
  assert!(valid_discovered_cwd("/work/Project  X "));
  for invalid in ["", "relative", "/work/line\nbreak", "/work/escape\u{001b}[0m"] {
    assert!(!valid_discovered_cwd(invalid));
  }
  assert!(!valid_discovered_cwd(&format!("/{}", "界".repeat(1366))));
}

#[test]
fn restricted_observer_skips_registry_without_breaking_session_work() {
  let directory = tempfile::tempdir().unwrap();
  let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
  let key = crate::model::encode_session_key(&crate::model::SessionLocator {
    version: 1,
    provider: ViewerProvider::Codex,
    session_id: "shared".into(),
    source_path: directory.path().join("shared.jsonl"),
  })
  .unwrap();
  let shared = service.scoped_to_sessions(&[key]).unwrap();
  shared.refresh_project_registry_if_changed().unwrap();
  assert!(service.project_registry.snapshot.lock().unwrap().is_none());
  assert!(shared.list_registered_projects().is_err());
}
