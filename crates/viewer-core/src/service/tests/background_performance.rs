use super::*;
use crate::background::{BackgroundScope, BackgroundUpdatesRequest};
use crate::runtime::{ViewerEvent, publish_index_refresh};
use std::io::Write;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

struct BackgroundFixture {
  _directory: tempfile::TempDir,
  service: ViewerService,
  repository: Arc<IndexingRepository>,
  headers: HashMap<String, SessionHeader>,
  child_key: String,
  affected_keys: BTreeSet<String>,
}

impl BackgroundFixture {
  fn new(historical_sessions: usize) -> Self {
    let directory = tempfile::tempdir().unwrap();
    let specs = [
      ("grandparent", None),
      ("parent", Some("grandparent")),
      ("child", Some("parent")),
      ("other-parent", None),
    ]
    .into_iter()
    .map(|(id, parent)| {
      let path = directory.path().join(format!("{id}.jsonl"));
      std::fs::write(&path, "initial fixture record\n").unwrap();
      IndexedLoadSpec {
        header: indexed_header(path, id, parent),
        messages: vec![indexed_message(Role::Assistant, MessageDelivery::Commentary)],
      }
    })
    .collect::<Vec<_>>();
    let headers = specs
      .iter()
      .map(|spec| (spec.header.id.clone(), spec.header.clone()))
      .collect::<HashMap<_, _>>();
    let repository = indexing_repository(specs.clone());
    let index = Arc::new(SessionIndex::open_in_memory().unwrap());
    let mut replacements = specs
      .iter()
      .map(|spec| {
        let locator = locator_for_header(ViewerProvider::Codex, &spec.header);
        let activity = Activity::from_loaded(spec.load(), &locator.session_id).unwrap();
        let source_key = index_source_key_for_path(ViewerProvider::Codex, &spec.header.path).unwrap();
        let mut metadata =
          session_metadata_from_header(&source_key, spec.header.clone(), Some(activity.marker()), false).unwrap();
        metadata.latest_message = activity.presentation.latest_message;
        SourceReplacement::baseline(
          SourceState::new(
            source_key,
            completed_body_cursor(&source_cursor(ViewerProvider::Codex, &spec.header.path).unwrap()),
            0,
          ),
          vec![metadata],
        )
      })
      .collect::<Vec<_>>();
    for number in 0..historical_sessions {
      let id = format!("historical-{number:05}");
      // Historical rows need no source file: publication must read the index
      // only, and an ordinary append must never rediscover these paths.
      let header = indexed_header(directory.path().join(format!("{id}.jsonl")), &id, None);
      let source_key = index_source_key_for_path(ViewerProvider::Codex, &header.path).unwrap();
      let mut metadata =
        session_metadata_from_header(&source_key, header, Some("session-activity.v4.1.0.0.0".into()), false).unwrap();
      metadata.latest_message = Some(
        json!({"message_id":format!("{id}-reply"),"delivery":"final","phase":"finished","preview":"Historical reply"})
          .to_string(),
      );
      replacements.push(SourceReplacement::baseline(
        SourceState::new(source_key, completed_body_cursor("historical"), 0),
        vec![metadata],
      ));
    }
    replacements.push(SourceReplacement::new(
      SourceState::new(index_catalog_source_key(ViewerProvider::Codex), "headers.v2", 0),
      Vec::new(),
    ));
    index.replace_sources(&replacements).unwrap();
    let service = ViewerService::new_with_index(repository.clone(), index);
    let keys = service.background_baseline_keys();
    for notification in service.session_notifications(&keys) {
      service.seed_background_notification(&notification);
    }
    let request = serde_json::from_value::<BackgroundUpdatesRequest>(json!({"subscription_id":"default"})).unwrap();
    assert_eq!(request.scope, BackgroundScope::All);
    service.subscribe_background(request).unwrap();
    let child_key = encode_session_key(&locator_for_header(ViewerProvider::Codex, &headers["child"])).unwrap();
    let affected_keys = ["child", "parent", "grandparent"]
      .into_iter()
      .map(|id| encode_session_key(&locator_for_header(ViewerProvider::Codex, &headers[id])).unwrap())
      .collect();
    Self {
      _directory: directory,
      service,
      repository,
      headers,
      child_key,
      affected_keys,
    }
  }

  fn append(&self) -> [IndexRefresh; 2] {
    let header = &self.headers["child"];
    writeln!(
      std::fs::OpenOptions::new().append(true).open(&header.path).unwrap(),
      "appended fixture record"
    )
    .unwrap();
    let locator = locator_for_header(ViewerProvider::Codex, header);
    self.repository.loads.lock().unwrap().insert(
      locator,
      Ok(IndexedLoadSpec {
        header: header.clone(),
        messages: vec![
          indexed_message(Role::Assistant, MessageDelivery::Commentary),
          indexed_message(Role::Assistant, MessageDelivery::Commentary),
        ],
      }),
    );
    [
      self
        .service
        .refresh_changed_file_catalogs(BTreeMap::from([(
          ViewerProvider::Codex,
          BTreeSet::from([header.path.clone()]),
        )]))
        .unwrap(),
      self.service.refresh_pending_session_index().unwrap(),
    ]
  }
}

fn emitted(receiver: &mut broadcast::Receiver<ViewerEvent>, event: &str) -> Vec<Value> {
  let mut values = Vec::new();
  while let Ok(update) = receiver.try_recv() {
    if update.event == event {
      values.push(update.payload);
    }
  }
  values
}

#[tokio::test]
async fn default_all_append_checks_only_changed_session_and_ancestors_without_historical_replay() {
  for historical_sessions in [1_000, 5_000] {
    let fixture = BackgroundFixture::new(historical_sessions);
    let initial_loads = fixture.repository.load_calls.load(Ordering::SeqCst);
    let initial_headers = fixture.repository.header_calls.load(Ordering::SeqCst);
    let refreshes = fixture.append();
    let before = fixture.service.background_work_stats();
    let (events, mut receiver) = broadcast::channel(32);
    for refresh in refreshes {
      assert!(!refresh.background_reconciliation_required);
      let body_completed = !refresh.updated_session_keys.is_empty();
      let notifications_before = fixture.service.notification_work_stats();
      publish_index_refresh(&fixture.service, &events, refresh).await;
      let notifications_after = fixture.service.notification_work_stats();
      assert_eq!(
        notifications_after.provider_snapshots - notifications_before.provider_snapshots,
        usize::from(body_completed),
        "header staging publishes no body notifications"
      );
      assert_eq!(
        notifications_after.rows_loaded - notifications_before.rows_loaded,
        if body_completed { historical_sessions + 4 } else { 0 },
        "one provider metadata snapshot serves both relation attention and compact messages"
      );
    }
    let after = fixture.service.background_work_stats();
    assert_eq!(after.reconciliation_key_queries, before.reconciliation_key_queries);
    assert_eq!(after.reconciliation_keys, before.reconciliation_keys);
    assert_eq!(after.fingerprint_checks - before.fingerprint_checks, 3);
    let updates = emitted(&mut receiver, "background-session-updated");
    assert_eq!(updates.len(), 1, "cached historical previews never replay on an append");
    assert_eq!(updates[0]["session_key"], fixture.child_key);
    assert_eq!(updates[0]["latest_assistant_message"]["message_id"], "child-1");
    assert_eq!(fixture.repository.load_calls.load(Ordering::SeqCst), initial_loads + 1);
    assert_eq!(fixture.repository.header_calls.load(Ordering::SeqCst), initial_headers);

    let (events, mut receiver) = broadcast::channel(32);
    let refresh = fixture.service.refresh_pending_session_index().unwrap();
    publish_index_refresh(&fixture.service, &events, refresh).await;
    assert!(emitted(&mut receiver, "background-session-updated").is_empty());
    assert_eq!(fixture.service.background_work_stats(), after);
  }
}

#[test]
fn targeted_notifications_cover_only_canonical_ancestors_from_one_provider_snapshot() {
  let fixture = BackgroundFixture::new(1_000);
  let before = fixture.service.notification_work_stats();
  let notifications = fixture.service.session_notifications(&[fixture.child_key.clone()]);
  assert_eq!(
    notifications
      .iter()
      .map(|notification| notification["session_key"].as_str().unwrap().to_owned())
      .collect::<BTreeSet<_>>(),
    fixture.affected_keys
  );
  let after = fixture.service.notification_work_stats();
  assert_eq!(after.provider_snapshots - before.provider_snapshots, 1);
  assert_eq!(after.rows_loaded - before.rows_loaded, 1_004);
}

#[tokio::test]
async fn relationship_changes_and_membership_removals_reconcile_all_known_interests() {
  let mut fixture = BackgroundFixture::new(0);
  let (events, mut receiver) = broadcast::channel(64);
  let old_parent = encode_session_key(&locator_for_header(ViewerProvider::Codex, &fixture.headers["parent"])).unwrap();
  let new_parent = encode_session_key(&locator_for_header(
    ViewerProvider::Codex,
    &fixture.headers["other-parent"],
  ))
  .unwrap();
  fixture.headers.get_mut("child").unwrap().parent_session_id = Some("other-parent".into());
  let child = fixture.headers["child"].clone();
  fixture
    .repository
    .targeted_headers
    .lock()
    .unwrap()
    .insert((ViewerProvider::Codex, child.path.clone()), Ok(child.clone()));
  writeln!(
    std::fs::OpenOptions::new().append(true).open(&child.path).unwrap(),
    "changed fixture parent"
  )
  .unwrap();
  let refresh = fixture
    .service
    .refresh_changed_file_catalogs(BTreeMap::from([(
      ViewerProvider::Codex,
      BTreeSet::from([child.path.clone()]),
    )]))
    .unwrap();
  assert!(refresh.background_reconciliation_required);
  let before = fixture.service.background_work_stats();
  publish_index_refresh(&fixture.service, &events, refresh).await;
  let after = fixture.service.background_work_stats();
  assert_eq!(after.reconciliation_key_queries - before.reconciliation_key_queries, 1);
  assert_eq!(after.reconciliation_keys - before.reconciliation_keys, 4);
  let updates = emitted(&mut receiver, "background-session-updated");
  assert!(
    updates
      .iter()
      .any(|value| value["session_key"] == old_parent && value["has_running_descendant"] == false)
  );
  assert!(
    updates
      .iter()
      .any(|value| value["session_key"] == new_parent && value["has_running_descendant"] == true)
  );

  fixture.repository.listings.lock().unwrap().insert(
    ViewerProvider::Codex,
    fixture
      .headers
      .values()
      .filter(|header| header.id != "child")
      .cloned()
      .collect(),
  );
  std::fs::remove_file(&child.path).unwrap();
  let refresh = fixture.service.refresh_session_catalog().unwrap();
  assert!(refresh.background_reconciliation_required);
  publish_index_refresh(&fixture.service, &events, refresh).await;
  let updates = emitted(&mut receiver, "background-session-updated");
  let removed = updates
    .iter()
    .find(|value| value["session_key"] == fixture.child_key)
    .unwrap();
  assert_eq!(removed["removed"], true);
  assert_eq!(removed["message_revision"], "removed");
  assert!(removed["latest_assistant_message"].is_null());
  assert!(
    updates
      .iter()
      .any(|value| value["session_key"] == new_parent && value["has_running_descendant"] == false)
  );
}

// Reproduce the former owner policy using the same current metadata projector,
// isolating the cost of reconciling every cached All interest on a sidebar
// refresh. This is a publication component comparison, not whole-app CPU.
async fn publish_with_legacy_fanout(service: &ViewerService, refresh: IndexRefresh) {
  if !refresh.changed {
    return;
  }
  if refresh.catalog_refresh_required {
    let service = service.clone();
    tokio::task::spawn_blocking(move || {
      let keys = service.background_notification_keys(false);
      for notification in service.session_notifications(&keys) {
        service.publish_background_notification(&notification);
      }
    })
    .await
    .unwrap();
  }
  let mut keys = refresh.updated_session_keys;
  keys.extend(refresh.attention_session_keys);
  keys.sort();
  keys.dedup();
  if !keys.is_empty() {
    let service = service.clone();
    tokio::task::spawn_blocking(move || {
      for notification in service.session_notifications(&keys) {
        service.publish_background_notification(&notification);
      }
    })
    .await
    .unwrap();
  }
}

fn median(samples: &mut [Duration]) -> Duration {
  samples.sort_unstable();
  samples[samples.len() / 2]
}

/// Run with `cargo test -p tokn-viewer-core benchmark_background_append_publication --lib -- --ignored --nocapture`.
/// Both arms have identical synthetic indexes, default All interests, cached
/// fingerprints and one actual temporary-file append. Fixture construction,
/// catalog/body indexing and initial fingerprint seeding are outside timings.
#[tokio::test]
#[ignore = "manual synthetic publication CPU benchmark"]
async fn benchmark_background_append_publication() {
  for historical_sessions in [1_000, 5_000] {
    let mut legacy = Vec::new();
    let mut targeted = Vec::new();
    for trial in 0..5 {
      for old_policy in if trial % 2 == 0 { [true, false] } else { [false, true] } {
        let fixture = BackgroundFixture::new(historical_sessions);
        let refreshes = fixture.append();
        let before = fixture.service.background_work_stats();
        let (events, _) = broadcast::channel(32);
        let started = Instant::now();
        for refresh in refreshes {
          if old_policy {
            publish_with_legacy_fanout(&fixture.service, refresh).await;
          } else {
            publish_index_refresh(&fixture.service, &events, refresh).await;
          }
        }
        let elapsed = started.elapsed();
        let after = fixture.service.background_work_stats();
        if old_policy {
          assert_eq!(after.reconciliation_key_queries - before.reconciliation_key_queries, 1);
          assert_eq!(
            after.reconciliation_keys - before.reconciliation_keys,
            historical_sessions + 4
          );
          legacy.push(elapsed);
        } else {
          assert_eq!(after.reconciliation_key_queries, before.reconciliation_key_queries);
          assert_eq!(after.fingerprint_checks - before.fingerprint_checks, 3);
          targeted.push(elapsed);
        }
      }
    }
    eprintln!(
      "background publication, {} indexed sessions, one append, 5 alternating trials: legacy All fanout median {:?}, targeted median {:?}; both use current one-snapshot metadata projection",
      historical_sessions + 4,
      median(&mut legacy),
      median(&mut targeted),
    );
  }
}
