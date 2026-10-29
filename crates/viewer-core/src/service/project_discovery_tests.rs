use super::*;
use crate::workspace::{ProjectDiscoveryState, ProjectOrigin, RegisteredProject};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct CatalogRepository {
  catalogs: Mutex<HashMap<ViewerProvider, Result<SessionHeaderCatalog, String>>>,
  roots: Mutex<HashMap<ViewerProvider, Vec<PathBuf>>>,
  catalog_reads: AtomicUsize,
  body_reads: AtomicUsize,
}

impl CatalogRepository {
  fn set(&self, provider: ViewerProvider, headers: Vec<SessionHeader>, complete: bool) {
    self
      .catalogs
      .lock()
      .unwrap()
      .insert(provider, Ok(SessionHeaderCatalog { headers, complete }));
  }
}

impl ViewerRepository for CatalogRepository {
  fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
    self
      .list_session_header_catalog(provider)
      .map(|catalog| catalog.headers)
  }

  fn list_session_header_catalog(&self, provider: ViewerProvider) -> Result<SessionHeaderCatalog, String> {
    self.catalog_reads.fetch_add(1, Ordering::SeqCst);
    self
      .catalogs
      .lock()
      .unwrap()
      .get(&provider)
      .cloned()
      .unwrap_or_else(|| {
        Ok(SessionHeaderCatalog {
          headers: Vec::new(),
          complete: true,
        })
      })
  }

  fn session_catalog_roots(&self, provider: ViewerProvider) -> Result<Option<Vec<PathBuf>>, String> {
    Ok(self.roots.lock().unwrap().get(&provider).cloned())
  }

  fn session_header_at_path(&self, provider: ViewerProvider, path: &Path) -> Result<SessionHeader, String> {
    self
      .catalogs
      .lock()
      .unwrap()
      .get(&provider)
      .and_then(|catalog| catalog.as_ref().ok())
      .and_then(|catalog| catalog.headers.iter().find(|header| header.path == path))
      .cloned()
      .ok_or_else(|| "The requested source is not readable".into())
  }

  fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
    self.body_reads.fetch_add(1, Ordering::SeqCst);
    Err("Folder discovery must not read message bodies".into())
  }
}

fn folder(root: &Path, name: &str) -> PathBuf {
  let path = root.join(name);
  std::fs::create_dir_all(&path).unwrap();
  std::fs::canonicalize(path).unwrap()
}

fn header(root: &Path, source_name: &str, id: &str, cwd: &Path) -> SessionHeader {
  let path = root.join(source_name);
  if !path.exists() {
    std::fs::write(&path, "source revision").unwrap();
  }
  SessionHeader {
    id: id.into(),
    parent_session_id: None,
    agent_path: None,
    agent_nickname: None,
    agent_role: None,
    title: Some(id.into()),
    preview: None,
    path,
    cwd: Some(cwd.to_str().unwrap().into()),
    timestamp: None,
    updated_at: None,
    updated_at_ms: None,
  }
}

fn saved_folder(service: &ViewerService, cwd: &Path) -> RegisteredProject {
  service
    .list_registered_projects()
    .unwrap()
    .projects
    .into_iter()
    .find(|project| Path::new(&project.folder_path) == cwd)
    .expect("discovered folder should have a stable registered identity")
}

fn present_sessions(service: &ViewerService, provider: ViewerProvider) -> BTreeSet<String> {
  service
    .session_index
    .list_all_sessions_for_provider(provider.as_str())
    .unwrap()
    .into_iter()
    .filter(|session| session.present)
    .map(|session| session.key.session_id)
    .collect()
}

#[test]
fn all_provider_catalogs_register_existing_launch_folders_without_body_reads() {
  let root = tempfile::tempdir().unwrap();
  let repository = Arc::new(CatalogRepository::default());
  let service = ViewerService::new(repository.clone());
  assert!(service.list_registered_projects().unwrap().projects.is_empty());

  for provider in ViewerProvider::ALL {
    let cwd = folder(root.path(), provider.as_str());
    let session = header(
      root.path(),
      &format!("{}.jsonl", provider.as_str()),
      provider.as_str(),
      &cwd,
    );
    repository.set(provider, vec![session], true);
    service.refresh_provider_catalog(provider).unwrap();
    let project = saved_folder(&service, &cwd);
    assert!(project.available, "{provider:?} should expose a usable launch folder");
    assert_eq!(project.origin, ProjectOrigin::Discovered);
    assert_eq!(project.discovery_state, ProjectDiscoveryState::Present);
    assert!(project.unavailable_reason.is_none());
  }
  assert_eq!(
    service.list_registered_projects().unwrap().projects.len(),
    ViewerProvider::ALL.len()
  );
  assert_eq!(repository.body_reads.load(Ordering::SeqCst), 0);
}

#[test]
fn one_remaining_provider_keeps_a_folder_available_until_all_histories_are_removed() {
  let root = tempfile::tempdir().unwrap();
  let cwd = folder(root.path(), "project");
  let repository = Arc::new(CatalogRepository::default());
  let service = ViewerService::new(repository.clone());
  for provider in [ViewerProvider::Codex, ViewerProvider::Pi] {
    repository.set(
      provider,
      vec![header(
        root.path(),
        &format!("{}.jsonl", provider.as_str()),
        "session",
        &cwd,
      )],
      true,
    );
    service.refresh_provider_catalog(provider).unwrap();
  }
  let initial = saved_folder(&service, &cwd);
  assert_eq!(service.list_registered_projects().unwrap().projects.len(), 1);

  repository.set(ViewerProvider::Codex, Vec::new(), true);
  service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
  let still_used = saved_folder(&service, &cwd);
  assert_eq!(still_used.project_id, initial.project_id);
  assert!(still_used.available);
  assert_eq!(still_used.discovery_state, ProjectDiscoveryState::Present);

  repository.set(ViewerProvider::Pi, Vec::new(), true);
  service.refresh_provider_catalog(ViewerProvider::Pi).unwrap();
  let removed = saved_folder(&service, &cwd);
  assert_eq!(removed.project_id, initial.project_id);
  assert!(!removed.available);
  assert_eq!(removed.discovery_state, ProjectDiscoveryState::Absent);
  assert!(removed.unavailable_reason.is_some());
}

#[test]
fn incomplete_file_catalog_preserves_omitted_history_and_folder_until_confirmed_removed() {
  let root = tempfile::tempdir().unwrap();
  let kept = folder(root.path(), "kept");
  let omitted = folder(root.path(), "omitted");
  let repository = Arc::new(CatalogRepository::default());
  let service = ViewerService::new(repository.clone());
  let kept_header = header(root.path(), "kept.jsonl", "kept", &kept);
  let omitted_header = header(root.path(), "omitted.jsonl", "omitted", &omitted);
  repository.set(ViewerProvider::Codex, vec![kept_header.clone(), omitted_header], true);
  service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
  let original_id = saved_folder(&service, &omitted).project_id;

  repository.set(ViewerProvider::Codex, vec![kept_header.clone()], false);
  service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
  assert_eq!(
    present_sessions(&service, ViewerProvider::Codex),
    BTreeSet::from(["kept".into(), "omitted".into()])
  );
  let uncertain = saved_folder(&service, &omitted);
  assert_eq!(uncertain.project_id, original_id);
  assert!(uncertain.available);
  assert_eq!(uncertain.discovery_state, ProjectDiscoveryState::Unknown);

  repository.set(ViewerProvider::Codex, vec![kept_header], true);
  service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
  assert_eq!(
    present_sessions(&service, ViewerProvider::Codex),
    BTreeSet::from(["kept".into()])
  );
  let removed = saved_folder(&service, &omitted);
  assert_eq!(removed.project_id, original_id);
  assert!(!removed.available);
  assert_eq!(removed.discovery_state, ProjectDiscoveryState::Absent);
}

#[test]
fn failed_catalog_preserves_the_last_confirmed_folder_membership() {
  let root = tempfile::tempdir().unwrap();
  let cwd = folder(root.path(), "project");
  let repository = Arc::new(CatalogRepository::default());
  let service = ViewerService::new(repository.clone());
  repository.set(
    ViewerProvider::Pi,
    vec![header(root.path(), "pi.jsonl", "saved", &cwd)],
    true,
  );
  service.refresh_provider_catalog(ViewerProvider::Pi).unwrap();
  repository.catalogs.lock().unwrap().insert(
    ViewerProvider::Pi,
    Err("History root is temporarily unavailable".into()),
  );

  assert!(service.refresh_provider_catalog(ViewerProvider::Pi).is_err());
  let retained = saved_folder(&service, &cwd);
  assert!(retained.available);
  assert_eq!(retained.discovery_state, ProjectDiscoveryState::Unknown);
  assert_eq!(
    present_sessions(&service, ViewerProvider::Pi),
    BTreeSet::from(["saved".into()])
  );
}

#[test]
fn repeated_headers_without_cwd_preserve_the_last_known_session_folder() {
  let root = tempfile::tempdir().unwrap();
  let cwd = folder(root.path(), "project");
  let repository = Arc::new(CatalogRepository::default());
  let service = ViewerService::new(repository.clone());
  let mut session = header(root.path(), "saved.jsonl", "saved", &cwd);
  repository.set(ViewerProvider::Codex, vec![session.clone()], true);
  service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
  let original = saved_folder(&service, &cwd);

  session.cwd = None;
  repository.set(ViewerProvider::Codex, vec![session.clone()], true);
  for revision in 1..=3 {
    std::fs::write(&session.path, "incomplete rewritten header\n".repeat(revision)).unwrap();
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    let retained = saved_folder(&service, &cwd);
    assert_eq!(retained.project_id, original.project_id);
    assert!(retained.available);
    let indexed = service.session_index.list_all_sessions_for_provider("codex").unwrap();
    assert_eq!(indexed.len(), 1);
    assert!(indexed[0].present);
    assert_eq!(indexed[0].cwd.as_deref(), cwd.to_str());
  }
  assert_eq!(repository.body_reads.load(Ordering::SeqCst), 0);
}

#[test]
fn incomplete_shared_database_inventory_does_not_delete_older_session_rows() {
  let root = tempfile::tempdir().unwrap();
  let kept = folder(root.path(), "kept");
  let omitted = folder(root.path(), "omitted");
  let repository = Arc::new(CatalogRepository::default());
  let service = ViewerService::new(repository.clone());
  let kept_header = header(root.path(), "opencode.db", "kept", &kept);
  let omitted_header = header(root.path(), "opencode.db", "omitted", &omitted);
  repository.set(
    ViewerProvider::OpenCode,
    vec![kept_header.clone(), omitted_header],
    true,
  );
  service.refresh_provider_catalog(ViewerProvider::OpenCode).unwrap();

  std::fs::write(&kept_header.path, "new database revision with an unreadable row").unwrap();
  repository.set(ViewerProvider::OpenCode, vec![kept_header.clone()], false);
  service.refresh_provider_catalog(ViewerProvider::OpenCode).unwrap();
  assert_eq!(
    present_sessions(&service, ViewerProvider::OpenCode),
    BTreeSet::from(["kept".into(), "omitted".into()])
  );
  assert!(saved_folder(&service, &kept).available);
  let retained = saved_folder(&service, &omitted);
  assert!(retained.available);
  assert_eq!(retained.discovery_state, ProjectDiscoveryState::Unknown);

  repository.set(ViewerProvider::OpenCode, vec![kept_header], true);
  service.refresh_provider_catalog(ViewerProvider::OpenCode).unwrap();
  assert_eq!(
    present_sessions(&service, ViewerProvider::OpenCode),
    BTreeSet::from(["kept".into()])
  );
  assert!(!saved_folder(&service, &omitted).available);
}

#[test]
fn missing_or_changed_history_scope_does_not_prove_removal_but_restored_empty_scope_does() {
  for remove_old_root in [false, true] {
    let root = tempfile::tempdir().unwrap();
    let cwd = folder(root.path(), "project");
    let history = folder(root.path(), "archived-sessions");
    let other_history = folder(root.path(), "sessions");
    let repository = Arc::new(CatalogRepository::default());
    let service = ViewerService::new(repository.clone());
    let archived = header(&history, "saved.jsonl", "saved", &cwd);
    repository
      .roots
      .lock()
      .unwrap()
      .insert(ViewerProvider::Codex, vec![other_history.clone(), history.clone()]);
    repository.set(ViewerProvider::Codex, vec![archived.clone()], true);
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    let original = saved_folder(&service, &cwd);

    if remove_old_root {
      std::fs::remove_dir_all(&history).unwrap();
    } else {
      repository
        .roots
        .lock()
        .unwrap()
        .insert(ViewerProvider::Codex, vec![other_history]);
    }
    repository.set(ViewerProvider::Codex, Vec::new(), true);
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    assert_eq!(
      present_sessions(&service, ViewerProvider::Codex),
      BTreeSet::from(["saved".into()])
    );
    let retained = saved_folder(&service, &cwd);
    assert_eq!(retained.project_id, original.project_id);
    assert!(retained.available);
    assert_eq!(retained.discovery_state, ProjectDiscoveryState::Unknown);

    std::fs::create_dir_all(&history).unwrap();
    if archived.path.exists() {
      std::fs::remove_file(&archived.path).unwrap();
    }
    repository
      .roots
      .lock()
      .unwrap()
      .insert(ViewerProvider::Codex, vec![history]);
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    assert!(present_sessions(&service, ViewerProvider::Codex).is_empty());
    let removed = saved_folder(&service, &cwd);
    assert_eq!(removed.project_id, original.project_id);
    assert!(!removed.available);
    assert_eq!(removed.discovery_state, ProjectDiscoveryState::Absent);
  }
}

#[test]
fn only_a_changed_targeted_cwd_requests_folder_reconciliation() {
  for provider in [ViewerProvider::Codex, ViewerProvider::Pi] {
    let root = tempfile::tempdir().unwrap();
    let original = folder(root.path(), "original");
    let next = folder(root.path(), "next");
    let repository = Arc::new(CatalogRepository::default());
    let service = ViewerService::new(repository.clone());
    let mut active = header(root.path(), "active.jsonl", "active", &original);
    repository.set(provider, vec![active.clone()], true);
    service.refresh_provider_catalog(provider).unwrap();
    let catalog_reads = repository.catalog_reads.load(Ordering::SeqCst);
    let paths = BTreeSet::from([active.path.clone()]);

    std::fs::write(&active.path, "ordinary append with the same working folder").unwrap();
    let unchanged_folder = service.refresh_changed_file_provider_catalog(provider, &paths).unwrap();
    assert!(unchanged_folder.changed);
    assert!(!unchanged_folder.retry_catalog_soon);
    assert_eq!(repository.catalog_reads.load(Ordering::SeqCst), catalog_reads);

    active.cwd = Some(next.to_str().unwrap().into());
    repository.set(provider, vec![active.clone()], true);
    std::fs::write(&active.path, "a different working folder").unwrap();
    let changed_folder = service.refresh_changed_file_provider_catalog(provider, &paths).unwrap();
    assert!(changed_folder.changed);
    assert!(changed_folder.retry_catalog_soon);
    assert_eq!(repository.catalog_reads.load(Ordering::SeqCst), catalog_reads);
    service.refresh_provider_catalog(provider).unwrap();
    assert!(saved_folder(&service, &next).available);
    assert!(!saved_folder(&service, &original).available);
  }
}

#[test]
fn legacy_cached_paths_are_verified_before_registration_and_preserve_spaces() {
  let root = tempfile::tempdir().unwrap();
  let cwd = folder(root.path(), "Project  two");
  let wrong_cwd = folder(root.path(), "Project two");
  let repository = Arc::new(CatalogRepository::default());
  let session = header(root.path(), "legacy.jsonl", "legacy", &cwd);
  let provider = ViewerProvider::Codex;
  let locator = locator_for_header(provider, &session);
  let key = index_session_key(&locator).unwrap();
  let index = Arc::new(SessionIndex::open_in_memory().unwrap());
  let mut cached = SessionMetadata::new(key.clone(), session.path.to_str().unwrap());
  cached.cwd = Some(wrong_cwd.to_str().unwrap().into());
  index
    .replace_sources(&[
      SourceReplacement::new(
        SourceState::new(index_catalog_source_key(provider), "headers.v2", 0),
        vec![],
      ),
      SourceReplacement::new(
        SourceState::new(
          key.source_key(),
          completed_body_cursor(&source_cursor(provider, &session.path).unwrap()),
          0,
        ),
        vec![cached],
      ),
    ])
    .unwrap();
  repository.set(provider, vec![session], true);
  let service = ViewerService::new_with_index(repository.clone(), index);
  assert!(service.list_registered_projects().unwrap().projects.is_empty());
  service.refresh_provider_catalog(provider).unwrap();
  let projects = service.list_registered_projects().unwrap().projects;
  assert_eq!(projects.len(), 1);
  assert_eq!(projects[0].folder_path, cwd.to_str().unwrap());
  assert!(projects[0].available);
  assert_eq!(repository.body_reads.load(Ordering::SeqCst), 0);
}

#[test]
fn invalid_cwd_never_replaces_or_retires_a_known_launch_folder() {
  let root = tempfile::tempdir().unwrap();
  let cwd = folder(root.path(), "known");
  let repository = Arc::new(CatalogRepository::default());
  let service = ViewerService::new(repository.clone());
  let mut session = header(root.path(), "invalid-cwd.jsonl", "known", &cwd);
  repository.set(ViewerProvider::Codex, vec![session.clone()], true);
  service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
  let id = saved_folder(&service, &cwd).project_id;
  for invalid in ["relative/path", "/folder\nname"] {
    session.cwd = Some(invalid.into());
    std::fs::write(&session.path, invalid).unwrap();
    repository.set(ViewerProvider::Codex, vec![session.clone()], true);
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    let project = saved_folder(&service, &cwd);
    assert_eq!(project.project_id, id);
    assert!(project.available);
    assert_eq!(project.discovery_state, ProjectDiscoveryState::Unknown);
  }
}

#[test]
fn folder_group_keys_preserve_trailing_spaces() {
  let root = tempfile::tempdir().unwrap();
  let cwd = folder(root.path(), "Project ");
  let other = folder(root.path(), "Project");
  let repository = Arc::new(CatalogRepository::default());
  let service = ViewerService::new(repository.clone());
  let session = header(root.path(), "spaces.jsonl", "spaces", &cwd);
  assert_eq!(project_key(&session), cwd.to_str());
  repository.set(ViewerProvider::Pi, vec![session], true);
  service.refresh_provider_catalog(ViewerProvider::Pi).unwrap();
  let projects = service.list_registered_projects().unwrap().projects;
  assert_eq!(projects.len(), 1);
  assert_eq!(projects[0].folder_path, cwd.to_str().unwrap());
  assert_eq!(projects[0].group_key, cwd.to_str().unwrap());
  assert_ne!(projects[0].group_key, other.to_str().unwrap());
  let order = service.session_index.project_order(&[], 0).unwrap();
  assert!(order.contains_key(cwd.to_str().unwrap()));
  assert!(!order.contains_key(other.to_str().unwrap()));
}
