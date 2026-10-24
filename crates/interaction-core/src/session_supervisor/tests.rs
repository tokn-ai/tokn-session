use super::*;
use crate::{admission::SubmitSessionInputRequest, managed_sessions::Admission};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

struct TestHost {
  route: Mutex<SessionTransport>,
  releases: AtomicUsize,
  saves: AtomicUsize,
  released: Semaphore,
  release_gate: Option<Semaphore>,
}
impl TestHost {
  fn new(block_release: bool) -> Arc<Self> {
    Arc::new(Self {
      route: Mutex::new(SessionTransport::Rpc),
      releases: AtomicUsize::new(0),
      saves: AtomicUsize::new(0),
      released: Semaphore::new(0),
      release_gate: block_release.then(|| Semaphore::new(0)),
    })
  }
}
impl SessionRouteHost for TestHost {
  fn current(&self) -> AdapterFuture<'_, Result<SessionTransport, String>> {
    Box::pin(async { Ok(*self.route.lock().unwrap()) })
  }
  fn release_idle_binding(&self) -> AdapterFuture<'_, Result<(), String>> {
    Box::pin(async {
      self.releases.fetch_add(1, Ordering::SeqCst);
      self.released.add_permits(1);
      if let Some(gate) = &self.release_gate {
        gate.acquire().await.unwrap().forget();
      }
      Ok(())
    })
  }
  fn save(&self, route: SessionTransport) -> AdapterFuture<'_, Result<(), String>> {
    Box::pin(async move {
      self.saves.fetch_add(1, Ordering::SeqCst);
      *self.route.lock().unwrap() = route;
      Ok(())
    })
  }
}
fn identity() -> SessionIdentity {
  SessionIdentity {
    scope: "verified-opaque-session".into(),
    provider: Provider::Dsh,
    native_id: "native-id".into(),
  }
}
fn input() -> SubmitSessionInputRequest {
  SubmitSessionInputRequest {
    session_key: "host-key".into(),
    request_id: uuid::Uuid::new_v4().to_string(),
    text: "prompt".into(),
    image_ids: Vec::new(),
    model_selection: None,
  }
}

#[tokio::test]
async fn concurrent_route_retries_save_once_and_conflicting_ids_do_not_mutate() {
  let coordinator = SessionCoordinator::new(SessionOwnership::new(None).unwrap());
  let host = TestHost::new(false);
  let broker = InputBroker::default();
  let request_id = uuid::Uuid::new_v4().to_string();
  let mut calls = tokio::task::JoinSet::new();
  for _ in 0..8 {
    let (coordinator, host, broker, request_id) =
      (coordinator.clone(), host.clone(), broker.clone(), request_id.clone());
    calls.spawn(async move {
      coordinator
        .set_transport(
          identity(),
          request_id,
          SessionTransport::Desktop,
          broker,
          "broker-scope".into(),
          host,
        )
        .await
    });
  }
  while let Some(result) = calls.join_next().await {
    result.unwrap().unwrap();
  }
  assert_eq!(host.releases.load(Ordering::SeqCst), 1);
  assert_eq!(host.saves.load(Ordering::SeqCst), 1);
  assert!(
    coordinator
      .set_transport(
        identity(),
        request_id,
        SessionTransport::Rpc,
        broker,
        "broker-scope".into(),
        host.clone()
      )
      .await
      .unwrap_err()
      .contains("conflicts")
  );
  assert_eq!(*host.route.lock().unwrap(), SessionTransport::Desktop);
  assert_eq!(host.saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn in_flight_message_fences_route_transition_before_idle_release() {
  let coordinator = SessionCoordinator::new(SessionOwnership::new(None).unwrap());
  let host = TestHost::new(false);
  let broker = InputBroker::default();
  let delivery_broker = broker.clone();
  let (started, wait_started) = tokio::sync::oneshot::channel();
  let (complete, wait_complete) = tokio::sync::oneshot::channel();
  let delivery = tokio::spawn(async move {
    delivery_broker
      .submit_custom("broker-scope", input(), async move {
        started.send(()).unwrap();
        wait_complete.await.unwrap();
        Ok(Admission::Accepted)
      })
      .await
  });
  wait_started.await.unwrap();
  let request_id = uuid::Uuid::new_v4().to_string();
  let change = || {
    coordinator.set_transport(
      identity(),
      request_id.clone(),
      SessionTransport::Desktop,
      broker.clone(),
      "broker-scope".into(),
      host.clone(),
    )
  };
  assert!(change().await.unwrap_err().contains("pending"));
  assert_eq!(host.releases.load(Ordering::SeqCst), 0);
  complete.send(()).unwrap();
  delivery.await.unwrap();
  assert!(change().await.is_err(), "failed route admission remains a receipt");
  coordinator
    .set_transport(
      identity(),
      uuid::Uuid::new_v4().to_string(),
      SessionTransport::Desktop,
      broker,
      "broker-scope".into(),
      host.clone(),
    )
    .await
    .unwrap();
  assert_eq!(host.saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn canonical_native_owner_conflict_prevents_saving_the_new_route() {
  let ownership = SessionOwnership::new(None).unwrap();
  let external = ownership.acquire(Provider::Dsh, "native-id").unwrap();
  let coordinator = SessionCoordinator::new(ownership.clone());
  let host = TestHost::new(false);
  let broker = InputBroker::default();
  assert!(
    coordinator
      .set_transport(
        identity(),
        uuid::Uuid::new_v4().to_string(),
        SessionTransport::Desktop,
        broker.clone(),
        "broker-scope".into(),
        host.clone()
      )
      .await
      .unwrap_err()
      .contains("owner")
  );
  assert_eq!(host.saves.load(Ordering::SeqCst), 0);
  assert_eq!(*host.route.lock().unwrap(), SessionTransport::Rpc);
  drop(external);
  coordinator
    .set_transport(
      identity(),
      uuid::Uuid::new_v4().to_string(),
      SessionTransport::Desktop,
      broker,
      "broker-scope".into(),
      host.clone(),
    )
    .await
    .unwrap();
  assert!(
    ownership.acquire(Provider::Dsh, "native-id").is_ok(),
    "route transition releases its temporary owner after saving"
  );
}

#[tokio::test]
async fn caller_cancellation_keeps_admitted_route_change_alive_and_replayable() {
  let coordinator = SessionCoordinator::new(SessionOwnership::new(None).unwrap());
  let host = TestHost::new(true);
  let broker = InputBroker::default();
  let request_id = uuid::Uuid::new_v4().to_string();
  let (task_coordinator, task_host, task_broker, task_id) =
    (coordinator.clone(), host.clone(), broker.clone(), request_id.clone());
  let caller = tokio::spawn(async move {
    task_coordinator
      .set_transport(
        identity(),
        task_id,
        SessionTransport::Desktop,
        task_broker,
        "broker-scope".into(),
        task_host,
      )
      .await
  });
  host.released.acquire().await.unwrap().forget();
  caller.abort();
  assert!(broker.begin_transition("broker-scope").is_err());
  host.release_gate.as_ref().unwrap().add_permits(1);
  coordinator
    .set_transport(
      identity(),
      request_id,
      SessionTransport::Desktop,
      broker.clone(),
      "broker-scope".into(),
      host.clone(),
    )
    .await
    .unwrap();
  assert_eq!(host.saves.load(Ordering::SeqCst), 1);
  assert!(broker.begin_transition("broker-scope").is_ok());
}

#[tokio::test]
async fn route_ownership_clones_protect_native_acknowledgement_after_registry_release() {
  let ownership = SessionOwnership::new(None).unwrap();
  let coordinator = SessionCoordinator::new(ownership.clone());
  let identity = identity();
  let _gate = coordinator.gate(&identity.scope).await;
  let in_flight = coordinator.claim_route(&identity).await.unwrap();
  assert_eq!(coordinator.route_owner_count(), 1);
  assert!(ownership.acquire(identity.provider, &identity.native_id).is_err());
  coordinator.release_routes();
  assert!(ownership.acquire(identity.provider, &identity.native_id).is_err());
  drop(in_flight);
  assert!(ownership.acquire(identity.provider, &identity.native_id).is_ok());
}

#[tokio::test]
async fn shutdown_drains_admitted_changes_rejects_new_owners_and_survives_cancellation() {
  let coordinator = SessionCoordinator::new(SessionOwnership::new(None).unwrap());
  let host = TestHost::new(true);
  let broker = InputBroker::default();
  let owned = coordinator.claim_route(&identity()).await.unwrap();
  drop(owned);
  let (task_coordinator, task_host, task_broker) = (coordinator.clone(), host.clone(), broker.clone());
  let transition = tokio::spawn(async move {
    task_coordinator
      .set_transport(
        identity(),
        uuid::Uuid::new_v4().to_string(),
        SessionTransport::Desktop,
        task_broker,
        "broker-scope".into(),
        task_host,
      )
      .await
  });
  host.released.acquire().await.unwrap().forget();
  coordinator.close_admission();
  assert!(
    coordinator
      .claim_route(&identity())
      .await
      .unwrap_err()
      .contains("shutting down")
  );
  assert!(
    coordinator
      .set_transport(
        identity(),
        uuid::Uuid::new_v4().to_string(),
        SessionTransport::Rpc,
        broker.clone(),
        "broker-scope".into(),
        host.clone()
      )
      .await
      .unwrap_err()
      .contains("shutting down")
  );
  let task_coordinator = coordinator.clone();
  let (started, wait_started) = tokio::sync::oneshot::channel();
  let shutdown = tokio::spawn(async move {
    started.send(()).unwrap();
    task_coordinator.shutdown().await;
  });
  wait_started.await.unwrap();
  tokio::task::yield_now().await;
  assert!(!shutdown.is_finished());
  assert_eq!(
    coordinator.route_owner_count(),
    1,
    "admitted changes retain their route owner until save completes"
  );
  shutdown.abort();
  host.release_gate.as_ref().unwrap().add_permits(1);
  transition.await.unwrap().unwrap();
  tokio::time::timeout(std::time::Duration::from_secs(1), coordinator.shutdown())
    .await
    .unwrap();
  assert_eq!(host.saves.load(Ordering::SeqCst), 1);
  assert_eq!(coordinator.route_owner_count(), 0);
}

#[tokio::test]
async fn opaque_scopes_cannot_reuse_native_ownership_or_receipts_for_another_identity() {
  let coordinator = SessionCoordinator::new(SessionOwnership::new(None).unwrap());
  let host = TestHost::new(false);
  let broker = InputBroker::default();
  let owned = coordinator.claim_route(&identity()).await.unwrap();
  drop(owned);
  let mut different = identity();
  different.native_id = "another-native-id".into();
  assert!(
    coordinator
      .claim_route(&different)
      .await
      .unwrap_err()
      .contains("identity changed")
  );
  assert!(
    coordinator
      .set_transport(
        different.clone(),
        uuid::Uuid::new_v4().to_string(),
        SessionTransport::Desktop,
        broker.clone(),
        "broker-scope".into(),
        host.clone()
      )
      .await
      .unwrap_err()
      .contains("identity changed")
  );
  assert_eq!(host.releases.load(Ordering::SeqCst), 0);
  let request_id = uuid::Uuid::new_v4().to_string();
  coordinator
    .set_transport(
      identity(),
      request_id.clone(),
      SessionTransport::Desktop,
      broker.clone(),
      "broker-scope".into(),
      host.clone(),
    )
    .await
    .unwrap();
  assert!(
    coordinator
      .set_transport(
        different,
        request_id,
        SessionTransport::Desktop,
        broker,
        "broker-scope".into(),
        host.clone()
      )
      .await
      .unwrap_err()
      .contains("conflicts")
  );
  assert_eq!(host.saves.load(Ordering::SeqCst), 1);
}
