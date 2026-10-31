use super::*;
use crate::runtime::NativeSessionDescriptor;
use crate::{Capabilities, ImageInput, SessionTarget};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Host {
  creates: AtomicUsize,
  sends: AtomicUsize,
  cancels: AtomicUsize,
  uncertain_create: AtomicBool,
  uncertain_send: AtomicBool,
  reject_model: AtomicBool,
  initial_models: Mutex<Vec<Option<ModelIdentity>>>,
  history_ready: AtomicBool,
  started: Notify,
  create_wait: Mutex<Option<oneshot::Receiver<()>>>,
  send_wait: Mutex<Option<oneshot::Receiver<()>>>,
  send_release: Mutex<Option<oneshot::Sender<()>>>,
  send_started: Notify,
}
impl Default for Host {
  fn default() -> Self {
    Self {
      creates: AtomicUsize::new(0),
      sends: AtomicUsize::new(0),
      cancels: AtomicUsize::new(0),
      uncertain_create: AtomicBool::new(false),
      uncertain_send: AtomicBool::new(false),
      reject_model: AtomicBool::new(false),
      initial_models: Mutex::new(Vec::new()),
      history_ready: AtomicBool::new(false),
      started: Notify::new(),
      create_wait: Mutex::new(None),
      send_wait: Mutex::new(None),
      send_release: Mutex::new(None),
      send_started: Notify::new(),
    }
  }
}
impl CreationHost for Host {
  fn plan_native_id(&self, _provider: Provider, id: &str) -> Result<Option<String>, String> {
    Ok(Some(format!("native-{id}")))
  }
  fn create_native<'a>(
    &'a self,
    request: &'a CreationRequest,
    id: Option<&'a str>,
  ) -> AdapterFuture<'a, Result<CreatedNativeSession, InteractionError>> {
    Box::pin(async move {
      self.creates.fetch_add(1, Ordering::SeqCst);
      self.started.notify_one();
      let wait = self.create_wait.lock().unwrap().take();
      if let Some(wait) = wait {
        let _ = wait.await;
      }
      if self.uncertain_create.load(Ordering::SeqCst) {
        return Err(InteractionError::uncertain("Create acknowledgement lost"));
      }
      Ok(CreatedNativeSession {
        provider: request.provider,
        profile_id: "64f80e49-1898-462c-8a21-598ad0cd405f".into(),
        target: SessionTarget {
          runtime_id: "shared-runtime".into(),
          instance_id: "instance".into(),
          session_id: id.map(str::to_owned),
        },
        native: NativeSessionDescriptor {
          native_id: id.unwrap().into(),
          working_directory: request.working_directory.clone(),
          history_path: None,
        },
        capabilities: Capabilities::default(),
      })
    })
  }
  fn send_initial<'a>(
    &'a self,
    _created: &'a CreatedNativeSession,
    _id: &'a str,
    _input: &'a MessageInput,
    model: Option<&'a ModelIdentity>,
  ) -> AdapterFuture<'a, Result<(), InteractionError>> {
    Box::pin(async move {
      self.initial_models.lock().unwrap().push(model.cloned());
      if self.reject_model.load(Ordering::SeqCst) {
        return Err(InteractionError::invalid(
          "The created model differs from the requested model",
        ));
      }
      self.sends.fetch_add(1, Ordering::SeqCst);
      self.send_started.notify_one();
      let wait = self.send_wait.lock().unwrap().take();
      if let Some(wait) = wait {
        let _ = wait.await;
      }
      if self.uncertain_send.load(Ordering::SeqCst) {
        Err(InteractionError::uncertain("Input acknowledgement lost"))
      } else {
        Ok(())
      }
    })
  }
  fn cancel_initial<'a>(
    &'a self,
    _created: &'a CreatedNativeSession,
  ) -> AdapterFuture<'a, Result<(), InteractionError>> {
    Box::pin(async move {
      self.cancels.fetch_add(1, Ordering::SeqCst);
      if let Some(release) = self.send_release.lock().unwrap().take() {
        let _ = release.send(());
      }
      Ok(())
    })
  }
  fn publish_history<'a>(&'a self, receipt: &'a CreationSnapshot) -> AdapterFuture<'a, Result<Option<String>, String>> {
    Box::pin(async move {
      Ok(
        self
          .history_ready
          .load(Ordering::SeqCst)
          .then(|| format!("verified-{}", receipt.request_id)),
      )
    })
  }
}
fn request() -> CreationRequest {
  CreationRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    provider: Provider::Codex,
    project_id: "project".into(),
    working_directory: std::env::temp_dir(),
    input: MessageInput {
      text: "private first prompt".into(),
      ..Default::default()
    },
    image_ids: vec![],
    runtime: None,
    runtime_plan: None,
    runtime_plan_id: None,
    model: None,
  }
}

#[tokio::test]
async fn creation_plan_survives_restart_without_replaying_input_or_re_resolving_configuration() {
  use crate::runtime_plan::{RuntimeConfigurationIdentity, RuntimePlan, RuntimeReuseScope, RuntimeStrategy};
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("receipts.json");
  let host = Arc::new(Host::default());
  host.uncertain_send.store(true, Ordering::SeqCst);
  let manager = CreationManager::new(Some(path.clone()), host.clone()).unwrap();
  let mut request = request();
  let plan = RuntimePlan {
    provider: request.provider,
    configuration: RuntimeConfigurationIdentity {
      id: "64f80e49-1898-462c-8a21-598ad0cd405f".into(),
      revision: 7,
    },
    ownership: crate::Ownership::Managed,
    reuse_scope: RuntimeReuseScope::Project,
    strategy: RuntimeStrategy::SharedRuntime,
    working_directory: request.working_directory.clone(),
  };
  request.runtime_plan_id = Some(plan.fingerprint().unwrap());
  request.runtime_plan = Some(plan.clone());
  manager.create(request.clone()).await.unwrap();
  let snapshot = settled(&manager, &request.request_id).await;
  assert_eq!(snapshot.runtime_plan, Some(plan));
  assert_eq!(snapshot.first_input, FirstInputState::Uncertain);
  manager.shutdown().await;
  drop(manager);
  let restored = CreationManager::new(Some(path), host.clone()).unwrap();
  let receipt = restored.create(request.clone()).await.unwrap();
  assert_eq!(receipt.runtime_plan_id, request.runtime_plan_id);
  assert_eq!(host.creates.load(Ordering::SeqCst), 1);
  assert_eq!(host.sends.load(Ordering::SeqCst), 1);
  request.runtime_plan_id = None;
  assert_eq!(restored.receipt(&request).unwrap_err().kind, ErrorKind::Conflict);
  let mut changed = request.clone();
  changed.runtime_plan.as_mut().unwrap().working_directory = directory.path().to_path_buf();
  assert_eq!(restored.receipt(&changed).unwrap_err().kind, ErrorKind::InvalidRequest);
}

#[test]
fn model_intent_preserves_legacy_fingerprints_and_ignores_display_labels() {
  let mut request = request();
  let legacy = serde_json::to_vec(&(
    request.provider,
    &request.project_id,
    &request.working_directory,
    &request.input.text,
    &request.image_ids,
    &request.runtime,
  ))
  .unwrap();
  let old = format!("{:x}", Sha256::digest(legacy));
  assert_eq!(fingerprint(&request).unwrap(), old);
  request.model = Some(ModelIdentity {
    model_provider: Some("provider".into()),
    model_id: "model".into(),
    display_name: Some("One label".into()),
  });
  let selected = fingerprint(&request).unwrap();
  assert_ne!(selected, old);
  request.model.as_mut().unwrap().display_name = Some("Changed label".into());
  assert_eq!(fingerprint(&request).unwrap(), selected);
  request.model.as_mut().unwrap().model_id = "different".into();
  assert_ne!(fingerprint(&request).unwrap(), selected);
}

#[tokio::test]
async fn model_is_part_of_durable_creation_intent_without_changing_message_input() {
  let host = Arc::new(Host::default());
  let manager = CreationManager::new(None, host.clone()).unwrap();
  let mut request = request();
  request.model = Some(ModelIdentity {
    model_provider: None,
    model_id: "chosen".into(),
    display_name: Some("Visible label".into()),
  });
  manager.create(request.clone()).await.unwrap();
  let snapshot = settled(&manager, &request.request_id).await;
  assert_eq!(snapshot.model.as_ref().unwrap().model_id, "chosen");
  assert!(snapshot.model.as_ref().unwrap().display_name.is_none());
  assert!(request.input.model.is_none());
  assert_eq!(
    host.initial_models.lock().unwrap()[0].as_ref().unwrap().model_id,
    "chosen"
  );
  request.model.as_mut().unwrap().display_name = None;
  assert!(manager.receipt(&request).unwrap().is_some());
  request.model = None;
  assert_eq!(manager.receipt(&request).unwrap_err().kind, ErrorKind::Conflict);
  assert_eq!(host.creates.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_initial_model_preserves_created_identity_and_never_replays_input() {
  let host = Arc::new(Host::default());
  host.reject_model.store(true, Ordering::SeqCst);
  let manager = CreationManager::new(None, host.clone()).unwrap();
  let mut request = request();
  request.model = Some(ModelIdentity {
    model_provider: None,
    model_id: "chosen".into(),
    display_name: None,
  });
  manager.create(request.clone()).await.unwrap();
  let receipt = settled(&manager, &request.request_id).await;
  assert!(receipt.created.is_some());
  assert_eq!(receipt.first_input, FirstInputState::Rejected);
  assert!(receipt.message.unwrap().contains("differs"));
  host.reject_model.store(false, Ordering::SeqCst);
  manager.create(request).await.unwrap();
  assert_eq!(host.sends.load(Ordering::SeqCst), 0);
  assert_eq!(host.creates.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn noncanonical_uuid_aliases_reject_before_native_admission() {
  let host = Arc::new(Host::default());
  let manager = CreationManager::new(None, host.clone()).unwrap();
  let mut request = request();
  request.request_id = "64f80e49-1898-462c-8a21-598ad0cd405f".into();
  for alias in [request.request_id.to_uppercase(), request.request_id.replace('-', "")] {
    let mut aliased = request.clone();
    aliased.request_id = alias;
    assert_eq!(
      manager.create(aliased).await.unwrap_err().kind,
      ErrorKind::InvalidRequest
    );
  }
  assert_eq!(host.creates.load(Ordering::SeqCst), 0);
  manager.create(request.clone()).await.unwrap();
  settled(&manager, &request.request_id).await;
  assert_eq!(host.creates.load(Ordering::SeqCst), 1);
}
async fn settled(manager: &CreationManager, id: &str) -> CreationSnapshot {
  tokio::time::timeout(std::time::Duration::from_secs(3), async {
    loop {
      let snapshot = manager.get(id).unwrap().unwrap();
      if matches!(
        snapshot.phase,
        CreationPhase::HistoryPending | CreationPhase::Ready | CreationPhase::Failed
      ) || snapshot.message.is_some()
      {
        return snapshot;
      }
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap()
}

#[tokio::test]
async fn duplicate_creation_and_restart_never_replay_private_input() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("viewer/creations.json");
  let host = Arc::new(Host::default());
  let manager = CreationManager::new(Some(path.clone()), host.clone()).unwrap();
  let mut request = request();
  request.image_ids.push(uuid::Uuid::new_v4().to_string());
  request.input.images.push(ImageInput {
    mime_type: "image/png".into(),
    data: "private-image-data".into(),
  });
  let (first, repeated) = tokio::join!(manager.create(request.clone()), manager.create(request.clone()));
  assert_eq!(first.unwrap().request_id, repeated.unwrap().request_id);
  assert_eq!(
    settled(&manager, &request.request_id).await.phase,
    CreationPhase::HistoryPending
  );
  let saved = std::fs::read_to_string(&path).unwrap();
  assert!(!saved.contains("private first prompt"));
  assert!(!saved.contains("private-image-data"));
  let mut without_bytes = request.clone();
  without_bytes.input.images.clear();
  assert!(
    manager.receipt(&without_bytes).unwrap().is_some(),
    "accepted receipts never need upload bytes again"
  );
  let mut changed = request.clone();
  changed.input.text.push_str(" changed");
  assert_eq!(manager.create(changed).await.unwrap_err().kind, ErrorKind::Conflict);
  manager.shutdown().await;
  let restarted = CreationManager::new(Some(path), host.clone()).unwrap();
  restarted.create(request.clone()).await.unwrap();
  host.history_ready.store(true, Ordering::SeqCst);
  assert_eq!(
    restarted.refresh(&request.request_id).await.unwrap().unwrap().phase,
    CreationPhase::Ready
  );
  assert_eq!(host.creates.load(Ordering::SeqCst), 1);
  assert_eq!(host.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn uncertain_native_or_input_outcomes_are_observed_without_retries() {
  for uncertain_create in [true, false] {
    let host = Arc::new(Host::default());
    host.uncertain_create.store(uncertain_create, Ordering::SeqCst);
    host.uncertain_send.store(!uncertain_create, Ordering::SeqCst);
    let manager = CreationManager::new(None, host.clone()).unwrap();
    let request = request();
    manager.create(request.clone()).await.unwrap();
    let snapshot = settled(&manager, &request.request_id).await;
    assert_eq!(
      snapshot.phase,
      if uncertain_create {
        CreationPhase::NativeCreateUncertain
      } else {
        CreationPhase::FirstInputUncertain
      }
    );
    manager.create(request.clone()).await.unwrap();
    host.history_ready.store(true, Ordering::SeqCst);
    assert!(
      manager
        .refresh(&request.request_id)
        .await
        .unwrap()
        .unwrap()
        .session_key
        .is_some()
    );
    assert_eq!(host.creates.load(Ordering::SeqCst), 1);
    assert_eq!(host.sends.load(Ordering::SeqCst), usize::from(!uncertain_create));
  }
}

#[tokio::test]
async fn admission_survives_caller_cancellation_and_cross_process_observers() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("creations.json");
  let host = Arc::new(Host::default());
  let (release, wait) = oneshot::channel();
  *host.create_wait.lock().unwrap() = Some(wait);
  let manager = CreationManager::new(Some(path.clone()), host.clone()).unwrap();
  let request = request();
  let creator = manager.clone();
  let submitted = request.clone();
  let caller = tokio::spawn(async move { creator.create(submitted).await });
  host.started.notified().await;
  caller.abort();
  let other = CreationManager::new(Some(path), host.clone()).unwrap();
  other.create(request.clone()).await.unwrap();
  assert!(
    other.cancel(&request.request_id).await.is_err(),
    "another host's request lease also fences cancellation"
  );
  assert_eq!(host.creates.load(Ordering::SeqCst), 1);
  let shutting_down = manager.clone();
  let shutdown = tokio::spawn(async move { shutting_down.shutdown().await });
  tokio::task::yield_now().await;
  assert!(!shutdown.is_finished());
  release.send(()).unwrap();
  shutdown.await.unwrap();
  assert_eq!(host.sends.load(Ordering::SeqCst), 1);
  assert_eq!(manager.create(request).await.unwrap_err().kind, ErrorKind::Unavailable);
}

#[tokio::test]
async fn cancellation_before_first_input_fences_delivery_and_survives_restart() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("creation.json");
  let host = Arc::new(Host::default());
  let (release, wait) = oneshot::channel();
  *host.create_wait.lock().unwrap() = Some(wait);
  let manager = CreationManager::new(Some(path.clone()), host.clone()).unwrap();
  let request = request();
  manager.create(request.clone()).await.unwrap();
  host.started.notified().await;
  let cancelling = manager.clone();
  let id = request.request_id.clone();
  let cancelled = tokio::spawn(async move { cancelling.cancel(&id).await });
  tokio::time::timeout(std::time::Duration::from_secs(2), async {
    while !manager.get(&request.request_id).unwrap().unwrap().cancel_requested {
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
  release.send(()).unwrap();
  let snapshot = cancelled.await.unwrap().unwrap().unwrap();
  assert_eq!(snapshot.first_input, FirstInputState::NotAttempted);
  assert!(snapshot.cancel_requested);
  assert_eq!(host.sends.load(Ordering::SeqCst), 0);
  manager.shutdown().await;
  let restarted = CreationManager::new(Some(path), host.clone()).unwrap();
  restarted.create(request).await.unwrap();
  assert_eq!(host.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_unblocks_an_in_flight_initial_admission_without_closing_its_runtime() {
  let host = Arc::new(Host::default());
  let (release, wait) = oneshot::channel();
  *host.send_wait.lock().unwrap() = Some(wait);
  *host.send_release.lock().unwrap() = Some(release);
  let manager = CreationManager::new(None, host.clone()).unwrap();
  let request = request();
  manager.create(request.clone()).await.unwrap();
  host.send_started.notified().await;
  let snapshot = tokio::time::timeout(std::time::Duration::from_secs(2), manager.cancel(&request.request_id))
    .await
    .unwrap()
    .unwrap()
    .unwrap();
  assert!(snapshot.cancel_requested);
  assert_eq!(host.sends.load(Ordering::SeqCst), 1);
  assert!(host.cancels.load(Ordering::SeqCst) >= 1);
  manager.create(request).await.unwrap();
  assert_eq!(host.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_first_admission_publishes_history_so_its_native_question_can_be_answered() {
  let host = Arc::new(Host::default());
  host.history_ready.store(true, Ordering::SeqCst);
  let (release, wait) = oneshot::channel();
  *host.send_wait.lock().unwrap() = Some(wait);
  let manager = CreationManager::new(None, host.clone()).unwrap();
  let request = request();
  manager.create(request.clone()).await.unwrap();
  host.send_started.notified().await;
  let early = tokio::time::timeout(
    std::time::Duration::from_millis(50),
    manager.refresh(&request.request_id),
  )
  .await
  .expect("refresh must not await the initial question's admission")
  .unwrap()
  .unwrap();
  assert_eq!(early.first_input, FirstInputState::Uncertain);
  let pending = tokio::time::timeout(std::time::Duration::from_secs(2), async {
    loop {
      let snapshot = manager.get(&request.request_id).unwrap().unwrap();
      if snapshot.session_key.is_some() {
        return snapshot;
      }
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
  assert_eq!(pending.first_input, FirstInputState::Uncertain);
  assert_eq!(pending.phase, CreationPhase::FirstInputUncertain);
  let refreshed = tokio::time::timeout(
    std::time::Duration::from_millis(50),
    manager.refresh(&request.request_id),
  )
  .await
  .expect("published history must remain immediately available while native input waits")
  .unwrap()
  .unwrap();
  assert_eq!(refreshed.session_key, pending.session_key);
  tokio::time::sleep(std::time::Duration::from_millis(550)).await;
  assert_eq!(
    manager.get(&request.request_id).unwrap().unwrap().revision,
    pending.revision,
    "published history does not cause repeated receipt writes while awaiting native admission"
  );
  // Reading the exact history provides the UI route needed to answer the native
  // preflight question. Completing that one admission must advance Ready.
  release.send(()).unwrap();
  assert_eq!(settled(&manager, &request.request_id).await.phase, CreationPhase::Ready);
  assert_eq!(host.creates.load(Ordering::SeqCst), 1);
  assert_eq!(host.sends.load(Ordering::SeqCst), 1);
}
