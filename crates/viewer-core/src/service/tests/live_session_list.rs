use super::*;
use crate::session_list::{OpenSessionListRequest, SessionListSubscriptionRequest, SessionListUpdate};

fn request(id: &str, limit: usize) -> OpenSessionListRequest {
  serde_json::from_value(json!({"subscription_id":id,"limit":limit,"query":{"providers":["codex"]}})).unwrap()
}

fn fixture(headers: Vec<SessionHeader>) -> ViewerService {
  service_with_indexed_headers(
    Arc::new(FakeRepository {
      listings: HashMap::new(),
      loaded: Mutex::new(None),
    }),
    vec![(ViewerProvider::Codex, headers)],
  )
}

fn replace(service: &ViewerService, header: &SessionHeader, present: bool) {
  let key = index_source_key_for_path(ViewerProvider::Codex, &header.path).unwrap();
  let sessions = if present {
    vec![session_metadata_from_header(&key, header.clone(), None, false).unwrap()]
  } else {
    Vec::new()
  };
  service
    .session_index
    .replace_source(SourceReplacement::new(SourceState::new(key, "changed", 1), sessions))
    .unwrap();
}

fn published(service: &ViewerService) -> Vec<SessionListUpdate> {
  service.session_lists.invalidate(None);
  let mut out = Vec::new();
  service.publish_session_lists(|u| out.push(u));
  out
}

#[test]
fn live_session_list_sends_one_renamed_row_and_preserves_filtered_scoped_windows() {
  let mut a = session_header("a", None, "/project", "2026-01-03T00:00:00Z");
  let mut b = session_header("b", None, "/project", "2026-01-02T00:00:00Z");
  let c = session_header("c", None, "/project", "2026-01-01T00:00:00Z");
  let service = fixture(vec![a.clone(), b.clone(), c.clone()]);
  let first = service.open_session_list(request("owner", 2)).unwrap();
  assert_eq!(
    first.rows.iter().map(|r| r.session_id.as_str()).collect::<Vec<_>>(),
    ["a", "b"]
  );
  assert!(first.base_revision.is_none() && first.order_edits.is_empty() && first.next_cursor.is_some());

  let guest_key = first.rows[1].session_key.clone();
  let guest = service.scoped_to_sessions(std::slice::from_ref(&guest_key)).unwrap();
  let guest_req = request("share_list_guest:visible", 2);
  let granted = guest.open_session_list(guest_req.clone()).unwrap();
  assert_eq!(granted.rows.len(), 1);
  assert_eq!(granted.rows[0].session_key, guest_key);
  let mut filter = request("search", 5);
  filter.query.search = Some("needle".into());
  assert!(service.open_session_list(filter).unwrap().rows.is_empty());

  a.title = Some("renamed".into());
  replace(&service, &a, true);
  let updates = published(&service);
  assert_eq!(updates.len(), 1);
  let update = &updates[0];
  assert_eq!(update.subscription_id, "owner");
  assert_eq!(update.base_revision.as_ref(), Some(&first.revision));
  assert_eq!(update.rows.len(), 1);
  assert_eq!(update.rows[0].title.as_deref(), Some("renamed"));
  assert!(update.order_edits.is_empty() && update.removed_keys.is_empty());
  assert!(published(&service).is_empty());

  let mut expand = request("owner", 3);
  expand.generation = Some(update.generation.clone());
  expand.revision = Some(update.revision.clone());
  let expanded = service.open_session_list(expand.clone()).unwrap();
  assert_eq!(expanded.rows.len(), 1);
  assert_eq!(expanded.rows[0].session_id, "c");
  assert!(expanded.next_cursor.is_none());
  assert_eq!(expanded.order_edits[0].start, 2);
  assert_eq!(expanded.order_edits[0].insert, [expanded.rows[0].session_key.clone()]);
  let recovered = service.open_session_list(expand).unwrap();
  assert!(recovered.base_revision.is_none());
  assert_eq!(recovered.revision, expanded.revision);
  assert_eq!(recovered.rows.len(), 3);

  b.title = Some("needle".into());
  replace(&service, &b, true);
  let updates = published(&service);
  assert_eq!(updates.len(), 3);
  for update in updates {
    assert_eq!(update.rows.len(), 1);
    assert_eq!(update.rows[0].session_id, "b");
    assert_eq!(
      update.order_edits.len(),
      usize::from(update.subscription_id == "search")
    );
  }
  let id = || SessionListSubscriptionRequest {
    subscription_id: guest_req.subscription_id.clone(),
  };
  assert!(service.release_session_list(id()).is_err());
  assert_eq!(
    guest
      .renew_session_lists(&[id().subscription_id])
      .unwrap()
      .renewed_ids
      .len(),
    1
  );
  assert!(guest.release_session_list(id()).unwrap());
  assert_eq!(
    guest
      .renew_session_lists(&[id().subscription_id])
      .unwrap()
      .missing_ids
      .len(),
    1
  );
  replace(&service, &b, false);
  let updates = published(&service);
  assert_eq!(updates.len(), 2);
  for update in updates {
    assert!(update.rows.is_empty());
    assert_eq!(update.removed_keys, [guest_key.clone()]);
    assert_eq!(update.order_edits.len(), 1);
    assert_eq!(update.order_edits[0].delete_count, 1);
    assert!(update.order_edits[0].insert.is_empty());
  }
}

#[test]
fn live_child_window_tracks_new_and_removed_members_including_parent_removal() {
  let parent = session_header("parent", None, "/project", "2026-01-04T00:00:00Z");
  let a = session_header("a", Some("parent"), "/project", "2026-01-03T00:00:00Z");
  let new = session_header("b", Some("parent"), "/project", "2026-01-05T00:00:00Z");
  let service = fixture(vec![parent.clone(), a.clone()]);
  let mut children = request("children", 2);
  children.parent_session_key = Some(encode_session_key(&locator_for_header(ViewerProvider::Codex, &parent)).unwrap());
  let first = service.open_session_list(children).unwrap();
  assert_eq!(first.rows.len(), 1);
  assert_eq!(first.rows[0].session_id, "a");
  replace(&service, &new, true);
  let inserted = published(&service).pop().unwrap();
  assert_eq!(inserted.rows.len(), 1);
  assert_eq!(inserted.rows[0].session_id, "b");
  assert_eq!(inserted.order_edits[0].start, 0);
  replace(&service, &parent, false);
  let removed = published(&service).pop().unwrap();
  assert!(removed.rows.is_empty());
  assert_eq!(removed.removed_keys.len(), 2);
  assert_eq!(removed.order_edits[0].delete_count, 2);
  assert!(published(&service).is_empty());
}

#[test]
fn live_session_list_offset_is_a_bounded_slice_of_the_same_order_as_pagination() {
  let service = fixture(
    (0..202)
      .map(|i| session_header(&format!("s{i}"), None, "/project", "2026-01-01T00:00:00Z"))
      .collect(),
  );
  let mut deep = request("deep", 1);
  deep.offset = 200;
  let slice = service.open_session_list(deep).unwrap();
  let prefix = service.open_session_list(request("prefix", 201)).unwrap();
  assert_eq!(prefix.rows.len(), 201);
  assert_eq!(prefix.rows[200].session_key, slice.rows[0].session_key);
  assert_eq!(prefix.next_cursor, slice.next_cursor);
  let overflow = service
    .list_sessions(ListSessionsRequest {
      cursor: slice.next_cursor,
      ..Default::default()
    })
    .unwrap();
  assert_eq!(overflow.sessions.len(), 1);
  assert!(service.open_session_list(request("huge", 4097)).is_err());
}

#[test]
fn idle_windows_and_batched_renewals_do_not_read_or_advance_the_projection() {
  let mut visible = session_header("visible", None, "/project", "2026-01-02T00:00:00Z");
  let private = session_header("private", None, "/project", "2026-01-01T00:00:00Z");
  let service = fixture(vec![visible.clone(), private.clone()]);
  let initial = service.open_session_list(request("owner", 2)).unwrap();
  let allowed = initial.rows[0].session_key.clone();
  let hidden = initial.rows[1].session_key.clone();
  let guest = service.scoped_to_sessions(std::slice::from_ref(&allowed)).unwrap();
  let grant = guest.open_session_list(request("share_list_guest:test", 2)).unwrap();

  // Mutate the fixture index without its normal commit notification. An idle
  // publisher and a lease-only heartbeat must not discover the new title.
  visible.title = Some("changed".into());
  replace(&service, &visible, true);
  let renewal = service
    .renew_session_lists(&["owner".into(), "absent".into(), "owner".into()])
    .unwrap();
  assert_eq!(renewal.renewed_ids, ["owner"]);
  assert_eq!(renewal.missing_ids, ["absent"]);
  service.publish_session_lists(|_| panic!("an idle list was queried"));

  service.session_lists.invalidate(Some(&[hidden]));
  let mut observed = Vec::new();
  service.publish_session_lists(|update| observed.push(update));
  assert_eq!(observed.len(), 1);
  assert_eq!(observed[0].subscription_id, initial.subscription_id);

  service.session_lists.invalidate(Some(&[allowed]));
  let mut observed = Vec::new();
  service.publish_session_lists(|update| observed.push(update));
  assert_eq!(observed.len(), 1);
  assert_eq!(observed[0].subscription_id, grant.subscription_id);
  assert_eq!(observed[0].rows[0].title.as_deref(), Some("changed"));
}

#[tokio::test]
async fn live_session_list_publication_follows_commits_and_directory_switches() {
  let mut record = session_header("visible", None, "/project", "2026-01-02T00:00:00Z");
  let service = fixture(vec![record.clone()]);
  let open = service.open_session_list(request("view", 1)).unwrap();
  let (events, _) = tokio::sync::broadcast::channel(16);
  record.title = Some("committed".to_owned());
  replace(&service, &record, true);
  crate::runtime::publish_index_refresh(&service, &events, IndexRefresh::default()).await;
  service.publish_session_lists(|_| panic!("an unchanged reconciliation woke a watch"));
  crate::runtime::publish_index_refresh(
    &service,
    &events,
    IndexRefresh {
      changed: true,
      updated_session_keys: vec![open.rows[0].session_key.clone()],
      ..Default::default()
    },
  )
  .await;
  let mut changes = Vec::new();
  service.publish_session_lists(|u| changes.push(u));
  assert_eq!(changes.len(), 1);
  assert_eq!(changes[0].rows[0].title.as_deref(), Some("committed"));

  record.title = Some("directory changed".to_owned());
  replace(&service, &record, true);
  crate::runtime::publish_relay_change(
    service.clone(),
    &events,
    crate::relay::RelayChange {
      session_key: Some(open.rows[0].session_key.clone()),
      reset: false,
    },
  )
  .await;
  service.publish_session_lists(|_| panic!("a timeline-only update queried the directory"));
  crate::runtime::publish_relay_change(
    service.clone(),
    &events,
    crate::relay::RelayChange {
      session_key: None,
      reset: false,
    },
  )
  .await;
  let mut changes = Vec::new();
  service.publish_session_lists(|u| changes.push(u));
  assert_eq!(changes.len(), 1);
  assert_eq!(changes[0].rows[0].title.as_deref(), Some("directory changed"));
}
