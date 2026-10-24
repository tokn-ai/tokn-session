use super::*;

#[test]
fn codex_defaults_to_rpc_and_explicit_desktop_persists() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("session-controls.json");
  let store = controls::ControlStore::new(Some(path.clone())).unwrap();
  assert_eq!(store.transport("codex:one").unwrap(), SessionTransport::Rpc);
  assert!(!path.exists(), "inspection does not save settings");
  store.set_transport("codex:one", SessionTransport::Desktop).unwrap();
  let reopened = controls::ControlStore::new(Some(path.clone())).unwrap();
  assert_eq!(reopened.transport("codex:one").unwrap(), SessionTransport::Desktop);
  assert_eq!(reopened.transport("codex:two").unwrap(), SessionTransport::Rpc);
  reopened.set_transport("codex:one", SessionTransport::Rpc).unwrap();
  assert_eq!(store.transport("codex:one").unwrap(), SessionTransport::Rpc);
}

#[test]
fn newer_transport_storage_is_preserved() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("session-controls.json");
  let saved = "{\"version\":2,\"sessions\":{}}";
  std::fs::write(&path, saved).unwrap();
  assert!(
    controls::ControlStore::new(Some(path.clone()))
      .unwrap()
      .transport("codex:one")
      .is_err()
  );
  assert_eq!(std::fs::read_to_string(path).unwrap(), saved);
}

#[test]
fn uncertainty_remains_distinct_from_rejection() {
  let response = input_response("one".into(), Err(DeliveryError::uncertain("pipe closed after write")));
  assert_eq!(response.status, crate::model::InputDeliveryStatus::Unknown);
  let response = input_response("two".into(), Err(DeliveryError::not_sent("owner changed")));
  assert_eq!(response.status, crate::model::InputDeliveryStatus::NotSent);
}

#[tokio::test]
async fn transport_retries_cannot_undo_a_newer_choice_or_bypass_another_owner() {
  let directory = tempfile::tempdir().unwrap();
  let supervisor = SessionSupervisor::new(Some(directory.path().into()), InteractionService::default()).unwrap();
  let target = VerifiedSession {
    session_key: "verified-session".into(),
    input: InputTarget {
      provider: ViewerProvider::Codex,
      session_id: "native-thread".into(),
      session_file: directory.path().join("history.jsonl"),
    },
    cwd: directory.path().into(),
  };
  let broker = input::InputBroker::default();
  let desktop = SetSessionControlRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    session_key: target.session_key.clone(),
    transport: SessionTransport::Desktop,
  };
  let owner = supervisor
    .ownership
    .acquire(Provider::Codex, &target.input.session_id)
    .unwrap();
  assert!(supervisor.set_transport(&target, &desktop, &broker).await.is_err());
  assert_eq!(supervisor.transport(&target).await.unwrap(), SessionTransport::Rpc);
  drop(owner);
  let desktop = SetSessionControlRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    ..desktop
  };
  supervisor.set_transport(&target, &desktop, &broker).await.unwrap();
  let rpc = SetSessionControlRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    transport: SessionTransport::Rpc,
    ..desktop.clone()
  };
  supervisor.set_transport(&target, &rpc, &broker).await.unwrap();
  supervisor.set_transport(&target, &desktop, &broker).await.unwrap();
  assert_eq!(
    supervisor.transport(&target).await.unwrap(),
    SessionTransport::Rpc,
    "old receipt must not reapply a previous choice"
  );
  let conflicting = SetSessionControlRequest {
    transport: SessionTransport::Desktop,
    ..rpc
  };
  assert!(supervisor.set_transport(&target, &conflicting, &broker).await.is_err());
  supervisor.shutdown().await;
}

#[tokio::test]
async fn transport_transition_excludes_both_message_and_control_admission() {
  let target = InputTarget {
    provider: ViewerProvider::Codex,
    session_id: "transition-thread".into(),
    session_file: "/history.jsonl".into(),
  };
  let broker = input::InputBroker::default();
  let transition = broker.begin_transition(&target).unwrap();
  let request = SubmitSessionInputRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    session_key: "key".into(),
    text: "input".into(),
    image_ids: Vec::new(),
    model_selection: None,
  };
  assert_eq!(
    broker
      .submit_custom(&target, request.clone(), async {
        panic!("message entered a switching transport")
      })
      .await
      .status,
    crate::model::InputDeliveryStatus::NotSent
  );
  let control = SubmitSessionInputRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    ..request
  };
  assert_eq!(
    broker
      .submit_control(&target, control.clone(), async {
        panic!("control entered after the idle check")
      })
      .await
      .status,
    crate::model::InputDeliveryStatus::NotSent
  );
  drop(transition);
  assert_eq!(
    broker
      .submit_control(&target, control, async { Ok(Admission::Accepted) })
      .await
      .status,
    crate::model::InputDeliveryStatus::Accepted
  );
}

#[tokio::test]
async fn desktop_question_delivery_checks_current_route_and_canonical_owner() {
  let directory = tempfile::tempdir().unwrap();
  let supervisor = SessionSupervisor::new(Some(directory.path().into()), InteractionService::default()).unwrap();
  let target = VerifiedSession {
    session_key: "verified-session".into(),
    input: InputTarget {
      provider: ViewerProvider::Codex,
      session_id: "question-thread".into(),
      session_file: directory.path().join("history.jsonl"),
    },
    cwd: directory.path().into(),
  };
  let error = supervisor
    .submit_desktop_question(&target, "answer", "text")
    .await
    .unwrap_err();
  assert!(error.message.contains("route changed"));
  assert!(supervisor.desktop_owners.lock().unwrap().is_empty());
  supervisor
    .controls
    .set_transport(&target.identity(), SessionTransport::Desktop)
    .unwrap();
  let owner = supervisor
    .ownership
    .acquire(Provider::Codex, &target.input.session_id)
    .unwrap();
  assert!(
    supervisor
      .submit_desktop_question(&target, "answer", "text")
      .await
      .is_err()
  );
  assert!(
    supervisor.desktop_owners.lock().unwrap().is_empty(),
    "question bypassed the canonical session lease"
  );
  drop(owner);
  let guard = supervisor.gate(&target).await;
  let waiting = supervisor.clone();
  let waiting_target = VerifiedSession {
    session_key: target.session_key.clone(),
    input: target.input.clone(),
    cwd: target.cwd.clone(),
  };
  let answer = tokio::spawn(async move { waiting.submit_desktop_question(&waiting_target, "answer", "text").await });
  supervisor
    .controls
    .set_transport(&target.identity(), SessionTransport::Rpc)
    .unwrap();
  drop(guard);
  assert!(answer.await.unwrap().unwrap_err().message.contains("route changed"));
  assert!(supervisor.desktop_owners.lock().unwrap().is_empty());
  supervisor.shutdown().await;
}
