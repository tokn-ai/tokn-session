use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::sync::oneshot;

fn test_model() -> ModelIdentity {
  ModelIdentity {
    model_provider: Some("provider".into()),
    model_id: "model".into(),
    display_name: Some("Model label".into()),
  }
}

#[tokio::test]
async fn draft_catalog_closes_temporary_runtime_without_creating_resuming_or_sending() {
  let (runtime, shared) = fixture();
  let models = runtime
    .creation_models(Provider::Codex, std::env::temp_dir(), None)
    .await
    .unwrap();
  assert_eq!(models[0].model, test_model());
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  assert_eq!(adapter.model_queries.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.closes.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.creates.load(Ordering::SeqCst), 0);
  assert_eq!(adapter.resumes.load(Ordering::SeqCst), 0);
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
  assert!(runtime.inventory().runtimes.is_empty());
  assert!(runtime.0.state.lock().unwrap().bindings.is_empty());
}

#[tokio::test]
async fn draft_catalog_reuses_live_runtime_and_never_closes_it_on_query_failure() {
  let (runtime, shared) = fixture();
  runtime
    .start(StartRuntimeRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      profile_id: runtime.0.host.profile.profile_id.clone(),
      expected_revision: 1,
    })
    .await
    .unwrap();
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  runtime
    .creation_models(Provider::Codex, std::env::temp_dir(), None)
    .await
    .unwrap();
  adapter.fail_models.store(true, Ordering::SeqCst);
  assert!(
    runtime
      .creation_models(Provider::Codex, std::env::temp_dir(), None)
      .await
      .is_err()
  );
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.closes.load(Ordering::SeqCst), 0);
  assert_eq!(runtime.inventory().runtimes.len(), 1);
  runtime.shutdown().await;
}

#[tokio::test]
async fn draft_catalog_failed_cleanup_reserves_profile_without_starting_a_second_probe() {
  let (runtime, shared) = fixture();
  shared.fail_close.store(true, Ordering::SeqCst);
  let error = runtime
    .creation_models(Provider::Codex, std::env::temp_dir(), None)
    .await
    .unwrap_err();
  assert!(error.message.contains("cleanup was not confirmed"));
  assert_eq!(runtime.inventory().runtimes[0].status, "cleanup_failed");
  assert!(
    runtime
      .creation_models(Provider::Codex, std::env::temp_dir(), None)
      .await
      .is_err()
  );
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn canceled_catalog_caller_still_cleans_up_and_shutdown_waits() {
  let (runtime, shared) = fixture();
  let (release, wait) = oneshot::channel();
  *shared.open_wait.lock().unwrap() = Some(wait);
  let probe = tokio::spawn({
    let runtime = runtime.clone();
    async move {
      runtime
        .creation_models(Provider::Codex, std::env::temp_dir(), None)
        .await
    }
  });
  shared.open_started.notified().await;
  probe.abort();
  let shutdown = tokio::spawn({
    let runtime = runtime.clone();
    async move { runtime.shutdown().await }
  });
  tokio::task::yield_now().await;
  assert!(!shutdown.is_finished());
  release.send(()).unwrap();
  shutdown.await.unwrap();
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  assert_eq!(adapter.closes.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.creates.load(Ordering::SeqCst), 0);
  assert!(runtime.inventory().runtimes.is_empty());
}

#[tokio::test]
async fn creation_revalidates_catalog_and_passes_only_selected_model_identity() {
  let (runtime, shared) = fixture();
  let created = runtime
    .create_native(Provider::Codex, std::env::temp_dir(), None, None, Some(test_model()))
    .await
    .unwrap();
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  assert_eq!(adapter.model_queries.load(Ordering::SeqCst), 1);
  assert_eq!(
    adapter.created_models.lock().unwrap()[0],
    Some(creation_model_identity(&test_model()))
  );
  assert!(!created.capabilities.atomic_model_on_send);
  let mut removed = test_model();
  removed.model_id = "no-longer-listed".into();
  assert_eq!(
    runtime
      .create_native(Provider::Codex, std::env::temp_dir(), None, None, Some(removed))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::InvalidRequest
  );
  adapter.fail_models.store(true, Ordering::SeqCst);
  assert!(
    runtime
      .create_native(Provider::Codex, std::env::temp_dir(), None, None, Some(test_model()))
      .await
      .is_err()
  );
  assert_eq!(adapter.creates.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
  runtime.shutdown().await;
}

#[tokio::test]
async fn unsupported_creation_model_is_rejected_before_runtime_start() {
  let (mut runtime, shared) = fixture();
  Arc::get_mut(&mut runtime.0).unwrap().host.profile.provider = Provider::Dsh;
  assert_eq!(
    runtime
      .create_native(Provider::Dsh, std::env::temp_dir(), None, None, Some(test_model()))
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unsupported
  );
  assert_eq!(shared.opens.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn verified_planned_creation_recovers_its_owner_without_repeating_native_writes() {
  let (runtime, shared) = fixture();
  let info = runtime
    .start(StartRuntimeRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      profile_id: runtime.0.host.profile.profile_id.clone(),
      expected_revision: 1,
    })
    .await
    .unwrap();
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  adapter.uncertain_create.store(true, Ordering::SeqCst);
  let cwd = std::env::temp_dir();
  assert_eq!(
    runtime
      .create_native(Provider::Codex, cwd.clone(), None, Some("planned-created".into()), None)
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Uncertain
  );
  assert!(runtime.binding("verified-created").is_none());
  let recovered = runtime
    .adopt_verified_creation(
      Provider::Codex,
      "planned-created",
      cwd.clone(),
      "verified-created".into(),
    )
    .await
    .unwrap()
    .unwrap();
  assert_eq!(runtime.binding("verified-created").unwrap().target, recovered.target);
  assert_eq!(recovered.target.runtime_id, info.runtime_id);
  assert_eq!(recovered.target.instance_id, info.instance_id);
  assert_eq!(recovered.native.working_directory, cwd);
  assert!(
    runtime
      .adopt_verified_creation(
        Provider::Codex,
        "unknown-created",
        cwd.clone(),
        "unknown-history".into()
      )
      .await
      .unwrap()
      .is_none()
  );
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.creates.load(Ordering::SeqCst), 1);
  assert_eq!(adapter.resumes.load(Ordering::SeqCst), 0);
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 0);
  assert_eq!(adapter.closes.load(Ordering::SeqCst), 0);
  adapter.unavailable_state.store(true, Ordering::SeqCst);
  assert!(
    runtime
      .adopt_verified_creation(
        Provider::Codex,
        "planned-created",
        cwd.clone(),
        "unrecovered-history".into()
      )
      .await
      .is_err()
  );
  assert!(runtime.binding("unrecovered-history").is_none());
  adapter.unavailable_state.store(false, Ordering::SeqCst);
  runtime
    .close(CloseRuntimeRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      runtime_id: info.runtime_id,
      expected_instance_id: info.instance_id,
    })
    .await
    .unwrap();
  assert!(
    runtime
      .adopt_verified_creation(Provider::Codex, "planned-created", cwd, "restart-history".into())
      .await
      .unwrap()
      .is_none()
  );
}

#[tokio::test]
async fn created_sessions_share_a_runtime_and_adoption_never_resumes_or_stops_it() {
  let (runtime, shared) = fixture();
  let first = runtime
    .create_native(
      Provider::Codex,
      std::env::temp_dir(),
      None,
      Some("created-one".into()),
      None,
    )
    .await
    .unwrap();
  let second = runtime
    .create_native(
      Provider::Codex,
      std::env::temp_dir(),
      None,
      Some("created-two".into()),
      None,
    )
    .await
    .unwrap();
  assert_eq!(first.target.runtime_id, second.target.runtime_id);
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
  runtime
    .execute_created(
      &first,
      uuid::Uuid::new_v4().to_string(),
      Command::Message {
        input: MessageInput {
          text: "first prompt".into(),
          ..Default::default()
        },
        delivery: DeliveryMode::StartOrSteer,
      },
    )
    .await
    .unwrap();
  let binding = runtime.adopt_created(&first, "verified-one".into()).await.unwrap();
  runtime.adopt_created(&second, "verified-two".into()).await.unwrap();
  assert_eq!(binding.target, first.target);
  let adapter = shared.adapter.lock().unwrap().clone().unwrap();
  assert_eq!(adapter.resumes.load(Ordering::SeqCst), 0);
  assert_eq!(adapter.sends.load(Ordering::SeqCst), 1);
  runtime.release_binding("verified-one").await.unwrap();
  assert!(runtime.binding("verified-two").is_some());
  assert_eq!(adapter.closes.load(Ordering::SeqCst), 0);
  runtime.shutdown().await;
  assert_eq!(adapter.closes.load(Ordering::SeqCst), 1);
  assert_eq!(
    runtime
      .execute_created(&second, uuid::Uuid::new_v4().to_string(), Command::State)
      .await
      .unwrap_err()
      .kind,
    ErrorKind::Unavailable
  );
}

#[tokio::test]
async fn creation_reserves_native_identity_across_hosts_until_exact_runtime_cleanup() {
  let directory = tempfile::tempdir().unwrap();
  let (mut first, _) = fixture();
  let (mut second, _) = fixture();
  first
    .with_session_ownership(SessionOwnership::new(Some(directory.path().to_path_buf())).unwrap())
    .unwrap();
  second
    .with_session_ownership(SessionOwnership::new(Some(directory.path().to_path_buf())).unwrap())
    .unwrap();
  let created = first
    .create_native(
      Provider::Codex,
      std::env::temp_dir(),
      None,
      Some("exclusive-created".into()),
      None,
    )
    .await
    .unwrap();
  assert!(
    second
      .create_native(
        Provider::Codex,
        std::env::temp_dir(),
        None,
        Some("exclusive-created".into()),
        None,
      )
      .await
      .is_err()
  );
  assert_eq!(
    first.inventory().runtimes[0].session_count,
    0,
    "ownership precedes a public history binding"
  );
  first
    .close(CloseRuntimeRequest {
      request_id: uuid::Uuid::new_v4().to_string(),
      runtime_id: created.target.runtime_id,
      expected_instance_id: created.target.instance_id,
    })
    .await
    .unwrap();
  second
    .create_native(
      Provider::Codex,
      std::env::temp_dir(),
      None,
      Some("exclusive-created".into()),
      None,
    )
    .await
    .unwrap();
  second.shutdown().await;
}

struct FakeHost {
  profile: ProfileDescriptor,
  shared: Arc<FakeHostState>,
}

struct FakeHostState {
  opens: AtomicUsize,
  fail_close: AtomicBool,
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

  fn validate_creation_model(&self, profile: &Self::Profile, _model: &ModelIdentity) -> Result<(), InteractionError> {
    if profile.provider == Provider::Dsh {
      Err(InteractionError::unsupported("No creation model override"))
    } else {
      Ok(())
    }
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
            .with(Operation::Models)
            .with(Operation::State)
            .with(Operation::PendingInteractions)
            .with(Operation::Message),
        },
        events: broadcast::channel(8).0,
        creates: AtomicUsize::new(0),
        created_models: Mutex::new(Vec::new()),
        model_queries: AtomicUsize::new(0),
        fail_models: AtomicBool::new(false),
        fail_close: AtomicBool::new(self.shared.fail_close.load(Ordering::SeqCst)),
        uncertain_create: AtomicBool::new(false),
        unavailable_state: AtomicBool::new(false),
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

  fn create_native<'a>(
    &'a self,
    request: &'a CreateNativeSessionRequest,
  ) -> AdapterFuture<'a, Result<NativeSessionDescriptor, InteractionError>> {
    Box::pin(async move {
      self.0.creates.fetch_add(1, Ordering::SeqCst);
      self.0.created_models.lock().unwrap().push(request.model.clone());
      if self.0.uncertain_create.load(Ordering::SeqCst) {
        return Err(InteractionError::uncertain("Native create acknowledgement was lost"));
      }
      Ok(NativeSessionDescriptor {
        native_id: request
          .native_id
          .clone()
          .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        working_directory: request.working_directory.clone(),
        history_path: None,
      })
    })
  }
}

struct FakeAdapter {
  descriptor: RuntimeDescriptor,
  events: broadcast::Sender<RuntimeEvent>,
  creates: AtomicUsize,
  created_models: Mutex<Vec<Option<ModelIdentity>>>,
  model_queries: AtomicUsize,
  fail_models: AtomicBool,
  fail_close: AtomicBool,
  uncertain_create: AtomicBool,
  unavailable_state: AtomicBool,
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
      if matches!(request.command, Command::State) && self.unavailable_state.load(Ordering::SeqCst) {
        return Err(InteractionError::unavailable("Native state recovery failed"));
      }
      Ok(match request.command {
        Command::Metadata => Reply::Metadata(Metadata::default()),
        Command::Models => {
          assert!(
            request.target.session_id.is_none(),
            "draft catalogs must never bind a native session"
          );
          self.model_queries.fetch_add(1, Ordering::SeqCst);
          if self.fail_models.load(Ordering::SeqCst) {
            return Err(InteractionError::unavailable("Catalog unavailable"));
          }
          Reply::Models(vec![ModelChoice {
            model: test_model(),
            supports_images: true,
          }])
        }
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
      if self.fail_close.load(Ordering::SeqCst) {
        return Err(InteractionError::unavailable("Cleanup not confirmed"));
      }
      Ok(())
    })
  }
}

fn fixture() -> (RuntimeCoordinator<FakeHost>, Arc<FakeHostState>) {
  let shared = Arc::new(FakeHostState {
    opens: AtomicUsize::new(0),
    fail_close: AtomicBool::new(false),
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
