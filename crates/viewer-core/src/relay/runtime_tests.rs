use super::*;
use crate::runtimes::{RuntimeExecutionKind, RuntimeKind, RuntimeOwnership};
use tokn_session_index::{SessionIndex, SessionKey, SessionMetadata, SourceReplacement, SourceState};

#[tokio::test]
async fn embedded_reader_restart_replaces_its_instance_and_rejects_stale_actions() {
  for mode in [RelayMode::Automatic, RelayMode::Local] {
    let relay = ViewerRelay::with_index(Some(Arc::new(SessionIndex::open_in_memory().unwrap())));
    let settings = RelaySettings {
      mode,
      ..Default::default()
    };
    relay.configure(settings.clone()).unwrap();
    let previous = relay.runtime_info();
    assert_eq!(previous.kind, RuntimeKind::Reader);
    assert_eq!(previous.execution_kind, RuntimeExecutionKind::Embedded);
    assert_eq!(previous.ownership, RuntimeOwnership::Viewer);
    assert!(previous.pid.is_none());
    assert!(previous.can_restart);
    assert!(previous.restart_disabled_reason.is_none());
    let old_service = {
      let state = relay.state.lock().unwrap();
      match &state.connection {
        Some(Connection::Embedded(service)) => service.clone(),
        _ => panic!("indexed reader did not own an embedded service"),
      }
    };

    relay.restart_runtime(&previous.instance_id).await.unwrap();
    let current = relay.runtime_info();
    assert_eq!(current.runtime_id, previous.runtime_id);
    assert_ne!(current.instance_id, previous.instance_id);
    assert_eq!(relay.status().settings, settings);
    assert!(current.pid.is_none());
    assert!(current.can_restart);
    {
      let state = relay.state.lock().unwrap();
      match &state.connection {
        Some(Connection::Embedded(service)) => assert!(!Arc::ptr_eq(&old_service, service)),
        _ => panic!("restarting an embedded reader changed its execution kind"),
      }
    }
    assert!(relay.restart_runtime(&previous.instance_id).await.is_err());
    assert_eq!(relay.runtime_info().instance_id, current.instance_id);
    relay.state.lock().unwrap().cancel.cancel();
  }
}

#[tokio::test]
async fn indexed_automatic_restart_preserves_displayed_history_and_recovers_source_follow() {
  use std::io::Write;

  let root = tempfile::tempdir().unwrap();
  let path = root.path().join("session.jsonl");
  let mut history =
    "{\"type\":\"session\",\"id\":\"reader-restart\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n".to_string();
  for turn in 0..7 {
    history.push_str(&format!(
      "{{\"type\":\"message\",\"id\":\"user-{turn}\",\"message\":{{\"role\":\"user\",\"content\":\"before restart {turn}\"}}}}\n"
    ));
    history.push_str(&format!(
      "{{\"type\":\"message\",\"id\":\"assistant-{turn}\",\"message\":{{\"role\":\"assistant\",\"content\":\"reply {turn}\"}}}}\n"
    ));
  }
  std::fs::write(&path, history).unwrap();
  let index = Arc::new(SessionIndex::open_in_memory().unwrap());
  let source = crate::service::index_source_key_for_path(ViewerProvider::Pi, &path).unwrap();
  index
    .replace_sources(&[
      SourceReplacement::new(
        SourceState::new(source.clone(), "fixture", 0),
        vec![SessionMetadata::new(
          SessionKey::new("pi", &source.source_key, "reader-restart"),
          path.to_str().unwrap(),
        )],
      ),
      SourceReplacement::new(
        SourceState::new(
          crate::service::index_catalog_source_key(ViewerProvider::Pi),
          "fixture",
          0,
        ),
        vec![],
      ),
    ])
    .unwrap();
  let relay = ViewerRelay::with_index(Some(index));
  relay
    .configure(RelaySettings {
      mode: RelayMode::Automatic,
      include_native: true,
      ..Default::default()
    })
    .unwrap();
  let locator = SessionLocator {
    version: 1,
    provider: ViewerProvider::Pi,
    session_id: "reader-restart".into(),
    source_path: path.clone(),
  };
  let loader = relay.clone();
  let target = locator.clone();
  let initial = tokio::task::spawn_blocking(move || loader.load(&target))
    .await
    .unwrap()
    .unwrap();
  let user_count = |loaded: &LoadedSession| {
    loaded
      .events
      .iter()
      .filter(|event| {
        matches!(event, tokn_session_core::AgentEvent::Message(message) if message.role == tokn_session_core::Role::User)
      })
      .count()
  };
  assert_eq!(user_count(&initial), 3);
  let initial_window = relay.window_info(&locator, &initial).unwrap();
  assert!(initial_window.has_earlier);
  let loader = relay.clone();
  let target = locator.clone();
  let pinned = tokio::task::spawn_blocking(move || loader.load_earlier(&target, initial_window.event_offset))
    .await
    .unwrap()
    .unwrap();
  assert_eq!(user_count(&pinned), 6);
  let expanded_window = relay.window_info(&locator, &pinned).unwrap();
  assert!(expanded_window.event_offset < initial_window.event_offset);
  let native = relay.native(&locator, 0, &pinned).unwrap();
  let previous = relay.runtime_info();

  relay.restart_runtime(&previous.instance_id).await.unwrap();
  {
    let state = relay.state.lock().unwrap();
    let displayed = state
      .sessions
      .get(&locator)
      .and_then(|session| session.displayed.as_ref());
    assert!(
      displayed.is_some_and(|displayed| Arc::ptr_eq(displayed, &pinned)),
      "restarting the same indexed reader must retain the displayed conversation"
    );
  }
  let retained_window = relay.window_info(&locator, &pinned).unwrap();
  assert_eq!(retained_window.event_offset, expanded_window.event_offset);
  assert_eq!(retained_window.has_earlier, expanded_window.has_earlier);
  assert_eq!(retained_window.generation, expanded_window.generation);
  assert_eq!(retained_window.revision, expanded_window.revision);
  assert_eq!(relay.native(&locator, 0, &pinned), Some(native.clone()));
  // Append before any post-restart load: replacement must restore the source
  // follow without requiring the conversation to be reopened.
  let mut updates = relay.changes.subscribe();
  writeln!(
    std::fs::OpenOptions::new().append(true).open(&path).unwrap(),
    "{{\"type\":\"message\",\"id\":\"two\",\"message\":{{\"role\":\"user\",\"content\":\"after restart\"}}}}"
  )
  .unwrap();
  relay.wake_sources().await;
  tokio::time::timeout(Duration::from_secs(4), async {
    loop {
      let updated = relay
        .state
        .lock()
        .unwrap()
        .sessions
        .get(&locator)
        .and_then(|session| session.loaded.as_ref())
        .is_some_and(|loaded| loaded.events.len() > pinned.events.len());
      if updated {
        break;
      }
      updates.recv().await.unwrap();
    }
  })
  .await
  .expect("the replacement reader must follow source appends");
  assert!(Arc::ptr_eq(&pinned, &relay.load(&locator).unwrap()));
  assert_eq!(relay.native(&locator, 0, &pinned), Some(native));
  let latest = relay.advance(&locator).unwrap();
  assert_eq!(latest.events.len(), pinned.events.len() + 1);
  assert_eq!(user_count(&latest), 7);
  assert_eq!(
    relay.window_info(&locator, &latest).unwrap().event_offset,
    expanded_window.event_offset,
    "the replacement follower must retain the expanded older-history range"
  );
  let events = serde_json::to_string(&latest.events).unwrap();
  assert!(events.contains("before restart 1"));
  assert!(events.contains("after restart"));
  assert!(relay.native(&locator, 0, &latest).is_some());
  relay.shutdown().await;
}

#[tokio::test]
async fn external_reader_inventory_cannot_restart_or_reconfigure_another_service() {
  let relay = ViewerRelay::new();
  let settings = RelaySettings {
    mode: RelayMode::External,
    endpoint: "tcp://127.0.0.1:1".into(),
    ..Default::default()
  };
  relay.configure(settings.clone()).unwrap();
  let row = relay.runtime_info();
  assert_eq!(row.kind, RuntimeKind::Reader);
  assert_eq!(row.execution_kind, RuntimeExecutionKind::External);
  assert_eq!(row.ownership, RuntimeOwnership::External);
  assert!(row.pid.is_none());
  assert!(!row.can_restart);
  assert!(row.restart_disabled_reason.is_some());
  assert_eq!(row.endpoint.as_deref(), Some(settings.endpoint.as_str()));
  assert!(relay.restart_runtime(&row.instance_id).await.is_err());
  assert_eq!(relay.status().settings, settings);
  assert_eq!(relay.runtime_info().instance_id, row.instance_id);
  relay.state.lock().unwrap().cancel.cancel();
}

#[tokio::test]
async fn unindexed_local_reader_does_not_offer_restart_without_an_owned_worker() {
  let relay = ViewerRelay::new();
  let settings = RelaySettings {
    mode: RelayMode::Local,
    ..Default::default()
  };
  relay.configure(settings.clone()).unwrap();
  let row = relay.runtime_info();
  assert!(row.pid.is_none());
  assert!(!row.can_restart);
  assert!(row.restart_disabled_reason.is_some());
  assert!(relay.restart_runtime(&row.instance_id).await.is_err());
  assert_eq!(relay.runtime_info().instance_id, row.instance_id);
  assert_eq!(relay.status().settings, settings);
  relay.state.lock().unwrap().cancel.cancel();
}
