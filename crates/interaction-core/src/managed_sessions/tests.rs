use super::*;
use std::{path::Path, sync::atomic::AtomicUsize};

#[derive(Default)]
struct TestDriver {
  starts: AtomicUsize,
  prompts: Mutex<Vec<String>>,
}

impl ManagedSessionDriver for Arc<TestDriver> {
  fn provider(&self) -> crate::Provider {
    crate::Provider::Dsh
  }
  fn label(&self) -> &str {
    "test agent"
  }
  fn spawn(&self, worker: ManagedWorker) -> crate::AdapterFuture<'static, ()> {
    let driver = self.clone();
    Box::pin(async move {
      driver.starts.fetch_add(1, Ordering::SeqCst);
      let ManagedWorker {
        host,
        lease,
        native_lease,
        mut receipt,
        prompt,
        resume,
        mut commands,
        worker_state,
        termination,
      } = worker;
      let lifecycle = ManagedWorkerLifecycle::new(host.clone(), lease, native_lease.clone(), worker_state.clone());
      assert_eq!(resume, prompt.is_none());
      if let Some((prompt, _)) = prompt {
        driver.prompts.lock().unwrap().push(prompt);
      }
      let id = receipt
        .session_id
        .clone()
        .unwrap_or_else(|| format!("native-{}", receipt.request_id));
      let path = receipt.folder.join("history.txt");
      if !resume {
        std::fs::write(&path, &id).unwrap();
      }
      let (path, key) = host
        .validate_history(&path, &id, &receipt.folder)
        .await
        .unwrap()
        .unwrap();
      let _native = match native_lease {
        Some(lease) => lease,
        None => host.acquire_native_ownership(&id).await.unwrap(),
      };
      receipt.session_id = Some(id);
      receipt.session_file = Some(path);
      receipt.session_key = Some(key);
      receipt.status = ManagedSessionStatus::Idle;
      host.update(receipt).await.unwrap();
      host.worker_started(Some(123));
      worker_state.send_replace(WorkerState::Ready);
      let mut shutdown = host.shutdown().unwrap();
      loop {
        if *shutdown.borrow() || termination.is_cancelled() {
          break;
        }
        tokio::select! {
          _ = termination.cancelled() => break,
          _ = shutdown.changed() => break,
          command = commands.recv() => match command {
            Some(Command::Prompt { text, reply, .. }) => {
              driver.prompts.lock().unwrap().push(text);
              let _ = reply.send(Ok(Admission::Started));
            }
            Some(Command::Abort(reply)) | Some(Command::Dialog(_, reply)) => { let _ = reply.send(Ok(())); }
            Some(Command::Model(_, reply)) => { let _ = reply.send(Err(DeliveryError::not_sent("No model catalog"))); }
            None => break,
          }
        }
      }
      lifecycle.close_admission(&mut commands, None, "test worker stopped");
      drop(_native);
      lifecycle.complete(None, true, None, None).await;
    })
  }
  fn validate_history(
    &self,
    path: &Path,
    session_id: &str,
    folder: &Path,
  ) -> Result<Option<ManagedHistoryIdentity>, String> {
    if path.parent() != Some(folder) || std::fs::read_to_string(path).map_err(|error| error.to_string())? != session_id
    {
      return Err("Wrong native history identity".into());
    }
    Ok(Some(ManagedHistoryIdentity {
      provider: self.provider(),
      session_id: session_id.into(),
      session_file: path.canonicalize().unwrap(),
    }))
  }
}

struct TestHost;
impl ManagedSessionHost for TestHost {
  fn session_key(&self, identity: &ManagedHistoryIdentity) -> Result<String, String> {
    Ok(format!("host-key:{}", identity.session_id))
  }
  fn keys_equal(&self, left: &str, right: &str) -> bool {
    left.trim_start_matches("alias:") == right.trim_start_matches("alias:")
  }
}

fn configuration(path: Option<PathBuf>) -> ManagedSessionConfiguration {
  ManagedSessionConfiguration {
    state_path: path,
    idle_timeout: IDLE_TIMEOUT,
    legacy_provider: Some(crate::Provider::Dsh),
    lease_prefix: "test-managed".into(),
  }
}

fn test_manager(path: Option<PathBuf>, driver: &Arc<TestDriver>) -> ManagedSessions {
  ManagedSessions::new(
    configuration(path),
    Arc::new(driver.clone()),
    Arc::new(TestHost),
    SessionOwnership::new(None).unwrap(),
  )
  .unwrap()
}

fn request(folder: &Path) -> (CreateProjectSessionRequest, RegisteredProject) {
  (
    CreateProjectSessionRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      project_id: "opaque-project".into(),
      prompt: "initial prompt".into(),
      image_ids: Vec::new(),
      model: None,
      runtime_plan_id: None,
    },
    RegisteredProject {
      project_id: "opaque-project".into(),
      folder_path: folder.canonicalize().unwrap().to_string_lossy().into_owned(),
      available: true,
    },
  )
}

#[test]
fn creation_model_fingerprints_preserve_legacy_receipts_and_ignore_display_names() {
  let directory = tempfile::tempdir().unwrap();
  let (mut request, _) = request(directory.path());
  assert_eq!(
    creation_fingerprint(&request, "/project").unwrap(),
    "c3f1046ff3959a9e0787a28128f58011804a282076cce44fc4ecaf6f577ab561"
  );
  request.image_ids = vec!["first-image".into()];
  let legacy = creation_fingerprint(&request, "/project").unwrap();
  assert_eq!(
    legacy,
    "239648e86a61a95898ea5c2aa0de5968e51c5afa9262e72c42d09d4b9f0ef11b"
  );
  request.model = Some(crate::ModelIdentity {
    model_provider: Some("provider".into()),
    model_id: "model".into(),
    display_name: None,
  });
  let selected = creation_fingerprint(&request, "/project").unwrap();
  assert_eq!(
    selected,
    "a6ca9e3f36325fd1e0d606ce310ada3add5ff0b804b7e3f2e73a5c050fdb2134"
  );
  assert_ne!(selected, legacy);
  request.model.as_mut().unwrap().display_name = Some("Updated label".into());
  assert_eq!(creation_fingerprint(&request, "/project").unwrap(), selected);
  request.model.as_mut().unwrap().model_id = "other".into();
  assert_ne!(creation_fingerprint(&request, "/project").unwrap(), selected);
  request.model.as_mut().unwrap().model_id = "model".into();
  request.model.as_mut().unwrap().model_provider = Some("other".into());
  assert_ne!(creation_fingerprint(&request, "/project").unwrap(), selected);
}

#[test]
fn creation_model_identity_is_bounded_before_admission() {
  for value in ["", " leading", "line\nbreak", &"x".repeat(513)] {
    for provider in [false, true] {
      let model = crate::ModelIdentity {
        model_provider: Some(if provider { value } else { "provider" }.into()),
        model_id: if provider { "model" } else { value }.into(),
        display_name: None,
      };
      assert!(validate_creation_model(&model).is_err());
    }
  }
}

#[test]
fn creation_plan_fences_are_optional_bounded_and_part_of_the_fingerprint() {
  let directory = tempfile::tempdir().unwrap();
  let (mut request, _) = request(directory.path());
  for model in [
    None,
    Some(crate::ModelIdentity {
      model_provider: Some("provider".into()),
      model_id: "model".into(),
      display_name: None,
    }),
  ] {
    request.model = model;
    let legacy = creation_fingerprint(&request, "/project").unwrap();
    request.runtime_plan_id = Some("a".repeat(64));
    validate_creation_request(&request).unwrap();
    let pinned = creation_fingerprint(&request, "/project").unwrap();
    assert_ne!(pinned, legacy);
    request.runtime_plan_id = Some("b".repeat(64));
    assert_ne!(creation_fingerprint(&request, "/project").unwrap(), pinned);
    request.runtime_plan_id = None;
    assert_eq!(creation_fingerprint(&request, "/project").unwrap(), legacy);
  }
  for invalid in [
    "".to_owned(),
    "a".repeat(63),
    "a".repeat(65),
    "g".repeat(64),
    " ".repeat(64),
  ] {
    request.runtime_plan_id = Some(invalid);
    assert!(
      validate_creation_request(&request)
        .unwrap_err()
        .contains("runtime plan")
    );
  }
}

#[tokio::test]
async fn creation_plan_fence_survives_resume_and_cannot_be_changed_by_a_worker_or_duplicate() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("managed.json");
  let driver = Arc::new(TestDriver::default());
  let manager = test_manager(Some(path.clone()), &driver);
  let (mut request, project) = request(directory.path());
  request.runtime_plan_id = Some("a".repeat(64));
  manager.create(request.clone(), project).await.unwrap();
  wait_status(&manager, &request.request_id, ManagedSessionStatus::Idle).await;
  let (original, generation) = {
    let records = manager.inner.records.lock().unwrap();
    let record = records.get(&request.request_id).unwrap();
    (record.receipt.clone(), record.generation)
  };
  let host = ManagedHostContext {
    inner: Arc::downgrade(&manager.inner),
    id: request.request_id.clone(),
    generation,
  };
  for plan_id in [None, Some("b".repeat(64))] {
    let mut changed = original.clone();
    changed.runtime_plan_id = plan_id.clone();
    assert!(
      host
        .update(changed)
        .await
        .unwrap_err()
        .contains("immutable creation intent")
    );
    let mut changed = request.clone();
    changed.runtime_plan_id = plan_id;
    assert!(
      manager
        .creation_receipt(&changed)
        .unwrap_err()
        .contains("different content")
    );
  }
  let mut changed = original.clone();
  changed.model = Some(crate::ModelIdentity {
    model_provider: Some("provider".into()),
    model_id: "changed".into(),
    display_name: None,
  });
  assert!(
    host
      .update(changed)
      .await
      .unwrap_err()
      .contains("immutable creation intent")
  );
  manager.shutdown().await;
  let restored = test_manager(Some(path), &driver);
  assert_eq!(
    restored
      .inner
      .store
      .latest(&request.request_id)
      .unwrap()
      .unwrap()
      .runtime_plan_id,
    request.runtime_plan_id
  );
  restored
    .resume(ManagedSessionRequest {
      request_id: Some(request.request_id.clone()),
      session_key: None,
    })
    .await
    .unwrap();
  wait_status(&restored, &request.request_id, ManagedSessionStatus::Idle).await;
  assert_eq!(driver.starts.load(Ordering::SeqCst), 2);
  assert_eq!(driver.prompts.lock().unwrap().as_slice(), ["initial prompt"]);
  assert_eq!(
    restored
      .inner
      .store
      .latest(&request.request_id)
      .unwrap()
      .unwrap()
      .runtime_plan_id,
    request.runtime_plan_id
  );
  restored.shutdown().await;
}

async fn wait_status(manager: &ManagedSessions, id: &str, expected: ManagedSessionStatus) -> ManagedSessionSnapshot {
  tokio::time::timeout(std::time::Duration::from_secs(5), async {
    loop {
      let snapshot = manager.get_by_id(id).unwrap();
      if snapshot.status == expected {
        return snapshot;
      }
      tokio::task::yield_now().await;
    }
  })
  .await
  .expect("worker status deadline")
}

#[tokio::test]
async fn concurrent_creation_and_resume_share_one_worker_and_never_replay_initial_prompt() {
  let directory = tempfile::tempdir().unwrap();
  let driver = Arc::new(TestDriver::default());
  let manager = test_manager(Some(directory.path().join("managed.json")), &driver);
  let (request, project) = request(directory.path());
  let mut creates = tokio::task::JoinSet::new();
  for _ in 0..8 {
    let (manager, request, project) = (manager.clone(), request.clone(), project.clone());
    creates.spawn(async move { manager.create(request, project).await });
  }
  while let Some(result) = creates.join_next().await {
    result.unwrap().unwrap();
  }
  let snapshot = wait_status(&manager, &request.request_id, ManagedSessionStatus::Idle).await;
  assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
  manager
    .inner
    .records
    .lock()
    .unwrap()
    .get(&request.request_id)
    .unwrap()
    .termination
    .as_ref()
    .unwrap()
    .cancel();
  wait_status(&manager, &request.request_id, ManagedSessionStatus::Stopped).await;
  let mut sends = tokio::task::JoinSet::new();
  for index in 0..8 {
    let manager = manager.clone();
    let key = format!("alias:{}", snapshot.session_key.as_ref().unwrap());
    sends.spawn(async move {
      manager
        .submit_owned_input(&key, &uuid::Uuid::new_v4().to_string(), &format!("message {index}"))
        .await
    });
  }
  while let Some(result) = sends.join_next().await {
    result.unwrap().unwrap();
  }
  assert_eq!(driver.starts.load(Ordering::SeqCst), 2);
  let prompts = driver.prompts.lock().unwrap().clone();
  assert_eq!(prompts.len(), 9);
  assert_eq!(
    prompts
      .iter()
      .filter(|prompt| prompt.as_str() == "initial prompt")
      .count(),
    1
  );
  manager.shutdown().await;
  assert!(
    manager
      .resume(ManagedSessionRequest {
        request_id: Some(request.request_id),
        session_key: None
      })
      .await
      .is_err()
  );
}

#[tokio::test]
async fn restart_fences_instance_and_resumes_without_replaying_creation() {
  let directory = tempfile::tempdir().unwrap();
  let driver = Arc::new(TestDriver::default());
  let manager = test_manager(Some(directory.path().join("managed.json")), &driver);
  let (request, project) = request(directory.path());
  manager.create(request.clone(), project).await.unwrap();
  wait_status(&manager, &request.request_id, ManagedSessionStatus::Idle).await;
  let runtime = manager.list_runtimes().await.pop().unwrap();
  let restart = RuntimeRestartRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    runtime_id: runtime.runtime_id,
    expected_instance_id: runtime.instance_id,
  };
  manager.restart_runtime(&restart).await.unwrap();
  assert_eq!(driver.starts.load(Ordering::SeqCst), 2);
  assert_eq!(driver.prompts.lock().unwrap().len(), 1);
  assert!(manager.restart_runtime(&restart).await.unwrap_err().contains("changed"));
  manager.shutdown().await;
}

#[tokio::test]
async fn durable_v1_receipts_require_explicit_provider_migration_and_preserve_identity() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("managed.json");
  let driver = Arc::new(TestDriver::default());
  let manager = test_manager(Some(path.clone()), &driver);
  let (request, project) = request(directory.path());
  manager.create(request.clone(), project).await.unwrap();
  let snapshot = wait_status(&manager, &request.request_id, ManagedSessionStatus::Idle).await;
  manager.shutdown().await;
  drop(manager);
  let mut saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
  saved["version"] = 1.into();
  saved["receipts"][0].as_object_mut().unwrap().remove("provider");
  std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
  let mut config = configuration(Some(path.clone()));
  config.legacy_provider = None;
  let error = ManagedSessions::new(
    config,
    Arc::new(driver.clone()),
    Arc::new(TestHost),
    SessionOwnership::new(None).unwrap(),
  )
  .err()
  .unwrap();
  assert!(error.contains("explicit provider"));
  let mut wrong_provider = configuration(Some(path.clone()));
  wrong_provider.legacy_provider = Some(crate::Provider::Pi);
  let error = ManagedSessions::new(
    wrong_provider,
    Arc::new(driver.clone()),
    Arc::new(TestHost),
    SessionOwnership::new(None).unwrap(),
  )
  .err()
  .unwrap();
  assert!(error.contains("different provider"));
  let migrated = test_manager(Some(path.clone()), &driver);
  let recovered = migrated.get_by_id(&request.request_id).unwrap();
  assert_eq!(recovered.session_key, snapshot.session_key);
  assert_eq!(recovered.status, ManagedSessionStatus::Stopped);
  assert_eq!(driver.starts.load(Ordering::SeqCst), 1);
  let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
  assert_eq!(saved["version"], 2);
  assert_eq!(saved["receipts"][0]["provider"], "dsh");
}

#[tokio::test(start_paused = true)]
async fn idle_policy_resets_when_any_provider_work_prevents_retirement() {
  let timeout = std::time::Duration::from_secs(30);
  let mut policy = ManagedIdlePolicy::new(timeout);
  assert!(!policy.observe(true));
  tokio::time::advance(timeout).await;
  assert!(policy.observe(true));
  assert!(!policy.observe(false));
  assert!(!policy.observe(true));
  tokio::time::advance(timeout).await;
  assert!(policy.observe(true));
}
