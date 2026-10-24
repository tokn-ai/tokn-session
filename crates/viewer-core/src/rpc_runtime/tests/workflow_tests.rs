use super::*;

#[tokio::test]
async fn ensure_coalesces_native_resume_and_message_receipts_deliver_once() {
  let peer = CodexPeer::start(false).await;
  let manager = manager();
  save_profile(&manager, attached_profile(peer.endpoint.clone()));
  assert!(manager.binding_snapshot("catalog").is_none());
  assert_eq!(peer.connections.load(Ordering::Relaxed), 0);
  let first_id = uuid::Uuid::new_v4().to_string();
  let second_id = uuid::Uuid::new_v4().to_string();
  let (first, second) = tokio::join!(
    manager.ensure_session(&first_id, "catalog", Provider::Codex, "native", Path::new("/tmp")),
    manager.ensure_session(&second_id, "catalog", Provider::Codex, "native", Path::new("/tmp")),
  );
  let first = first.unwrap();
  let second = second.unwrap();
  assert_eq!(first.target, second.target);
  assert_eq!(peer.connections.load(Ordering::Relaxed), 1);
  assert_eq!(
    peer
      .methods()
      .iter()
      .filter(|method| method.as_str() == "thread/resume")
      .count(),
    1
  );
  assert_eq!(
    manager
      .ensure_session(&first_id, "catalog", Provider::Codex, "native", Path::new("/tmp"))
      .await
      .unwrap()
      .target,
    first.target
  );
  let send_id = uuid::Uuid::new_v4().to_string();
  let command = Command::Message {
    input: MessageInput {
      text: "single delivery".into(),
      ..MessageInput::default()
    },
    delivery: DeliveryMode::StartOrSteer,
  };
  assert!(matches!(
    manager
      .execute_binding(&first, send_id.clone(), command.clone())
      .await
      .unwrap(),
    Reply::Admission(_)
  ));
  assert!(matches!(
    manager.execute_binding(&first, send_id, command).await.unwrap(),
    Reply::Admission(_)
  ));
  assert_eq!(
    peer
      .methods()
      .iter()
      .filter(|method| method.as_str() == "turn/start")
      .count(),
    1
  );
  manager.shutdown().await;
}

#[tokio::test]
async fn one_runtime_can_bind_multiple_sessions_and_idle_release_fences_the_old_target() {
  let peer = CodexPeer::start(false).await;
  let manager = manager();
  save_profile(&manager, attached_profile(peer.endpoint.clone()));
  let first = manager
    .ensure_session(
      &uuid::Uuid::new_v4().to_string(),
      "first-key",
      Provider::Codex,
      "first-native",
      Path::new("/tmp"),
    )
    .await
    .unwrap();
  let second = manager
    .ensure_session(
      &uuid::Uuid::new_v4().to_string(),
      "second-key",
      Provider::Codex,
      "second-native",
      Path::new("/tmp"),
    )
    .await
    .unwrap();
  assert_eq!(first.target.runtime_id, second.target.runtime_id);
  assert_eq!(first.target.instance_id, second.target.instance_id);
  assert_ne!(first.target.session_id, second.target.session_id);
  assert_eq!(peer.connections.load(Ordering::Relaxed), 1);
  assert_eq!(manager.inventory().runtimes[0].session_count, 2);
  assert!(manager.binding_snapshot("first-key").is_some());
  manager.release_binding("first-key").await.unwrap();
  assert!(manager.binding("first-key").is_none());
  assert!(manager.binding("second-key").is_some());
  assert_eq!(manager.inventory().runtimes[0].session_count, 1);
  assert_eq!(
    manager
      .execute_binding(&first, uuid::Uuid::new_v4().to_string(), Command::State)
      .await
      .unwrap_err()
      .kind,
    ErrorKind::StaleTarget
  );
  assert!(matches!(
    manager
      .execute_binding(&second, uuid::Uuid::new_v4().to_string(), Command::Models)
      .await
      .unwrap(),
    Reply::Models(_)
  ));
  manager.shutdown().await;
}

#[tokio::test]
async fn canonical_native_lease_blocks_another_profile_and_process_until_idle_release() {
  let first_peer = CodexPeer::start(false).await;
  let second_peer = CodexPeer::start(false).await;
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("profiles.json");
  let first = RpcRuntimeManager::new(Some(path.clone()), InteractionService::default());
  let second = RpcRuntimeManager::new(Some(path), InteractionService::default());
  let mut first_request = save_profile(&first, attached_profile(first_peer.endpoint.clone()));
  let second_request = save_profile(&first, attached_profile(second_peer.endpoint.clone()));
  first_request.expected_revision = second_request.expected_revision;
  let info = first.start(first_request.clone()).await.unwrap();
  first
    .bind(
      uuid::Uuid::new_v4().to_string(),
      "original".into(),
      first_request.profile_id,
      info.instance_id,
      "same-native".into(),
      Provider::Codex,
    )
    .await
    .unwrap();
  assert!(
    second
      .ensure_session(
        &uuid::Uuid::new_v4().to_string(),
        "alias",
        Provider::Codex,
        "same-native",
        Path::new("/tmp")
      )
      .await
      .is_err()
  );
  assert!(!second_peer.methods().iter().any(|method| method == "thread/resume"));
  first.release_binding("original").await.unwrap();
  let replacement = second
    .ensure_session(
      &uuid::Uuid::new_v4().to_string(),
      "alias",
      Provider::Codex,
      "same-native",
      Path::new("/tmp"),
    )
    .await
    .unwrap();
  assert_eq!(replacement.profile_id, second_request.profile_id);
  assert_eq!(
    second_peer
      .methods()
      .iter()
      .filter(|method| method.as_str() == "thread/resume")
      .count(),
    1
  );
  first.shutdown().await;
  second.shutdown().await;
}

#[tokio::test]
async fn ambiguous_runtime_profiles_require_a_default_without_starting_a_server() {
  let peer = CodexPeer::start(false).await;
  let manager = manager();
  let mut input = attached_profile(peer.endpoint.clone());
  input.is_default = false;
  save_profile(&manager, input.clone());
  save_profile(&manager, input);
  let error = manager
    .ensure_session(
      &uuid::Uuid::new_v4().to_string(),
      "key",
      Provider::Codex,
      "native",
      Path::new("/tmp"),
    )
    .await
    .unwrap_err();
  assert!(error.contains("default"));
  assert_eq!(peer.connections.load(Ordering::Relaxed), 0);
  assert!(manager.inventory().runtimes.is_empty());
  manager.shutdown().await;
}

#[tokio::test]
async fn running_session_cannot_release_its_canonical_owner_until_native_state_is_idle() {
  let peer = CodexPeer::start(false).await;
  let manager = manager();
  save_profile(&manager, attached_profile(peer.endpoint.clone()));
  let binding = manager
    .ensure_session(
      &uuid::Uuid::new_v4().to_string(),
      "active-key",
      Provider::Codex,
      "native",
      Path::new("/tmp"),
    )
    .await
    .unwrap();
  manager
    .execute_binding(
      &binding,
      uuid::Uuid::new_v4().to_string(),
      Command::Message {
        input: MessageInput {
          text: "start".into(),
          ..MessageInput::default()
        },
        delivery: DeliveryMode::StartOrSteer,
      },
    )
    .await
    .unwrap();
  assert!(manager.release_binding("active-key").await.is_err());
  assert!(manager.binding("active-key").is_some());
  peer.active.store(false, Ordering::Relaxed);
  manager.release_binding("active-key").await.unwrap();
  assert!(manager.binding("active-key").is_none());
  manager.shutdown().await;
}

#[tokio::test]
async fn activation_readiness_reports_profile_constraints_without_connecting() {
  let peer = CodexPeer::start(false).await;
  let manager = manager();
  assert!(
    manager
      .activation_ready(Provider::Dsh, Path::new("/tmp"))
      .await
      .is_err()
  );
  assert!(
    manager
      .activation_ready(Provider::Codex, Path::new("relative"))
      .await
      .is_err()
  );
  assert!(manager.activation_ready(Provider::Pi, Path::new("/tmp")).await.is_err());
  let mut profile = attached_profile(peer.endpoint.clone());
  profile.is_default = false;
  save_profile(&manager, profile.clone());
  assert!(
    manager
      .activation_ready(Provider::Codex, Path::new("/tmp"))
      .await
      .is_ok()
  );
  save_profile(&manager, profile);
  assert!(
    manager
      .activation_ready(Provider::Codex, Path::new("/tmp"))
      .await
      .unwrap_err()
      .contains("default")
  );
  assert_eq!(peer.connections.load(Ordering::Relaxed), 0);
  assert!(manager.inventory().runtimes.is_empty());
  manager.shutdown().await;
}
