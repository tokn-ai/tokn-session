use super::*;

#[test]
fn commentary_changes_publish_compact_notifications_without_catalog_reload_or_extra_reads() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("active.jsonl");
  std::fs::write(&path, "first source").unwrap();
  let header = indexed_header(path.clone(), "active", None);
  let locator = locator_for_header(ViewerProvider::Codex, &header);
  let key = encode_session_key(&locator).unwrap();
  let repository = indexing_repository(vec![IndexedLoadSpec {
    header: header.clone(),
    messages: vec![indexed_message(Role::Assistant, MessageDelivery::Commentary)],
  }]);
  let index = Arc::new(SessionIndex::open_in_memory().unwrap());
  let service = ViewerService::new_with_index(repository.clone(), index.clone());
  service.refresh_session_index().unwrap();
  let before = service.session_notifications(&[key.clone()])[0].clone();
  assert_eq!(before["latest_assistant_message"]["message_id"], "active-0");
  assert_eq!(before["latest_assistant_message"]["delivery"], "commentary");
  let initial_loads = repository.load_calls.load(Ordering::SeqCst);
  std::fs::write(&path, "second source with intermediate commentary").unwrap();
  repository.loads.lock().unwrap().insert(
    locator.clone(),
    Ok(IndexedLoadSpec {
      header,
      messages: vec![
        indexed_message(Role::Assistant, MessageDelivery::Commentary),
        indexed_message(Role::Assistant, MessageDelivery::Commentary),
      ],
    }),
  );
  service
    .refresh_changed_file_catalogs(BTreeMap::from([(ViewerProvider::Codex, BTreeSet::from([path]))]))
    .unwrap();
  let refresh = service.refresh_pending_session_index().unwrap();
  assert!(!refresh.catalog_refresh_required);
  assert!(refresh.updated_session_keys.contains(&key));
  let after = service.session_notifications(&[key.clone()])[0].clone();
  assert_eq!(after["latest_assistant_message"]["message_id"], "active-1");
  assert_eq!(after["latest_assistant_message"]["preview"], "fixture message");
  assert_ne!(after["message_revision"], before["message_revision"]);
  assert_eq!(after["unread_final_count"], 0);
  assert_eq!(repository.load_calls.load(Ordering::SeqCst), initial_loads + 1);
  for _ in 0..3 {
    assert_eq!(service.session_notifications(&[key.clone()])[0], after);
  }
  assert_eq!(
    repository.load_calls.load(Ordering::SeqCst),
    initial_loads + 1,
    "notifications read the compact index only"
  );
}

#[test]
fn parent_notifications_keep_parent_messages_when_child_activity_changes() {
  let directory = tempfile::tempdir().unwrap();
  let mut specs = Vec::new();
  for (id, parent) in [("parent", None), ("child", Some("parent"))] {
    let path = directory.path().join(format!("{id}.jsonl"));
    std::fs::write(&path, "fixture source").unwrap();
    specs.push(IndexedLoadSpec {
      header: indexed_header(path, id, parent),
      messages: vec![indexed_message(Role::Assistant, MessageDelivery::Commentary)],
    });
  }
  let child = locator_for_header(ViewerProvider::Codex, &specs[1].header);
  let repository = indexing_repository(specs);
  let service = ViewerService::new_with_index(repository.clone(), Arc::new(SessionIndex::open_in_memory().unwrap()));
  service.refresh_session_index().unwrap();
  let loads = repository.load_calls.load(Ordering::SeqCst);
  let notifications = service.session_notifications(&[encode_session_key(&child).unwrap()]);
  assert_eq!(notifications.len(), 2);
  for notification in notifications {
    let locator = decode_session_key(notification["session_key"].as_str().unwrap()).unwrap();
    assert_eq!(
      notification["latest_assistant_message"]["message_id"],
      format!("{}-0", locator.session_id)
    );
  }
  assert_eq!(repository.load_calls.load(Ordering::SeqCst), loads);
}

#[test]
fn committed_body_reset_clears_messages_and_tombstones_emit_explicit_removals() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("reset.jsonl");
  std::fs::write(&path, "initial source with visible reply").unwrap();
  let header = indexed_header(path.clone(), "reset", None);
  let locator = locator_for_header(ViewerProvider::Codex, &header);
  let key = encode_session_key(&locator).unwrap();
  let repository = indexing_repository(vec![IndexedLoadSpec {
    header: header.clone(),
    messages: vec![indexed_message(Role::Assistant, MessageDelivery::Final)],
  }]);
  let index = Arc::new(SessionIndex::open_in_memory().unwrap());
  let service = ViewerService::new_with_index(repository.clone(), index.clone());
  service.refresh_session_index().unwrap();
  std::fs::write(&path, "reset").unwrap();
  repository.loads.lock().unwrap().insert(
    locator.clone(),
    Ok(IndexedLoadSpec {
      header,
      messages: Vec::new(),
    }),
  );
  service
    .refresh_changed_file_catalogs(BTreeMap::from([(
      ViewerProvider::Codex,
      BTreeSet::from([path.clone()]),
    )]))
    .unwrap();
  let pending = service.session_notifications(&[key.clone()]);
  assert!(
    pending[0]["message_revision"].is_null(),
    "pending catalogs cannot claim a new message body revision"
  );
  service.refresh_pending_session_index().unwrap();
  let cleared = service.session_notifications(&[key.clone()])[0].clone();
  assert!(cleared["latest_assistant_message"].is_null());
  assert!(cleared["message_revision"].is_string());
  std::fs::remove_file(&path).unwrap();
  repository
    .listings
    .lock()
    .unwrap()
    .insert(ViewerProvider::Codex, Vec::new());
  service.refresh_session_catalog().unwrap();
  let removed = service.session_notifications(&[key])[0].clone();
  assert_eq!(removed["removed"], true);
  assert_eq!(removed["message_revision"], "removed");
  assert!(removed["latest_assistant_message"].is_null());
}

#[test]
fn separate_index_reader_receives_persisted_compact_projection_without_loading_sources() {
  let directory = tempfile::tempdir().unwrap();
  let source = directory.path().join("shared.jsonl");
  let database = directory.path().join("shared.sqlite");
  std::fs::write(&source, "shared first source").unwrap();
  let header = indexed_header(source.clone(), "shared", None);
  let locator = locator_for_header(ViewerProvider::Codex, &header);
  let key = encode_session_key(&locator).unwrap();
  let repository = indexing_repository(vec![IndexedLoadSpec {
    header: header.clone(),
    messages: vec![indexed_message(Role::Assistant, MessageDelivery::Final)],
  }]);
  let writer = ViewerService::new_with_index(repository.clone(), Arc::new(SessionIndex::open(&database).unwrap()));
  writer.refresh_session_index().unwrap();
  let follower = ViewerService::new_with_index(repository.clone(), Arc::new(SessionIndex::open(&database).unwrap()));
  assert_eq!(
    follower.session_notifications(&[key.clone()])[0]["latest_assistant_message"]["delivery"],
    "final"
  );
  std::fs::write(&source, "shared second source with commentary").unwrap();
  repository.loads.lock().unwrap().insert(
    locator,
    Ok(IndexedLoadSpec {
      header,
      messages: vec![
        indexed_message(Role::Assistant, MessageDelivery::Final),
        indexed_message(Role::Assistant, MessageDelivery::Commentary),
      ],
    }),
  );
  writer
    .refresh_changed_file_catalogs(BTreeMap::from([(ViewerProvider::Codex, BTreeSet::from([source]))]))
    .unwrap();
  writer.refresh_pending_session_index().unwrap();
  let calls = repository.load_calls.load(Ordering::SeqCst);
  assert!(follower.follow_shared_index().unwrap());
  let notification = follower.session_notifications(&[key])[0].clone();
  assert_eq!(notification["latest_assistant_message"]["message_id"], "shared-1");
  assert_eq!(notification["latest_assistant_message"]["delivery"], "commentary");
  assert_eq!(repository.load_calls.load(Ordering::SeqCst), calls);
}

#[test]
fn follower_scope_widening_does_not_replay_changes_from_before_subscription() {
  use crate::background::{BackgroundScope, BackgroundUpdatesRequest};
  let directory = tempfile::tempdir().unwrap();
  let specs = ["inside", "outside"].map(|id| {
    let path = directory.path().join(format!("{id}.jsonl"));
    std::fs::write(&path, "first source").unwrap();
    IndexedLoadSpec {
      header: indexed_header(path, id, None),
      messages: vec![indexed_message(Role::Assistant, MessageDelivery::Commentary)],
    }
  });
  let inside = encode_session_key(&locator_for_header(ViewerProvider::Codex, &specs[0].header)).unwrap();
  let outside_locator = locator_for_header(ViewerProvider::Codex, &specs[1].header);
  let outside = encode_session_key(&outside_locator).unwrap();
  let outside_header = specs[1].header.clone();
  let repository = indexing_repository(specs.to_vec());
  let service = ViewerService::new_with_index(repository.clone(), Arc::new(SessionIndex::open_in_memory().unwrap()));
  service.refresh_session_index().unwrap();
  service
    .subscribe_background(BackgroundUpdatesRequest {
      subscription_id: "scope".into(),
      scope: BackgroundScope::Sidebar,
      session_keys: vec![inside.clone()],
      unsubscribe: false,
    })
    .unwrap();
  assert!(
    !service.background_notification_keys(false).contains(&outside),
    "owner recovery remains scoped"
  );
  let mirrored = service.background_notification_keys(true);
  assert!(mirrored.contains(&outside), "follower observes outside rows silently");
  for notification in service.session_notifications(&mirrored) {
    service.seed_background_notification(&notification);
  }
  let append_outside = |count: usize| {
    std::fs::write(
      &outside_locator.source_path,
      format!("updated source with {count} commentary messages"),
    )
    .unwrap();
    repository.loads.lock().unwrap().insert(
      outside_locator.clone(),
      Ok(IndexedLoadSpec {
        header: outside_header.clone(),
        messages: (0..count)
          .map(|_| indexed_message(Role::Assistant, MessageDelivery::Commentary))
          .collect(),
      }),
    );
    service
      .refresh_changed_file_catalogs(BTreeMap::from([(
        ViewerProvider::Codex,
        BTreeSet::from([outside_locator.source_path.clone()]),
      )]))
      .unwrap();
    service.refresh_pending_session_index().unwrap();
  };
  let reconcile = || {
    service
      .session_notifications(&service.background_notification_keys(true))
      .iter()
      .flat_map(|notification| service.publish_background_notification(notification))
      .collect::<Vec<_>>()
  };
  append_outside(2);
  assert!(reconcile().is_empty(), "outside change is remembered without delivery");
  service
    .subscribe_background(BackgroundUpdatesRequest {
      subscription_id: "scope".into(),
      scope: BackgroundScope::All,
      session_keys: vec![],
      unsubscribe: false,
    })
    .unwrap();
  assert!(
    reconcile().is_empty(),
    "widening must not replay earlier outside changes"
  );
  append_outside(3);
  let updates = reconcile();
  assert_eq!(updates.len(), 1);
  assert_eq!(updates[0]["session_key"], outside);
  assert_eq!(updates[0]["latest_assistant_message"]["message_id"], "outside-2");
}

struct RuntimeCodexRepository {
  header: SessionHeader,
  native: NativeRepository,
  activity_calls: AtomicUsize,
  full_load_calls: AtomicUsize,
}

impl ViewerRepository for RuntimeCodexRepository {
  fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
    Ok(if provider == ViewerProvider::Codex {
      vec![self.header.clone()]
    } else {
      Vec::new()
    })
  }

  fn file_session_roots(&self, provider: ViewerProvider) -> Result<Vec<PathBuf>, String> {
    if provider != ViewerProvider::Codex {
      return Err("fixture supports Codex only".into());
    }
    Ok(vec![self.header.path.parent().unwrap().to_path_buf()])
  }

  fn file_session_paths(&self, provider: ViewerProvider) -> Result<Vec<PathBuf>, String> {
    if provider != ViewerProvider::Codex {
      return Err("fixture supports Codex only".into());
    }
    Ok(vec![self.header.path.clone()])
  }

  fn session_header_at_path(&self, provider: ViewerProvider, path: &Path) -> Result<SessionHeader, String> {
    if provider != ViewerProvider::Codex || path != self.header.path {
      return Err("unexpected fixture source".into());
    }
    Ok(self.header.clone())
  }

  fn load_activity(&self, locator: &SessionLocator) -> Result<Activity, String> {
    self.activity_calls.fetch_add(1, Ordering::SeqCst);
    self.native.load_activity(locator)
  }

  fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
    self.full_load_calls.fetch_add(1, Ordering::SeqCst);
    Err("background-only runtime test must never open a conversation history".into())
  }
}

async fn runtime_preview(
  events: &mut tokio::sync::broadcast::Receiver<crate::runtime::ViewerEvent>,
  preview: &str,
) -> Value {
  let mut observed = Vec::new();
  tokio::time::timeout(std::time::Duration::from_secs(15), async {
    loop {
      let event = events.recv().await.unwrap();
      observed.push(json!({"event":event.event,"preview":event.payload["latest_assistant_message"]["preview"],"error":event.payload["worker_error"]}));
      if event.event == "background-session-updated" && event.payload["latest_assistant_message"]["preview"] == preview {
        break event.payload;
      }
    }
  }).await.unwrap_or_else(|_| panic!("missing compact runtime preview {preview:?}; observed {observed:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_codex_appends_reach_background_runtime_without_opening_history() {
  use crate::background::{BackgroundScope, BackgroundUpdatesRequest};
  use std::io::Write;

  let directory = tempfile::tempdir().unwrap();
  // macOS watcher paths are canonical; use that same identity in the fixture
  // catalog rather than the /var -> /private/var temporary-directory alias.
  let path = directory.path().canonicalize().unwrap().join("runtime.jsonl");
  let row = |value: Value| value.to_string() + "\n";
  std::fs::write(
    &path,
    row(json!({"type":"session_meta","payload":{"id":"runtime","cwd":directory.path()}})),
  )
  .unwrap();
  let header = indexed_header(path.clone(), "runtime", None);
  let key = encode_session_key(&locator_for_header(ViewerProvider::Codex, &header)).unwrap();
  let repository = Arc::new(RuntimeCodexRepository {
    header,
    native: NativeRepository::default(),
    activity_calls: AtomicUsize::new(0),
    full_load_calls: AtomicUsize::new(0),
  });
  let service = ViewerService::new_with_index(repository.clone(), Arc::new(SessionIndex::open_in_memory().unwrap()));
  service.refresh_session_index().unwrap();
  let baseline_reads = repository.activity_calls.load(Ordering::SeqCst);
  service
    .subscribe_background(BackgroundUpdatesRequest {
      subscription_id: "runtime-background".into(),
      scope: BackgroundScope::Sidebar,
      session_keys: vec![key.clone()],
      unsubscribe: false,
    })
    .unwrap();
  assert_eq!(
    repository.activity_calls.load(Ordering::SeqCst),
    baseline_reads,
    "subscription is source-free"
  );
  let runtime = crate::runtime::ViewerRuntime::start(service);
  let mut events = runtime.events.subscribe();
  let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
  for fragment in ["Inspecting ", "workspace"] {
    write!(
      file,
      "{}",
      row(json!({"type":"event_msg","payload":{"type":"agent_message_delta","item_id":"progress","delta":fragment}}))
    )
    .unwrap();
  }
  file.flush().unwrap();
  drop(file);
  // Drive the same typed changed-source hint as Relay. Platform watcher
  // delivery is covered separately, so this pipeline test stays deterministic.
  runtime
    .service
    .relay
    .index_wakes
    .send((ViewerProvider::Codex, path.clone()))
    .unwrap();
  let update = runtime_preview(&mut events, "Inspecting workspace").await;
  assert_eq!(update["session_key"], key);
  assert_eq!(update["latest_assistant_message"]["phase"], "delta");
  assert!(update["message_revision"].is_string());
  let before_append = repository.activity_calls.load(Ordering::SeqCst);
  let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
  write!(
    file,
    "{}",
    row(json!({"type":"event_msg","payload":{"type":"agent_message_delta","item_id":"progress","delta":"."}}))
  )
  .unwrap();
  file.flush().unwrap();
  drop(file);
  // Drive the same typed changed-source hint as Relay. Platform watcher
  // delivery is covered separately, so this pipeline test stays deterministic.
  runtime
    .service
    .relay
    .index_wakes
    .send((ViewerProvider::Codex, path.clone()))
    .unwrap();
  runtime_preview(&mut events, "Inspecting workspace.").await;
  assert!(repository.activity_calls.load(Ordering::SeqCst) > before_append);
  let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
  write!(file, "{}", row(json!({"type":"response_item","payload":{"type":"message","id":"progress","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Final result\nLonger explanation stays in the source."}]}}))).unwrap();
  file.flush().unwrap();
  drop(file);
  // Drive the same typed changed-source hint as Relay. Platform watcher
  // delivery is covered separately, so this pipeline test stays deterministic.
  runtime
    .service
    .relay
    .index_wakes
    .send((ViewerProvider::Codex, path.clone()))
    .unwrap();
  let final_update = runtime_preview(&mut events, "Final result").await;
  assert_eq!(final_update["latest_assistant_message"]["delivery"], "final");
  assert_eq!(final_update["latest_assistant_message"].as_object().unwrap().len(), 4);
  assert_eq!(repository.full_load_calls.load(Ordering::SeqCst), 0);
}
