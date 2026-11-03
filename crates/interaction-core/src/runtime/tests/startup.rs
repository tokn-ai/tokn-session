use super::*;

fn managed_fixture() -> (RuntimeCoordinator<FakeHost>, Arc<FakeHostState>, tempfile::TempDir) {
  let (mut runtime, shared) = fixture();
  let directory = tempfile::tempdir().unwrap();
  let inner = Arc::get_mut(&mut runtime.0).unwrap();
  inner.host.profile.ownership = Ownership::Managed;
  inner.lease_directory = Some(directory.path().join("leases"));
  (runtime, shared, directory)
}

fn lease_path(runtime: &RuntimeCoordinator<FakeHost>) -> PathBuf {
  runtime
    .0
    .lease_directory
    .as_ref()
    .unwrap()
    .join(format!("{}.lock", runtime.0.host.profile.profile_id))
}

fn start_request(runtime: &RuntimeCoordinator<FakeHost>) -> StartRuntimeRequest {
  StartRuntimeRequest {
    request_id: uuid::Uuid::new_v4().to_string(),
    profile_id: runtime.0.host.profile.profile_id.clone(),
    expected_revision: runtime.0.host.profile.revision,
  }
}

#[tokio::test]
async fn confirmed_failed_model_startup_releases_its_lease_and_allows_a_new_probe() {
  let (runtime, shared, _directory) = managed_fixture();
  *shared.open_failure.lock().unwrap() = Some(RuntimeOpenError::cleaned("Original child was reaped"));
  let error = runtime
    .creation_models(Provider::Codex, std::env::temp_dir(), None)
    .await
    .unwrap_err();
  assert!(!error.message.contains("reserved"));
  assert!(runtime.inventory().runtimes.is_empty());
  drop(tokn_session_storage::acquire_private_lock(&lease_path(&runtime)).unwrap());
  assert_eq!(
    runtime
      .creation_models(Provider::Codex, std::env::temp_dir(), None)
      .await
      .unwrap()[0]
      .model,
    test_model()
  );
  assert_eq!(shared.opens.load(Ordering::SeqCst), 2);
  assert!(runtime.inventory().runtimes.is_empty());
  runtime.shutdown().await;
}

#[tokio::test]
async fn confirmed_failed_runtime_startup_releases_its_lease_and_allows_a_new_attempt() {
  let (runtime, shared, _directory) = managed_fixture();
  *shared.open_failure.lock().unwrap() = Some(RuntimeOpenError::cleaned("Original child was reaped"));
  let error = runtime.start(start_request(&runtime)).await.unwrap_err();
  assert!(!error.contains("reserved"));
  assert!(runtime.inventory().runtimes.is_empty());
  drop(tokn_session_storage::acquire_private_lock(&lease_path(&runtime)).unwrap());
  runtime.start(start_request(&runtime)).await.unwrap();
  assert_eq!(shared.opens.load(Ordering::SeqCst), 2);
  assert_eq!(runtime.inventory().runtimes[0].status, "connected");
  runtime.shutdown().await;
}

#[tokio::test]
async fn unconfirmed_startup_failures_keep_the_original_reservation_and_refuse_retry() {
  for metadata in [false, true] {
    let (runtime, shared, _directory) = managed_fixture();
    *shared.open_failure.lock().unwrap() = Some(RuntimeOpenError::unconfirmed("No child cleanup proof"));
    if metadata {
      assert!(
        runtime
          .creation_models(Provider::Codex, std::env::temp_dir(), None)
          .await
          .is_err()
      );
    } else {
      assert!(runtime.start(start_request(&runtime)).await.is_err());
    }
    assert_eq!(runtime.inventory().runtimes[0].status, "cleanup_failed");
    assert!(tokn_session_storage::acquire_private_lock(&lease_path(&runtime)).is_err());
    assert!(runtime.start(start_request(&runtime)).await.is_err());
    assert!(
      runtime
        .creation_models(Provider::Codex, std::env::temp_dir(), None)
        .await
        .is_err()
    );
    assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
    runtime.shutdown().await;
  }
}

#[tokio::test]
async fn model_startup_timeout_never_invents_cleanup_proof() {
  let (runtime, shared, _directory) = managed_fixture();
  let (_release, wait) = oneshot::channel();
  *shared.open_wait.lock().unwrap() = Some(wait);
  let loading = tokio::spawn({
    let runtime = runtime.clone();
    async move {
      runtime
        .creation_models(Provider::Codex, std::env::temp_dir(), None)
        .await
    }
  });
  shared.open_started.notified().await;
  tokio::time::pause();
  tokio::time::advance(std::time::Duration::from_secs(31)).await;
  let error = loading.await.unwrap().unwrap_err();
  assert!(error.message.contains("startup timed out"));
  assert_eq!(runtime.inventory().runtimes[0].status, "cleanup_failed");
  assert!(tokn_session_storage::acquire_private_lock(&lease_path(&runtime)).is_err());
  assert!(runtime.start(start_request(&runtime)).await.is_err());
  assert_eq!(shared.opens.load(Ordering::SeqCst), 1);
  runtime.shutdown().await;
}
