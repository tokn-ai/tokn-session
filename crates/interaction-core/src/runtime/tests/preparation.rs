use super::*;

async fn selected_creation(runtime: &RuntimeCoordinator<FakeHost>) -> CreatedNativeSession {
  runtime
    .create_native(
      Provider::Codex,
      std::env::temp_dir(),
      None,
      Some(uuid::Uuid::new_v4().to_string()),
      Some(test_model()),
    )
    .await
    .unwrap()
}

fn first_message() -> Command {
  Command::Message {
    input: MessageInput {
      text: "first message".into(),
      ..Default::default()
    },
    delivery: DeliveryMode::StartOrSteer,
  }
}

#[tokio::test]
async fn created_model_preparation_is_once_for_the_original_intent_and_precedes_input() {
  let (runtime, shared) = fixture();
  let created = selected_creation(&runtime).await;
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  let mut different = test_model();
  different.model_id = "another-model".into();
  assert_eq!(
    runtime
      .prepare_created_model(&created, different)
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Conflict
  );
  assert!(adapter.prepared_models.lock().unwrap().is_empty());
  runtime.prepare_created_model(&created, test_model()).await.unwrap();
  let mut relabeled = test_model();
  relabeled.display_name = Some("A different display label".into());
  runtime.prepare_created_model(&created, relabeled).await.unwrap();
  assert_eq!(
    adapter.prepared_models.lock().unwrap().as_slice(),
    &[creation_model_identity(&test_model())]
  );
  assert_eq!(adapter.creates.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
  assert!(!created.capabilities.atomic_model_on_send);
  runtime
    .execute_created(&created, uuid::Uuid::new_v4().to_string(), first_message())
    .await
    .unwrap();
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 1);
  assert_eq!(
    runtime
      .prepare_created_model(&created, test_model())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::InvalidRequest
  );
  assert_eq!(adapter.prepared_models.lock().unwrap().len(), 1);
  runtime.shutdown().await;
}

#[tokio::test]
async fn preparation_rejects_unknown_intent_stale_instances_and_already_published_sessions() {
  let (runtime, shared) = fixture();
  let default = runtime
    .create_native(Provider::Codex, std::env::temp_dir(), None, None, None)
    .await
    .unwrap();
  assert_eq!(
    runtime
      .prepare_created_model(&default, test_model())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::InvalidRequest
  );
  let created = selected_creation(&runtime).await;
  let mut stale = created.clone();
  stale.target.instance_id = uuid::Uuid::new_v4().to_string();
  assert_eq!(
    runtime
      .prepare_created_model(&stale, test_model())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::StaleTarget
  );
  runtime
    .adopt_created(&created, "published-creation".into())
    .await
    .unwrap();
  assert_eq!(
    runtime
      .prepare_created_model(&created, test_model())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::InvalidRequest
  );
  assert!(
    shared
      .adapter
      .lock()
      .unwrap()
      .as_ref()
      .unwrap()
      .prepared_models
      .lock()
      .unwrap()
      .is_empty()
  );
  runtime.shutdown().await;
}

#[tokio::test]
async fn preparation_requires_idle_state_and_retains_the_rejection_without_retrying() {
  let (runtime, shared) = fixture();
  let created = selected_creation(&runtime).await;
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  adapter.running.store(true, Ordering::SeqCst);
  let first = runtime.prepare_created_model(&created, test_model()).await.unwrap_err();
  assert_eq!(first.kind, ErrorKind::InvalidRequest);
  adapter.running.store(false, Ordering::SeqCst);
  assert_eq!(
    runtime.prepare_created_model(&created, test_model()).await.unwrap_err(),
    first
  );
  assert!(adapter.prepared_models.lock().unwrap().is_empty());
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
  assert_eq!(
    runtime
      .execute_created(&created, uuid::Uuid::new_v4().to_string(), first_message())
      .await
      .unwrap_err(),
    first
  );
  runtime.shutdown().await;
}

#[tokio::test]
async fn rejected_or_uncertain_preparation_preserves_identity_and_never_repeats_a_native_write() {
  for error in [
    InteractionError::new(ErrorKind::Rejected, "Model rejected"),
    InteractionError::uncertain("Model acknowledgement lost"),
  ] {
    let (runtime, shared) = fixture();
    let created = selected_creation(&runtime).await;
    let adapter = shared.adapter.lock().unwrap().clone().unwrap();
    *adapter.prepare_error.lock().unwrap() = Some(error.clone());
    assert_eq!(
      runtime.prepare_created_model(&created, test_model()).await.unwrap_err(),
      error
    );
    *adapter.prepare_error.lock().unwrap() = None;
    assert_eq!(
      runtime.prepare_created_model(&created, test_model()).await.unwrap_err(),
      error
    );
    assert_eq!(
      runtime
        .execute_created(&created, uuid::Uuid::new_v4().to_string(), first_message())
        .await
        .unwrap_err(),
      error
    );
    assert_eq!(adapter.creates.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.prepared_models.lock().unwrap().len(), 1);
    assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
    runtime
      .execute_created(&created, uuid::Uuid::new_v4().to_string(), Command::State)
      .await
      .unwrap();
    assert_eq!(
      created.target.session_id.as_deref(),
      Some(created.native.native_id.as_str())
    );
    runtime.shutdown().await;
  }
}

#[tokio::test]
async fn cancelled_preparation_keeps_ownership_until_cleanup_and_refuses_competing_input_or_adoption() {
  let (runtime, shared) = fixture();
  let created = selected_creation(&runtime).await;
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  let (release, wait) = oneshot::channel();
  *adapter.prepare_wait.lock().unwrap() = Some(wait);
  let preparing = tokio::spawn({
    let runtime = runtime.clone();
    let created = created.clone();
    async move { runtime.prepare_created_model(&created, test_model()).await }
  });
  adapter.prepare_started.notified().await;
  preparing.abort();
  assert_eq!(
    runtime
      .prepare_created_model(&created, test_model())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
  assert!(
    runtime
      .execute_created(&created, uuid::Uuid::new_v4().to_string(), first_message())
      .await
      .is_err()
  );
  assert!(
    runtime
      .adopt_created(&created, "not-published-yet".into())
      .await
      .is_err()
  );
  assert!(runtime.binding("not-published-yet").is_none());
  adapter.unavailable_state.store(true, Ordering::SeqCst);
  assert!(
    runtime
      .adopt_created_if_ready(&created, "not-published-yet".into())
      .await
      .unwrap()
      .is_none()
  );
  adapter.unavailable_state.store(false, Ordering::SeqCst);
  assert!(runtime.binding("not-published-yet").is_none());
  let closing = tokio::spawn({
    let runtime = runtime.clone();
    let created = created.clone();
    async move {
      runtime
        .close(CloseRuntimeRequest {
          request_id: uuid::Uuid::new_v4().to_string(),
          runtime_id: created.target.runtime_id,
          expected_instance_id: created.target.instance_id,
        })
        .await
    }
  });
  tokio::task::yield_now().await;
  assert!(!closing.is_finished());
  assert_eq!(adapter.closes.load(Ordering::SeqCst), 0);
  release.send(()).unwrap();
  closing.await.unwrap().unwrap();
  assert_eq!(adapter.prepared_models.lock().unwrap().len(), 1);
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
  assert!(runtime.0.state.lock().unwrap().created_models.is_empty());
  assert_eq!(
    runtime
      .prepare_created_model(&created, test_model())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::StaleTarget
  );
  runtime.shutdown().await;
}

#[tokio::test]
async fn adoption_rechecks_pending_preparation_after_its_read_baseline() {
  let (runtime, shared) = fixture();
  let created = selected_creation(&runtime).await;
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  let (baseline_release, baseline_wait) = oneshot::channel();
  *adapter.preflight_wait.lock().unwrap() = Some((Operation::State, baseline_wait));
  let adopting = tokio::spawn({
    let runtime = runtime.clone();
    let created = created.clone();
    async move {
      runtime
        .adopt_created_if_ready(&created, "preparation-race".into())
        .await
    }
  });
  adapter.preflight_started.notified().await;
  let (preparation_release, preparation_wait) = oneshot::channel();
  *adapter.prepare_wait.lock().unwrap() = Some(preparation_wait);
  let preparing = tokio::spawn({
    let runtime = runtime.clone();
    let created = created.clone();
    async move { runtime.prepare_created_model(&created, test_model()).await }
  });
  adapter.prepare_started.notified().await;
  baseline_release.send(()).unwrap();
  assert!(adopting.await.unwrap().unwrap().is_none());
  assert!(runtime.binding("preparation-race").is_none());
  preparation_release.send(()).unwrap();
  preparing.await.unwrap().unwrap();
  assert!(
    runtime
      .adopt_created_if_ready(&created, "preparation-race".into())
      .await
      .unwrap()
      .is_some()
  );
  runtime.shutdown().await;
}

#[tokio::test]
async fn stopped_preparation_can_finish_but_its_first_message_stays_fenced() {
  let (runtime, shared) = fixture();
  let created = selected_creation(&runtime).await;
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  let (release, wait) = oneshot::channel();
  *adapter.prepare_wait.lock().unwrap() = Some(wait);
  let preparing = tokio::spawn({
    let runtime = runtime.clone();
    let created = created.clone();
    async move { runtime.prepare_created_model(&created, test_model()).await }
  });
  adapter.prepare_started.notified().await;
  runtime.fence_created_input(&created).unwrap();
  runtime.fence_created_input(&created).unwrap();
  release.send(()).unwrap();
  preparing.await.unwrap().unwrap();
  assert_eq!(
    runtime
      .execute_created(&created, uuid::Uuid::new_v4().to_string(), first_message())
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Rejected
  );
  assert_eq!(adapter.prepared_models.lock().unwrap().len(), 1);
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
  assert!(
    runtime
      .adopt_created_if_ready(&created, "stopped-created".into())
      .await
      .unwrap()
      .is_some()
  );
  runtime
    .close(CloseRuntimeRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      runtime_id: created.target.runtime_id.clone(),
      expected_instance_id: created.target.instance_id.clone(),
    })
    .await
    .unwrap();
  assert!(runtime.0.state.lock().unwrap().created_input_fences.is_empty());
  assert_eq!(
    runtime.fence_created_input(&created).unwrap_err().kind,
    ErrorKind::StaleTarget
  );
  runtime.shutdown().await;
}

#[tokio::test]
async fn stop_during_either_initial_preflight_fences_default_and_selected_messages() {
  for model in [None, Some(test_model())] {
    for operation in [Operation::State, Operation::PendingInteractions] {
      let (runtime, shared) = fixture();
      let created = runtime
        .create_native(Provider::Codex, std::env::temp_dir(), None, None, model.clone())
        .await
        .unwrap();
      let adapter = shared.adapter.lock().unwrap().clone().unwrap();
      let (release, wait) = oneshot::channel();
      *adapter.preflight_wait.lock().unwrap() = Some((operation, wait));
      let sending = tokio::spawn({
        let runtime = runtime.clone();
        let created = created.clone();
        async move {
          runtime
            .execute_created(&created, uuid::Uuid::new_v4().to_string(), first_message())
            .await
        }
      });
      adapter.preflight_started.notified().await;
      runtime.fence_created_input(&created).unwrap();
      release.send(()).unwrap();
      assert_eq!(sending.await.unwrap().unwrap_err().kind, ErrorKind::Rejected);
      assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
      assert!(adapter.prepared_models.lock().unwrap().is_empty());
      runtime.shutdown().await;
      assert!(runtime.0.state.lock().unwrap().created_input_fences.is_empty());
    }
  }
}
