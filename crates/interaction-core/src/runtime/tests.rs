use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::sync::oneshot;

struct FakeHost {
  profile: ProfileDescriptor,
  shared: Arc<FakeHostState>,
}

struct FakeHostState {
  opens: AtomicUsize,
  adapter: Mutex<Option<Arc<FakeAdapter>>>,
  open_wait: Mutex<Option<oneshot::Receiver<()>>>,
  open_started: Notify,
}

impl RuntimeHost for FakeHost {
  type Profile = ProfileDescriptor;

  fn resolve_profile(&self, id: &str, revision: u64) -> Result<Self::Profile, String> {
    if id != self.profile.profile_id || revision != self.profile.revision {
      return Err("Profile changed".into());
    }
    Ok(self.profile.clone())
  }

  fn describe(&self, profile: &Self::Profile) -> ProfileDescriptor {
    profile.clone()
  }

  fn validate_launch_ready(&self, _profile: &Self::Profile) -> Result<(), String> {
    Ok(())
  }

  fn select_profile<'a>(
    &'a self,
    _provider: Provider,
    _cwd: &'a Path,
  ) -> AdapterFuture<'a, Result<(String, u64), String>> {
    Box::pin(async { Ok((self.profile.profile_id.clone(), self.profile.revision)) })
  }

  fn open<'a>(
    &'a self,
    id: &'a str,
    _profile: &'a Self::Profile,
  ) -> AdapterFuture<'a, Result<Arc<dyn RuntimeHandle>, String>> {
    Box::pin(async move {
      self.shared.opens.fetch_add(1, Ordering::SeqCst);
      let wait = self.shared.open_wait.lock().unwrap().take();
      self.shared.open_started.notify_one();
      if let Some(wait) = wait {
        let _ = wait.await;
      }
      let adapter = Arc::new(FakeAdapter {
        descriptor: RuntimeDescriptor {
          runtime_id: id.into(),
          instance_id: uuid::Uuid::new_v4().to_string(),
          provider: self.profile.provider,
          ownership: self.profile.ownership,
          capabilities: Capabilities::default()
            .with(Operation::Metadata)
            .with(Operation::State)
            .with(Operation::PendingInteractions)
            .with(Operation::Message),
        },
        events: broadcast::channel(8).0,
        resumes: AtomicUsize::new(0),
        sends: AtomicUsize::new(0),
        closes: AtomicUsize::new(0),
      });
      *self.shared.adapter.lock().unwrap() = Some(adapter.clone());
      Ok(Arc::new(FakeHandle(adapter)) as Arc<dyn RuntimeHandle>)
    })
  }

  fn redact_error(&self, _error: &str, _profile: &Self::Profile) -> String {
    "The host rejected this connection".into()
  }
}

struct FakeHandle(Arc<FakeAdapter>);
impl RuntimeHandle for FakeHandle {
  fn adapter(&self) -> Arc<dyn InteractionAdapter> {
    self.0.clone()
  }

  fn bind_native<'a>(&'a self, _id: &'a str) -> AdapterFuture<'a, Result<(), String>> {
    Box::pin(async {
      self.0.resumes.fetch_add(1, Ordering::SeqCst);
      Ok(())
    })
  }
}

struct FakeAdapter {
  descriptor: RuntimeDescriptor,
  events: broadcast::Sender<RuntimeEvent>,
  resumes: AtomicUsize,
  sends: AtomicUsize,
  closes: AtomicUsize,
}

impl InteractionAdapter for FakeAdapter {
  fn descriptor(&self) -> RuntimeDescriptor {
    self.descriptor.clone()
  }

  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Result<Reply, InteractionError>> {
    Box::pin(async move {
      Ok(match request.command {
        Command::Metadata => Reply::Metadata(Metadata::default()),
        Command::State => Reply::State(SessionState {
          session_id: request.target.session_id,
          status: ExecutionState::Idle,
          model: None,
          pending_inputs: Some(0),
          revision: None,
        }),
        Command::PendingInteractions => Reply::Interactions(Vec::new()),
        Command::Message { .. } => {
          self.sends.fetch_add(1, Ordering::SeqCst);
          Reply::Admission(InputReceipt {
            disposition: InputDisposition::Accepted,
            model: None,
            model_ignored: false,
            native_id: None,
          })
        }
        _ => Reply::Acknowledged,
      })
    })
  }

  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.events.subscribe()
  }

  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>> {
    Box::pin(async {
      self.closes.fetch_add(1, Ordering::SeqCst);
      Ok(())
    })
  }
}

fn fixture() -> (RuntimeCoordinator<FakeHost>, Arc<FakeHostState>) {
  let shared = Arc::new(FakeHostState {
    opens: AtomicUsize::new(0),
    adapter: Mutex::new(None),
    open_wait: Mutex::new(None),
    open_started: Notify::new(),
  });
  let host = FakeHost {
    profile: ProfileDescriptor {
      profile_id: uuid::Uuid::new_v4().to_string(),
      revision: 1,
      name: "Scripted runtime".into(),
      provider: Provider::Codex,
      ownership: Ownership::Attached,
    },
    shared: shared.clone(),
  };
  let runtime = RuntimeCoordinator::new(host, InteractionService::default(), None, SessionOwnership::new(None));
  (runtime, shared)
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
  tokio::time::timeout(std::time::Duration::from_secs(2), async {
    while !condition() {
      tokio::task::yield_now().await;
    }
  })
  .await
  .unwrap();
}

#[tokio::test]
async fn passive_inspection_concurrent_activation_and_idle_release_are_shared() {
  let (runtime, shared) = fixture();
  assert!(runtime.inventory().runtimes.is_empty());
  assert!(runtime.binding_snapshot("opaque-key").is_none());
  assert_eq!(shared.opens.load(Ordering::SeqCst), 0);
  let first_id = uuid::Uuid::new_v4().to_string();
  let second_id = uuid::Uuid::new_v4().to_string();
  let cwd = std::env::temp_dir();
  let (first, second) = tokio::join!(
    runtime.ensure_session(&first_id, "opaque-key", Provider::Codex, "native-id", &cwd),
    runtime.ensure_session(&second_id, "opaque-key", Provider::Codex, "native-id", &cwd),
  );
  let binding = first.unwrap();
  assert_eq!(binding.target, second.unwrap().target);
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  assert_eq!(adapter.resumes.load(Ordering::SeqCst), 1);
  let request_id = uuid::Uuid::new_v4().to_string();
  let command = Command::Message {
    input: MessageInput {
      text: "one delivery".into(),
      ..Default::default()
    },
    delivery: DeliveryMode::StartOrSteer,
  };
  runtime
    .execute_binding(&binding, request_id.clone(), command.clone())
    .await
    .unwrap();
  runtime.execute_binding(&binding, request_id, command).await.unwrap();
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 1);
  runtime.release_binding("opaque-key").await.unwrap();
  assert!(runtime.binding_snapshot("opaque-key").is_none());
  assert!(
    runtime
      .0
      .session_ownership
      .as_ref()
      .unwrap()
      .acquire(Provider::Codex, "native-id")
      .is_ok()
  );
  assert_eq!(
    runtime
      .execute_binding(&binding, uuid::Uuid::new_v4().to_string(), Command::State)
      .await
      .unwrap_err()
      .kind,
    ErrorKind::StaleTarget
  );
  runtime.shutdown().await;
}

#[tokio::test]
async fn cancelled_start_is_owned_until_shutdown_finishes_its_cleanup() {
  let (runtime, shared) = fixture();
  let (release, wait) = oneshot::channel();
  *shared.open_wait.lock().unwrap() = Some(wait);
  let request = StartRuntimeRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    profile_id: runtime.0.host.profile.profile_id.clone(),
    expected_revision: 1,
  };
  let starting = {
    let runtime = runtime.clone();
    let request = request.clone();
    tokio::spawn(async move { runtime.start(request).await })
  };
  shared.open_started.notified().await;
  starting.abort();
  let shutdown = {
    let runtime = runtime.clone();
    tokio::spawn(async move { runtime.shutdown().await })
  };
  wait_until(|| runtime.is_stopping()).await;
  assert!(!shutdown.is_finished());
  release.send(()).unwrap();
  shutdown.await.unwrap();
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
  assert_eq!(
    shared
      .adapter
      .lock()
      .unwrap()
      .as_ref()
      .unwrap()
      .closes
      .load(Ordering::SeqCst),
    1
  );
  assert!(runtime.inventory().runtimes.is_empty());
  assert!(runtime.start(request).await.is_err());
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disconnected_native_owner_requires_its_exact_runtime_close() {
  let (runtime, shared) = fixture();
  let binding = runtime
    .ensure_session(
      &uuid::Uuid::new_v4().to_string(),
      "opaque-key",
      Provider::Codex,
      "native-id",
      &std::env::temp_dir(),
    )
    .await
    .unwrap();
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  adapter
    .events
    .send(RuntimeEvent {
      runtime_id: binding.target.runtime_id.clone(),
      instance_id: binding.target.instance_id.clone(),
      session_id: None,
      sequence: 1,
      kind: RuntimeEventKind::Disconnected {
        reason: "scripted".into(),
      },
    })
    .unwrap();
  wait_until(|| runtime.inventory().runtimes[0].status == "disconnected").await;
  assert!(runtime.binding("opaque-key").is_none());
  assert!(
    runtime
      .0
      .session_ownership
      .as_ref()
      .unwrap()
      .acquire(Provider::Codex, "native-id")
      .is_err()
  );
  assert!(
    runtime
      .close(CloseRuntimeRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        runtime_id: binding.target.runtime_id.clone(),
        expected_instance_id: "stale-instance".into(),
      })
      .await
      .is_err()
  );
  runtime
    .close(CloseRuntimeRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      runtime_id: binding.target.runtime_id,
      expected_instance_id: binding.target.instance_id,
    })
    .await
    .unwrap();
  assert!(
    runtime
      .0
      .session_ownership
      .as_ref()
      .unwrap()
      .acquire(Provider::Codex, "native-id")
      .is_ok()
  );
  runtime.shutdown().await;
}

#[tokio::test]
async fn unregistered_uncertain_cleanup_remains_visible_and_holds_profile_lease_until_shutdown() {
  let directory = tempfile::tempdir().unwrap();
  let lease_directory = directory.path().join("rpc-leases");
  let (mut first, shared) = fixture();
  Arc::get_mut(&mut first.0).unwrap().lease_directory = Some(lease_directory.clone());
  let profile = first.0.host.profile.clone();
  let second = RuntimeCoordinator::new(
    FakeHost {
      profile: profile.clone(),
      shared: shared.clone(),
    },
    InteractionService::default(),
    Some(lease_directory.clone()),
    SessionOwnership::new(None),
  );
  let mut request = StartRuntimeRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    profile_id: profile.profile_id.clone(),
    expected_revision: profile.revision,
  };
  let lease =
    tokn_session_storage::acquire_private_lock(&lease_directory.join(format!("{}.lock", profile.profile_id))).unwrap();
  let handle = first
    .0
    .host
    .open(&format!("rpc:{}", profile.profile_id), &profile)
    .await
    .unwrap();
  first.retain_unconfirmed_start(Some(handle), Some(lease), &profile);
  let info = first.inventory().runtimes[0].clone();
  assert_eq!(info.status, "cleanup_failed");
  assert!(info.last_error.is_some());
  assert!(first.0.interactions.snapshots().is_empty());
  assert!(first.start(request.clone()).await.is_err());
  assert!(
    first
      .close(CloseRuntimeRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        runtime_id: info.runtime_id,
        expected_instance_id: info.instance_id,
      })
      .await
      .is_err()
  );
  assert!(
    first
      .with_inactive_profile(profile.profile_id.clone(), || Ok(()))
      .await
      .is_err()
  );
  assert!(
    second
      .with_inactive_profile(profile.profile_id.clone(), || Ok(()))
      .await
      .is_err()
  );
  request.request_id = uuid::Uuid::new_v4().to_string();
  assert!(second.start(request.clone()).await.is_err());
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
  assert_eq!(first.inventory().runtimes[0].status, "cleanup_failed");
  first.shutdown().await;
  assert!(first.inventory().runtimes.is_empty());
  request.request_id = uuid::Uuid::new_v4().to_string();
  second.start(request).await.unwrap();
  assert_eq!(shared.opens.load(Ordering::SeqCst), 2);
  second.shutdown().await;
}
