use super::*;
use crate::session_list::{
  OpenSessionListRequest, SessionListEntry, SessionListSubscriptionRequest, SessionListUpdate,
};

fn request(id: &str, limit: usize) -> OpenSessionListRequest {
  serde_json::from_value(json!({"subscription_id":id,"limit":limit,"query":{"providers":["codex"]}})).unwrap()
}

fn resume(id: &str, limit: usize, since: &str) -> OpenSessionListRequest {
  OpenSessionListRequest {
    since: Some(since.into()),
    ..request(id, limit)
  }
}

fn release(service: &ViewerService, id: &str) {
  assert!(
    service
      .release_session_list(SessionListSubscriptionRequest {
        subscription_id: id.to_owned(),
      })
      .unwrap()
  );
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

fn entry_by_id<'a>(update: &'a SessionListUpdate, id: &str) -> &'a SessionListEntry {
  update
    .sessions
    .iter()
    .find(|entry| entry.session.session_id == id)
    .expect("missing fixture session")
}

#[test]
fn live_session_list_sends_one_renamed_session_and_preserves_filtered_scoped_windows() {
  let mut a = session_header("a", None, "/project", "2026-01-03T00:00:00Z");
  let mut b = session_header("b", None, "/project", "2026-01-02T00:00:00Z");
  let c = session_header("c", None, "/project", "2026-01-01T00:00:00Z");
  let service = fixture(vec![a.clone(), b.clone(), c.clone()]);
  let first = service.open_session_list(request("owner", 2)).unwrap();
  assert_eq!(
    first
      .sessions
      .iter()
      .map(|entry| entry.session.session_id.as_str())
      .collect::<BTreeSet<_>>(),
    BTreeSet::from(["a", "b"])
  );
  assert!(first.base_cursor.is_none() && first.next_cursor.is_some());

  let guest_key = entry_by_id(&first, "b").session.session_key.clone();
  let guest = service.scoped_to_sessions(std::slice::from_ref(&guest_key)).unwrap();
  let guest_req = request("share_list_guest:visible", 2);
  let granted = guest.open_session_list(guest_req.clone()).unwrap();
  assert_eq!(granted.sessions.len(), 1);
  assert_eq!(granted.sessions[0].session.session_key, guest_key);
  let mut filter = request("search", 5);
  filter.query.search = Some("needle".into());
  assert!(service.open_session_list(filter).unwrap().sessions.is_empty());

  a.title = Some("renamed".into());
  replace(&service, &a, true);
  let updates = published(&service);
  assert_eq!(updates.len(), 1);
  let update = &updates[0];
  assert_eq!(update.subscription_id, "owner");
  assert_eq!(update.base_cursor.as_ref(), Some(&first.synced_through));
  assert_eq!(update.sessions.len(), 1);
  assert_eq!(update.sessions[0].session.title.as_deref(), Some("renamed"));
  assert_ne!(entry_by_id(update, "a").revision, entry_by_id(&first, "a").revision);
  assert_eq!(
    update.sessions[0].revision,
    format!(
      "{:x}",
      Sha256::digest(serde_json::to_vec(&update.sessions[0].session).unwrap())
    )
  );
  assert!(update.removed_keys.is_empty());
  assert!(published(&service).is_empty());

  let expanded = service
    .open_session_list(resume("owner", 3, &update.synced_through))
    .unwrap();
  assert_eq!(expanded.base_cursor.as_ref(), Some(&update.synced_through));
  assert_eq!(expanded.sessions.len(), 1);
  assert_eq!(expanded.sessions[0].session.session_id, "c");
  assert!(expanded.next_cursor.is_none());
  let recovered = service
    .open_session_list(resume("owner", 3, &expanded.synced_through))
    .unwrap();
  assert_eq!(recovered.base_cursor.as_ref(), Some(&expanded.synced_through));
  assert_eq!(recovered.synced_through, expanded.synced_through);
  assert!(recovered.sessions.is_empty() && recovered.removed_keys.is_empty());

  b.title = Some("needle".into());
  replace(&service, &b, true);
  let updates = published(&service);
  assert_eq!(updates.len(), 3);
  for update in updates {
    assert_eq!(update.sessions.len(), 1);
    assert_eq!(update.sessions[0].session.session_id, "b");
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
    assert!(update.sessions.is_empty());
    assert_eq!(update.removed_keys, [guest_key.clone()]);
  }
}

#[test]
fn two_clients_share_a_cutoff_and_an_offline_client_resumes_after_multiple_changes() {
  let mut a = session_header("a", None, "/project", "2026-01-03T00:00:00Z");
  let b = session_header("b", None, "/project", "2026-01-02T00:00:00Z");
  let c = session_header("c", None, "/project", "2026-01-01T00:00:00Z");
  let d = session_header("d", None, "/project", "2026-01-04T00:00:00Z");
  let service = fixture(vec![a.clone(), b.clone(), c]);
  let desktop = service.open_session_list(request("desktop", 10)).unwrap();
  let reads_after_open = service.session_lists.read_count();
  let phone = service.open_session_list(request("phone", 10)).unwrap();
  assert_eq!(desktop.synced_through, phone.synced_through);
  assert_eq!(service.session_lists.read_count(), reads_after_open);
  let b_key = entry_by_id(&desktop, "b").session.session_key.clone();

  a.title = Some("first edit".into());
  replace(&service, &a, true);
  let reads = service.session_lists.read_count();
  let updates = published(&service);
  assert_eq!(service.session_lists.read_count() - reads, 1);
  assert_eq!(
    updates
      .iter()
      .map(|u| u.subscription_id.as_str())
      .collect::<BTreeSet<_>>(),
    BTreeSet::from(["desktop", "phone"])
  );
  for update in &updates {
    assert_eq!(update.base_cursor.as_ref(), Some(&desktop.synced_through));
    assert_eq!(update.synced_through, updates[0].synced_through);
    assert_ne!(update.synced_through, desktop.synced_through);
    assert_eq!(update.sessions.len(), 1);
    assert_eq!(update.sessions[0].session.title.as_deref(), Some("first edit"));
  }

  // Reconnect the phone from its initial snapshot, as if the published edit
  // never arrived. Releasing every lease must preserve shared replay coverage.
  release(&service, "desktop");
  release(&service, "phone");
  replace(&service, &b, false);
  a.title = Some("last edit".into());
  replace(&service, &a, true);
  replace(&service, &d, true);
  assert!(published(&service).is_empty());
  let resumed = service
    .open_session_list(resume("new-phone", 10, &phone.synced_through))
    .unwrap();
  assert_eq!(resumed.base_cursor.as_ref(), Some(&phone.synced_through));
  assert_ne!(resumed.synced_through, updates[0].synced_through);
  assert_eq!(resumed.removed_keys, [b_key]);
  assert_eq!(
    resumed
      .sessions
      .iter()
      .map(|entry| entry.session.session_id.as_str())
      .collect::<BTreeSet<_>>(),
    BTreeSet::from(["a", "d"])
  );
  assert_eq!(entry_by_id(&resumed, "a").session.title.as_deref(), Some("last edit"));
  let caught_up = service
    .open_session_list(resume("new-phone", 10, &resumed.synced_through))
    .unwrap();
  assert_eq!(caught_up.base_cursor.as_ref(), Some(&resumed.synced_through));
  assert_eq!(caught_up.synced_through, resumed.synced_through);
  assert!(caught_up.sessions.is_empty() && caught_up.removed_keys.is_empty());
}

#[test]
fn reopening_before_publication_preserves_the_other_clients_pending_update() {
  let mut a = session_header("a", None, "/project", "2026-01-03T00:00:00Z");
  let service = fixture(vec![a.clone()]);
  let desktop = service.open_session_list(request("desktop", 10)).unwrap();
  let phone = service.open_session_list(request("phone", 10)).unwrap();

  a.title = Some("new title".into());
  replace(&service, &a, true);
  service.session_lists.invalidate(None);
  let early = service
    .open_session_list(resume("phone", 10, &phone.synced_through))
    .unwrap();
  assert_eq!(early.base_cursor.as_ref(), Some(&phone.synced_through));
  assert_eq!(entry_by_id(&early, "a").session.title.as_deref(), Some("new title"));

  let mut events = Vec::new();
  service.publish_session_lists(|update| events.push(update));
  let pending = events
    .iter()
    .find(|update| update.subscription_id == "desktop")
    .unwrap();
  assert_eq!(pending.base_cursor.as_ref(), Some(&desktop.synced_through));
  assert_eq!(pending.synced_through, early.synced_through);
  assert_eq!(entry_by_id(pending, "a").session.title.as_deref(), Some("new title"));
  service.publish_session_lists(|_| panic!("an idle publication replayed an update"));
}

#[test]
fn replay_merges_a_session_that_left_and_reentered_a_search() {
  let mut a = session_header("a", None, "/project", "2026-01-03T00:00:00Z");
  a.title = Some("needle".into());
  let service = fixture(vec![a.clone()]);
  let mut search = request("old", 10);
  search.query.search = Some("needle".into());
  let original = service.open_session_list(search.clone()).unwrap();
  let key = entry_by_id(&original, "a").session.session_key.clone();
  a.title = Some("hidden".into());
  replace(&service, &a, true);
  let departure = published(&service).pop().unwrap();
  assert_eq!(departure.removed_keys, [key.clone()]);

  a.title = Some("found needle".into());
  replace(&service, &a, true);
  let arrival = published(&service).pop().unwrap();
  search.subscription_id = "new".into();
  search.since = Some(original.synced_through.clone());
  let replay = service.open_session_list(search).unwrap();
  assert_eq!(replay.base_cursor.as_ref(), Some(&original.synced_through));
  assert_eq!(replay.synced_through, arrival.synced_through);
  assert!(replay.removed_keys.is_empty());
  assert_eq!(replay.sessions.len(), 1);
  assert_eq!(replay.sessions[0].session.session_key, key);
  assert_eq!(replay.sessions[0].session.title.as_deref(), Some("found needle"));
}

#[test]
fn dropping_an_old_tombstone_requires_a_snapshot_only_for_stale_cutoffs() {
  let a = session_header("a", None, "/project", "2026-01-03T00:00:00Z");
  let b = session_header("b", None, "/project", "2026-01-02T00:00:00Z");
  let c = session_header("c", None, "/project", "2026-01-01T00:00:00Z");
  let service = fixture(vec![a.clone(), b.clone(), c]);
  service.session_lists.limit_tombstones(1);
  let first = service.open_session_list(request("all", 10)).unwrap();
  let b_key = entry_by_id(&first, "b").session.session_key.clone();
  replace(&service, &a, false);
  let after_a = published(&service).pop().unwrap();
  replace(&service, &b, false);
  let after_b = published(&service).pop().unwrap();
  let stale = service
    .open_session_list(resume("stale", 10, &first.synced_through))
    .unwrap();
  assert!(stale.base_cursor.is_none() && stale.removed_keys.is_empty());
  assert_eq!(stale.synced_through, after_b.synced_through);
  assert_eq!(stale.sessions.len(), 1);
  assert_eq!(stale.sessions[0].session.session_id, "c");
  let recent = service
    .open_session_list(resume("recent", 10, &after_a.synced_through))
    .unwrap();
  assert_eq!(recent.base_cursor.as_ref(), Some(&after_a.synced_through));
  assert_eq!(recent.synced_through, after_b.synced_through);
  assert!(recent.sessions.is_empty());
  assert_eq!(recent.removed_keys, [b_key]);
}

#[test]
fn foreign_or_expired_cutoffs_replace_only_the_requested_visible_list() {
  let mut public = session_header("public", None, "/project", "2026-01-02T00:00:00Z");
  public.title = Some("match search".into());
  let private = session_header("private", None, "/project", "2026-01-01T00:00:00Z");
  let headers = vec![public, private];
  let service = fixture(headers.clone());
  let all = service.open_session_list(request("all", 10)).unwrap();
  let public_key = entry_by_id(&all, "public").session.session_key.clone();
  let guest = service.scoped_to_sessions(std::slice::from_ref(&public_key)).unwrap();
  let guest_page = guest
    .open_session_list(resume("share_list_guest:page", 10, &all.synced_through))
    .unwrap();
  assert!(guest_page.base_cursor.is_none() && guest_page.removed_keys.is_empty());
  assert_eq!(guest_page.sessions.len(), 1);
  assert_eq!(guest_page.sessions[0].session.session_key, public_key);

  let mut search = resume("search", 10, &all.synced_through);
  search.query.search = Some("match search".into());
  let matched = service.open_session_list(search).unwrap();
  assert!(matched.base_cursor.is_none() && matched.removed_keys.is_empty());
  assert_eq!(matched.sessions.len(), 1);
  assert_eq!(matched.sessions[0].session.session_key, public_key);

  let restarted = fixture(headers);
  let fresh = restarted
    .open_session_list(resume("all", 10, &all.synced_through))
    .unwrap();
  assert!(fresh.base_cursor.is_none() && fresh.removed_keys.is_empty());
  assert_ne!(fresh.synced_through, all.synced_through);
  assert_eq!(fresh.sessions.len(), 2);
}

#[test]
fn rank_change_sends_one_upsert_and_only_drops_a_key_if_it_leaves_the_page() {
  let a = session_header("a", None, "/project", "2026-01-03T00:00:00Z");
  let b = session_header("b", None, "/project", "2026-01-02T00:00:00Z");
  let mut c = session_header("c", None, "/project", "2026-01-01T00:00:00Z");
  let service = fixture(vec![a, b, c.clone()]);
  let page = service.open_session_list(request("first-page", 2)).unwrap();
  let full = service.open_session_list(request("full", 3)).unwrap();

  c.updated_at_ms = parse_updated_at_ms(Some("2026-01-04T00:00:00Z"));
  replace(&service, &c, true);
  let updates = published(&service);
  assert_eq!(updates.len(), 2);
  for update in updates {
    assert_eq!(update.sessions.len(), 1);
    assert_eq!(update.sessions[0].session.session_id, "c");
    assert_ne!(update.sessions[0].revision, entry_by_id(&full, "c").revision);
    if update.subscription_id == page.subscription_id {
      assert_eq!(
        update.removed_keys,
        [entry_by_id(&page, "b").session.session_key.clone()]
      );
    } else {
      assert_eq!(update.subscription_id, full.subscription_id);
      assert!(update.removed_keys.is_empty());
    }
    let wire = serde_json::to_value(&update).unwrap();
    assert_eq!(wire["synced_through"], update.synced_through);
    assert!(wire["base_cursor"].is_string());
    for field in [
      "rows",
      "order",
      "order_edits",
      "position",
      "moves",
      "generation",
      "base_revision",
      "revision",
    ] {
      assert!(wire.get(field).is_none());
    }
    assert_eq!(
      wire["sessions"][0],
      json!({"session":update.sessions[0].session,"revision":update.sessions[0].revision})
    );
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
  assert_eq!(first.sessions.len(), 1);
  assert_eq!(first.sessions[0].session.session_id, "a");
  replace(&service, &new, true);
  let inserted = published(&service).pop().unwrap();
  assert_eq!(inserted.sessions.len(), 1);
  assert_eq!(inserted.sessions[0].session.session_id, "b");
  replace(&service, &parent, false);
  let removed = published(&service).pop().unwrap();
  assert!(removed.sessions.is_empty());
  assert_eq!(removed.removed_keys.len(), 2);
  assert!(published(&service).is_empty());
}

#[test]
fn live_session_list_offset_matches_static_page_membership() {
  let service = fixture(
    (0..202)
      .map(|i| session_header(&format!("s{i}"), None, "/project", "2026-01-01T00:00:00Z"))
      .collect(),
  );
  let mut deep = request("deep", 1);
  deep.offset = 200;
  let slice = service.open_session_list(deep).unwrap();
  let prefix = service.open_session_list(request("prefix", 201)).unwrap();
  assert_eq!(prefix.sessions.len(), 201);
  let public_page = service
    .list_sessions(ListSessionsRequest {
      limit: Some(200),
      ..Default::default()
    })
    .unwrap();
  let boundary = service
    .list_sessions(ListSessionsRequest {
      cursor: public_page.next_cursor,
      limit: Some(1),
      ..Default::default()
    })
    .unwrap();
  assert_eq!(slice.sessions.len(), 1);
  assert_eq!(slice.sessions[0].session.session_key, boundary.sessions[0].session_key);
  let expected: BTreeSet<_> = public_page
    .sessions
    .iter()
    .chain(&boundary.sessions)
    .map(|session| &session.session_key)
    .collect();
  assert_eq!(
    prefix
      .sessions
      .iter()
      .map(|entry| &entry.session.session_key)
      .collect::<BTreeSet<_>>(),
    expected
  );
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
  let allowed = entry_by_id(&initial, "visible").session.session_key.clone();
  let hidden = entry_by_id(&initial, "private").session.session_key.clone();
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
  let unchanged_grant = guest
    .open_session_list(resume("share_list_guest:test", 2, &grant.synced_through))
    .unwrap();
  assert_eq!(unchanged_grant.synced_through, grant.synced_through);
  assert!(unchanged_grant.sessions.is_empty() && unchanged_grant.removed_keys.is_empty());

  service.session_lists.invalidate(Some(&[allowed]));
  let mut observed = Vec::new();
  service.publish_session_lists(|update| observed.push(update));
  assert_eq!(observed.len(), 1);
  assert_eq!(observed[0].subscription_id, grant.subscription_id);
  assert_eq!(observed[0].sessions[0].session.title.as_deref(), Some("changed"));
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
      updated_session_keys: vec![open.sessions[0].session.session_key.clone()],
      ..Default::default()
    },
  )
  .await;
  let mut changes = Vec::new();
  service.publish_session_lists(|u| changes.push(u));
  assert_eq!(changes.len(), 1);
  assert_eq!(changes[0].sessions[0].session.title.as_deref(), Some("committed"));

  record.title = Some("directory changed".to_owned());
  replace(&service, &record, true);
  crate::runtime::publish_relay_change(
    service.clone(),
    &events,
    crate::relay::RelayChange {
      session_key: Some(open.sessions[0].session.session_key.clone()),
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
  assert_eq!(
    changes[0].sessions[0].session.title.as_deref(),
    Some("directory changed")
  );
}

#[test]
fn many_watchers_of_one_list_use_one_index_read_and_one_broadcast_per_change() {
  let mut first = session_header("one", None, "/project", "2026-01-01T00:00:00Z");
  let second = session_header("two", None, "/project", "2026-01-01T00:00:00Z");
  let service = fixture(vec![first.clone(), second]);
  // More than the number of slots in the runtime event broadcast channel.
  let viewers: BTreeSet<_> = (0..150).map(|index| format!("viewer-{index}")).collect();
  let reads = service.session_lists.read_count();
  for id in &viewers {
    service.open_session_list(request(id, 10)).unwrap();
  }
  assert_eq!(service.session_lists.read_count() - reads, 1);

  first.title = Some("changed".to_owned());
  replace(&service, &first, true);
  service.session_lists.invalidate(None);
  let mut batches = 0;
  service.publish_session_list_batches(|update, routes, local| {
    batches += 1;
    assert!(!local);
    assert!(update.subscription_id.is_empty());
    assert_eq!(routes.into_iter().collect::<BTreeSet<_>>(), viewers);
    assert_eq!(update.sessions.len(), 1);
    assert_eq!(update.sessions[0].session.title.as_deref(), Some("changed"));
  });
  assert_eq!(batches, 1);
  assert_eq!(service.session_lists.read_count() - reads, 2);
}

#[test]
fn native_and_network_routes_share_one_page_and_deliver_only_on_their_own_adapter() {
  let mut session = session_header("one", None, "/project", "2026-01-01T00:00:00Z");
  let service = fixture(vec![session.clone()]);
  let native = service.open_local_session_list(request("screen:native", 10)).unwrap();
  let http = service.open_session_list(request("screen:browser", 10)).unwrap();
  assert_eq!(native.synced_through, http.synced_through);
  assert!(service.open_session_list(request("screen:native", 10)).is_err());
  assert!(service.open_local_session_list(request("screen:browser", 10)).is_err());

  session.title = Some("renamed".to_owned());
  replace(&service, &session, true);
  service.session_lists.invalidate(None);
  let mut deliveries = BTreeSet::new();
  let reads = service.session_lists.read_count();
  service.publish_session_list_batches(|update, routes, local| {
    assert_eq!(routes.len(), 1);
    assert_eq!(update.base_cursor.as_deref(), Some(native.synced_through.as_str()));
    deliveries.insert((routes[0].clone(), local));
  });
  assert_eq!(service.session_lists.read_count() - reads, 1);
  assert_eq!(
    deliveries,
    BTreeSet::from([("screen:native".into(), true), ("screen:browser".into(), false)])
  );
}
