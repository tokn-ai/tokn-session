use super::*;
use std::sync::atomic::AtomicBool;

struct ColdBodyRepository {
  inner: Arc<IndexingRepository>,
  catalog_only: AtomicBool,
  deferred: AtomicBool,
}

impl ViewerRepository for ColdBodyRepository {
  fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
    self.inner.list_session_headers(provider)
  }

  fn session_body_indexing(&self, _locator: &SessionLocator) -> Result<SessionBodyIndexing, String> {
    Ok(if self.deferred.load(Ordering::SeqCst) {
      SessionBodyIndexing::Deferred
    } else if self.catalog_only.load(Ordering::SeqCst) {
      SessionBodyIndexing::CatalogOnly
    } else {
      SessionBodyIndexing::Ready
    })
  }

  fn load_session(&self, locator: &SessionLocator) -> Result<LoadedSession, String> {
    self.inner.load_session(locator)
  }
}

fn cold_repository(
  provider: ViewerProvider,
  header: SessionHeader,
  messages: Vec<IndexedMessageSpec>,
) -> Arc<ColdBodyRepository> {
  Arc::new(ColdBodyRepository {
    inner: indexing_repository_for(provider, vec![IndexedLoadSpec { header, messages }]),
    catalog_only: AtomicBool::new(true),
    deferred: AtomicBool::new(false),
  })
}

fn seed_completed_marker(index: &SessionIndex, provider: ViewerProvider, header: &SessionHeader, marker: &str) {
  let source_key = index_source_key_for_path(provider, &header.path).unwrap();
  let session = session_metadata_from_header(&source_key, header.clone(), Some(marker.into()), false).unwrap();
  index
    .replace_sources(&[SourceReplacement::new(
      SourceState::new(
        source_key,
        completed_body_cursor(&source_cursor(provider, &header.path).unwrap()),
        0,
      ),
      vec![session],
    )])
    .unwrap();
}

fn indexed_row(index: &SessionIndex, provider: ViewerProvider, header: &SessionHeader) -> IndexedSession {
  index
    .session(&index_session_key(&locator_for_header(provider, header)).unwrap())
    .unwrap()
    .unwrap()
}

#[test]
fn cold_large_source_completion_reconciles_activity_after_an_offline_append() {
  for provider in [ViewerProvider::Codex, ViewerProvider::Pi] {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("finished.jsonl");
    std::fs::write(&path, "initial active source").unwrap();
    let header = indexed_header(path.clone(), "finished", None);
    let repository = cold_repository(
      provider,
      header.clone(),
      vec![
        indexed_message(Role::Assistant, MessageDelivery::Final),
        indexed_message(Role::Assistant, MessageDelivery::Commentary),
      ],
    );
    repository.catalog_only.store(false, Ordering::SeqCst);
    let index = Arc::new(SessionIndex::open_in_memory().unwrap());
    let service = ViewerService::new_with_index(repository.clone(), index.clone());
    service.refresh_session_index().unwrap();
    assert_eq!(
      indexed_row(&index, provider, &header).attention_marker.as_deref(),
      Some("session-activity.v4.1.1.0.0")
    );

    // The viewer is offline when the final reply is appended. On reopening,
    // the source is cold and its normal body policy is catalog-only.
    std::fs::write(&path, "initial active source followed by its completed reply").unwrap();
    repository.catalog_only.store(true, Ordering::SeqCst);
    repository.inner.loads.lock().unwrap().insert(
      locator_for_header(provider, &header),
      Ok(IndexedLoadSpec {
        header: header.clone(),
        messages: vec![
          indexed_message(Role::Assistant, MessageDelivery::Final),
          indexed_message(Role::Assistant, MessageDelivery::Commentary),
          indexed_message(Role::Assistant, MessageDelivery::Final),
        ],
      }),
    );
    let reopened = ViewerService::new_with_index(repository.clone(), index.clone());
    let refresh = reopened.refresh_session_index().unwrap();
    let repaired = indexed_row(&index, provider, &header);
    assert_eq!(
      repaired.attention_marker.as_deref(),
      Some("session-activity.v4.2.0.0.0")
    );
    assert_eq!(
      repaired.attention_revision, 1,
      "the actual new final reply remains unread"
    );
    assert_eq!(
      refresh.updated_session_keys,
      vec![encode_session_key(&locator_for_header(provider, &header)).unwrap()]
    );
    assert!(!reopened.list_sessions(ListSessionsRequest::default()).unwrap().sessions[0].is_running);
    let load_calls = repository.inner.load_calls.load(Ordering::SeqCst);
    reopened.refresh_session_index().unwrap();
    assert_eq!(repository.inner.load_calls.load(Ordering::SeqCst), load_calls);
  }
}

#[test]
fn legacy_running_marker_reconciles_even_when_the_completed_source_cursor_matches() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("stale.jsonl");
  std::fs::write(&path, "completed source with an already-matching cursor").unwrap();
  let header = indexed_header(path, "stale", None);
  let repository = cold_repository(
    ViewerProvider::Codex,
    header.clone(),
    vec![indexed_message(Role::Assistant, MessageDelivery::Final)],
  );
  let index = Arc::new(SessionIndex::open_in_memory().unwrap());
  seed_completed_marker(&index, ViewerProvider::Codex, &header, "session-activity.v3.1.1.0.0");
  let service = ViewerService::new_with_index(repository.clone(), index.clone());
  let refresh = service.refresh_session_index().unwrap();
  let repaired = indexed_row(&index, ViewerProvider::Codex, &header);
  assert_eq!(
    repaired.attention_marker.as_deref(),
    Some("session-activity.v4.1.0.0.0")
  );
  assert_eq!(
    repaired.attention_revision, 0,
    "repairing running state does not invent an unread reply"
  );
  assert_eq!(refresh.updated_session_keys.len(), 1);
  assert_eq!(repository.inner.load_calls.load(Ordering::SeqCst), 1);
  service.refresh_session_index().unwrap();
  assert_eq!(
    repository.inner.load_calls.load(Ordering::SeqCst),
    1,
    "the legacy repair runs once"
  );
}

#[test]
fn legacy_running_marker_keeps_confirmed_active_work_running() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("active.jsonl");
  std::fs::write(&path, "active source").unwrap();
  let header = indexed_header(path, "active", None);
  let repository = cold_repository(
    ViewerProvider::Codex,
    header.clone(),
    vec![indexed_message(Role::Assistant, MessageDelivery::Commentary)],
  );
  let index = Arc::new(SessionIndex::open_in_memory().unwrap());
  seed_completed_marker(&index, ViewerProvider::Codex, &header, "session-activity.v3.0.1.0.0");
  let service = ViewerService::new_with_index(repository.clone(), index.clone());
  service.refresh_session_index().unwrap();
  let repaired = indexed_row(&index, ViewerProvider::Codex, &header);
  assert_eq!(
    repaired.attention_marker.as_deref(),
    Some("session-activity.v4.0.1.0.0")
  );
  assert!(service.list_sessions(ListSessionsRequest::default()).unwrap().sessions[0].is_running);
  assert_eq!(repaired.attention_revision, 0);
  service.refresh_session_index().unwrap();
  assert_eq!(repository.inner.load_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cold_idle_and_unbaselined_histories_do_not_require_body_reads() {
  for marker in [Some("session-activity.v3.2.0.0.0"), None] {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("historical.jsonl");
    std::fs::write(&path, "unrelated historical source").unwrap();
    let header = indexed_header(path, "historical", None);
    let repository = cold_repository(ViewerProvider::Codex, header.clone(), vec![]);
    let index = Arc::new(SessionIndex::open_in_memory().unwrap());
    if let Some(marker) = marker {
      seed_completed_marker(&index, ViewerProvider::Codex, &header, marker);
      std::fs::write(&header.path, "unrelated historical source with a changed cursor").unwrap();
    }
    let service = ViewerService::new_with_index(repository.clone(), index.clone());
    service.refresh_session_index().unwrap();
    let stored = indexed_row(&index, ViewerProvider::Codex, &header);
    assert_eq!(repository.inner.load_calls.load(Ordering::SeqCst), 0);
    assert!(stored.attention_baselined);
    assert_eq!(stored.attention_revision, 0);
    assert_eq!(
      stored.attention_marker.as_deref(),
      marker.map(|_| "session-activity.v4.2.0.0.0")
    );
  }
}

#[test]
fn deferred_known_running_sources_reconcile_promptly_without_loading_idle_bodies() {
  for running in [false, true] {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("deferred.jsonl");
    std::fs::write(&path, "deferred source").unwrap();
    let header = indexed_header(path, "deferred", None);
    let repository = cold_repository(
      ViewerProvider::Codex,
      header.clone(),
      vec![indexed_message(Role::Assistant, MessageDelivery::Final)],
    );
    repository.deferred.store(true, Ordering::SeqCst);
    let index = Arc::new(SessionIndex::open_in_memory().unwrap());
    seed_completed_marker(
      &index,
      ViewerProvider::Codex,
      &header,
      &format!("session-activity.v4.1.{}.0.0", u8::from(running)),
    );
    std::fs::write(&header.path, "deferred source with appended final reply").unwrap();
    let service = ViewerService::new_with_index(repository.clone(), index.clone());
    let refresh = service.refresh_session_index().unwrap();
    let stored = indexed_row(&index, ViewerProvider::Codex, &header);
    assert_eq!(repository.inner.load_calls.load(Ordering::SeqCst), usize::from(running));
    assert_eq!(stored.attention_baselined, running);
    assert_eq!(refresh.has_pending_body_jobs, !running);
    assert_eq!(stored.attention_revision, 0);
    assert_eq!(stored.attention_marker.as_deref(), Some("session-activity.v4.1.0.0.0"));
  }
}
