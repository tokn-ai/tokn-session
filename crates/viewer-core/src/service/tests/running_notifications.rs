use super::*;

#[test]
fn stopping_a_child_refreshes_collapsed_ancestors_and_preserves_running_siblings() {
  for sibling_running in [false, true] {
    let directory = tempfile::tempdir().unwrap();
    let specs = [
      ("grandparent", None, true),
      ("parent", Some("grandparent"), true),
      ("child", Some("parent"), true),
      ("sibling", Some("parent"), sibling_running),
    ]
    .into_iter()
    .map(|(id, parent, running)| {
      let path = directory.path().join(format!("{id}.jsonl"));
      std::fs::write(&path, "baseline").unwrap();
      IndexedLoadSpec {
        header: indexed_header(path, id, parent),
        messages: vec![indexed_message(
          Role::Assistant,
          if running {
            MessageDelivery::Commentary
          } else {
            MessageDelivery::Final
          },
        )],
      }
    })
    .collect::<Vec<_>>();
    let child = specs[2].header.clone();
    let child_locator = locator_for_header(ViewerProvider::Codex, &child);
    let child_key = encode_session_key(&child_locator).unwrap();
    let parent_key = encode_session_key(&locator_for_header(ViewerProvider::Codex, &specs[1].header)).unwrap();
    let grandparent_key = encode_session_key(&locator_for_header(ViewerProvider::Codex, &specs[0].header)).unwrap();
    let repository = indexing_repository(specs);
    let service = ViewerService::new_with_index(repository.clone(), Arc::new(SessionIndex::open_in_memory().unwrap()));
    service.refresh_session_index().unwrap();
    let before = service.session_notifications(&[child_key.clone()]);
    assert!(
      before
        .iter()
        .any(|row| row["session_key"] == parent_key && row["has_running_descendant"] == true)
    );

    std::fs::write(&child.path, "completed child source").unwrap();
    repository.loads.lock().unwrap().insert(
      child_locator,
      Ok(IndexedLoadSpec {
        header: child,
        messages: vec![indexed_message(Role::Assistant, MessageDelivery::Final)],
      }),
    );
    service.refresh_session_catalog().unwrap();
    let refresh = service.refresh_pending_session_index().unwrap();
    assert!(
      !refresh.catalog_refresh_required,
      "body notifications should avoid a full catalog read"
    );
    let notifications = service.session_notifications(&[child_key.clone()]);
    assert_eq!(notifications.len(), 3, "only the child and its ancestor chain change");
    for key in [&parent_key, &grandparent_key] {
      let row = notifications.iter().find(|row| row["session_key"] == *key).unwrap();
      assert_eq!(row["is_running"], true);
      assert_eq!(
        row["has_running_descendant"],
        sibling_running || *key == grandparent_key
      );
      assert_eq!(row["has_unread"], false, "child unread replies never propagate");
    }
    let row = notifications
      .iter()
      .find(|row| row["session_key"] == child_key)
      .unwrap();
    assert_eq!(row["is_running"], false);
    assert_eq!(row["has_running_descendant"], false);
    assert_eq!(row["unread_final_count"], 1);

    let shared = service.scoped_to_sessions(&[child_key.clone()]).unwrap();
    let shared_notifications = shared.session_notifications(&[child_key.clone(), parent_key]);
    assert_eq!(
      shared_notifications.len(),
      1,
      "a scoped reader cannot receive unshared ancestors"
    );
    assert_eq!(shared_notifications[0]["session_key"], child_key);
    assert_eq!(shared_notifications[0]["has_unread"], false);
  }
}

#[test]
fn stopping_parent_clears_descendant_running_without_child_completion_records() {
  let directory = tempfile::tempdir().unwrap();
  let specs = [
    ("parent", None),
    ("child", Some("parent")),
    ("grandchild", Some("child")),
  ]
  .into_iter()
  .map(|(id, parent)| {
    let path = directory.path().join(format!("{id}.jsonl"));
    std::fs::write(&path, "active baseline").unwrap();
    IndexedLoadSpec {
      header: indexed_header(path, id, parent),
      messages: vec![indexed_message(Role::Assistant, MessageDelivery::Commentary)],
    }
  })
  .collect::<Vec<_>>();
  let parent = specs[0].header.clone();
  let locator = locator_for_header(ViewerProvider::Codex, &parent);
  let key = encode_session_key(&locator).unwrap();
  let repository = indexing_repository(specs);
  let service = ViewerService::new_with_index(repository.clone(), Arc::new(SessionIndex::open_in_memory().unwrap()));
  service.refresh_session_index().unwrap();
  let before = service.list_sessions(ListSessionsRequest::default()).unwrap();
  assert!(before.sessions[0].is_running);
  assert!(before.sessions[0].has_running_descendant);
  std::fs::write(&parent.path, "parent completed, child files unchanged").unwrap();
  repository.loads.lock().unwrap().insert(
    locator.clone(),
    Ok(IndexedLoadSpec {
      header: parent.clone(),
      messages: vec![
        indexed_message(Role::Assistant, MessageDelivery::Commentary),
        indexed_message(Role::Assistant, MessageDelivery::Final),
      ],
    }),
  );
  service.refresh_session_index().unwrap();
  let after = service.list_sessions(ListSessionsRequest::default()).unwrap();
  assert!(!after.sessions[0].is_running);
  assert!(!after.sessions[0].has_running_descendant);
  let notifications = service.session_notifications(&[key.clone()]);
  let parent_notification = notifications.iter().find(|row| row["session_key"] == key).unwrap();
  assert_eq!(parent_notification["is_running"], false);
  assert_eq!(parent_notification["has_running_descendant"], false);
  // Even later activity notifications from an unfinished child cannot revive
  // the parent. Resuming the parent is what makes delegated work active again.
  let child = service
    .list_session_children(ListSessionChildrenRequest {
      parent_session_key: key.clone(),
      cursor: None,
      offset: None,
      limit: None,
    })
    .unwrap();
  let later = service.session_notifications(&[child.sessions[0].session_key.clone()]);
  assert_eq!(
    later.iter().find(|row| row["session_key"] == key).unwrap()["has_running_descendant"],
    false
  );
  repository.loads.lock().unwrap().insert(
    locator,
    Ok(IndexedLoadSpec {
      header: parent.clone(),
      messages: vec![
        indexed_message(Role::Assistant, MessageDelivery::Final),
        indexed_message(Role::Assistant, MessageDelivery::Commentary),
      ],
    }),
  );
  std::fs::write(&parent.path, "parent resumes delegated work again").unwrap();
  service.refresh_session_index().unwrap();
  let resumed = service.list_sessions(ListSessionsRequest::default()).unwrap();
  assert!(resumed.sessions[0].is_running);
  assert!(resumed.sessions[0].has_running_descendant);
}
