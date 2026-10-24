//! Session admission fences and atomic changes between host-provided routes.
//! Hosts supply route persistence and native idle checks; the core owns their
//! ordering, deduplication and canonical ownership for the whole transition.
use crate::session_ownership::{SessionLease, SessionOwnership};
use crate::{AdapterFuture, Provider, ReceiptAdmission, ReceiptLedger, admission::InputBroker};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
  collections::HashMap,
  sync::{Arc, Mutex, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTransport {
  #[default]
  Rpc,
  Desktop,
}

/// Construct only after verifying the host's native session identity.
#[derive(Clone)]
pub struct SessionIdentity {
  pub scope: String,
  pub provider: Provider,
  pub native_id: String,
}

pub trait SessionRouteHost: Send + Sync {
  fn current(&self) -> AdapterFuture<'_, Result<SessionTransport, String>>;
  /// Prove idle and release this session's binding, preserving shared runtimes.
  fn release_idle_binding(&self) -> AdapterFuture<'_, Result<(), String>>;
  fn save(&self, transport: SessionTransport) -> AdapterFuture<'_, Result<(), String>>;
}

#[derive(Clone)]
pub struct SessionCoordinator {
  ownership: SessionOwnership,
  gates: Arc<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>>,
  route_owners: Arc<Mutex<HashMap<String, RouteOwner>>>,
  receipts: Arc<Mutex<ReceiptLedger<Result<(), String>>>>,
  lifecycle: Arc<CoordinatorLifecycle>,
}

struct RouteOwner {
  provider: Provider,
  native_id: String,
  lease: SessionLease,
}

impl RouteOwner {
  fn lease_for(&self, identity: &SessionIdentity) -> Result<SessionLease, String> {
    if self.provider != identity.provider || self.native_id != identity.native_id {
      return Err("Session ownership identity changed; verify its native identity before continuing".into());
    }
    Ok(self.lease.clone())
  }
}

#[derive(Default)]
struct CoordinatorLifecycle {
  state: Mutex<LifecycleState>,
  settled: Notify,
}

#[derive(Default)]
struct LifecycleState {
  stopping: bool,
  active: usize,
}

struct AdmittedOperation(Arc<CoordinatorLifecycle>);

impl Drop for AdmittedOperation {
  fn drop(&mut self) {
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    state.active = state.active.saturating_sub(1);
    drop(state);
    self.0.settled.notify_waiters();
  }
}

#[cfg(test)]
mod tests;

impl SessionCoordinator {
  pub fn new(ownership: SessionOwnership) -> Self {
    Self {
      ownership,
      gates: Default::default(),
      route_owners: Default::default(),
      receipts: Arc::new(Mutex::new(ReceiptLedger::new(256))),
      lifecycle: Default::default(),
    }
  }

  fn admit_operation(&self) -> Result<AdmittedOperation, String> {
    let mut state = self.lifecycle.state.lock().unwrap_or_else(|error| error.into_inner());
    if state.stopping {
      return Err("Session coordinator is shutting down".into());
    }
    state.active += 1;
    Ok(AdmittedOperation(self.lifecycle.clone()))
  }

  /// Close admission before shutting down host runtimes. Already admitted
  /// changes retain their host dependencies until `drain_transitions` settles.
  pub fn close_admission(&self) {
    self
      .lifecycle
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .stopping = true;
  }

  pub async fn drain_transitions(&self) {
    loop {
      let settled = self.lifecycle.settled.notified();
      tokio::pin!(settled);
      settled.as_mut().enable();
      if self
        .lifecycle
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .active
        == 0
      {
        return;
      }
      settled.await;
    }
  }

  pub async fn gate(&self, scope: &str) -> OwnedMutexGuard<()> {
    let gate = {
      let mut gates = self.gates.lock().unwrap_or_else(|error| error.into_inner());
      gates.retain(|_, gate| gate.strong_count() > 0);
      match gates.get(scope).and_then(Weak::upgrade) {
        Some(gate) => gate,
        None => {
          let gate = Arc::new(AsyncMutex::new(()));
          gates.insert(scope.to_owned(), Arc::downgrade(&gate));
          gate
        }
      }
    };
    gate.lock_owned().await
  }

  /// Keep an admitted transition alive if its requesting frontend disconnects.
  pub async fn set_transport(
    &self,
    identity: SessionIdentity,
    request_id: String,
    transport: SessionTransport,
    broker: InputBroker,
    broker_scope: String,
    host: Arc<dyn SessionRouteHost>,
  ) -> Result<(), String> {
    uuid::Uuid::parse_str(&request_id).map_err(|_| "Invalid session control request ID")?;
    let admitted = self.admit_operation()?;
    let coordinator = self.clone();
    tokio::spawn(async move {
      let _admitted = admitted;
      let _guard = coordinator.gate(&identity.scope).await;
      let fingerprint = Sha256::digest(
        serde_json::to_vec(&(identity.provider, &identity.native_id, transport))
          .expect("transport identity serializes"),
      )
      .into();
      {
        let mut receipts = coordinator.receipts.lock().unwrap_or_else(|error| error.into_inner());
        match receipts.admit(
          request_id.clone(),
          identity.scope.clone(),
          fingerprint,
          Err("Session transport change is still pending".into()),
        ) {
          Ok(ReceiptAdmission::Replay(previous)) => return previous,
          Ok(ReceiptAdmission::New) => {}
          Err(_) => return Err("Session control request ID conflicts or its receipt capacity is full".into()),
        }
      }
      let result = coordinator
        .change_transport(&identity, transport, &broker, &broker_scope, host.as_ref())
        .await;
      coordinator
        .receipts
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .complete(&request_id, result.clone());
      result
    })
    .await
    .map_err(|_| "Session transport change was interrupted; inspect its status before retrying".to_owned())?
  }

  async fn change_transport(
    &self,
    identity: &SessionIdentity,
    transport: SessionTransport,
    broker: &InputBroker,
    broker_scope: &str,
    host: &dyn SessionRouteHost,
  ) -> Result<(), String> {
    let _transition = broker.begin_transition(broker_scope)?;
    if host.current().await? == transport {
      return Ok(());
    }
    let existing = self
      .route_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(&identity.scope)
      .map(|owner| owner.lease_for(identity))
      .transpose()?;
    host.release_idle_binding().await?;
    let _lease = match existing {
      Some(lease) => lease,
      None => self.acquire(identity).await?,
    };
    host.save(transport).await?;
    self
      .route_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .remove(&identity.scope);
    Ok(())
  }

  async fn acquire(&self, identity: &SessionIdentity) -> Result<SessionLease, String> {
    let ownership = self.ownership.clone();
    let provider = identity.provider;
    let native_id = identity.native_id.clone();
    tokio::task::spawn_blocking(move || ownership.acquire(provider, &native_id))
      .await
      .map_err(|_| "Could not check session ownership")?
  }

  /// Call while holding the session admission gate. Retain the returned lease
  /// through native acknowledgement even if route ownership is later released.
  pub async fn claim_route(&self, identity: &SessionIdentity) -> Result<SessionLease, String> {
    let _admitted = self.admit_operation()?;
    {
      let owners = self.route_owners.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(lease) = owners.get(&identity.scope) {
        return lease.lease_for(identity);
      }
      if owners.len() >= 1024 {
        return Err("Session ownership capacity reached; release an idle session first".into());
      }
    }
    let lease = self.acquire(identity).await?;
    self
      .route_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .insert(
        identity.scope.clone(),
        RouteOwner {
          provider: identity.provider,
          native_id: identity.native_id.clone(),
          lease: lease.clone(),
        },
      );
    Ok(lease)
  }

  pub fn route_owner_count(&self) -> usize {
    self
      .route_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .len()
  }

  /// Invoke after native runtimes and in-flight deliveries have settled.
  pub fn release_routes(&self) {
    self
      .route_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .clear();
  }

  /// Hosts close admission and drain transitions before stopping native
  /// runtimes, then call this to release their remaining route ownership.
  /// Caller cancellation never reopens admission; a later call can finish.
  pub async fn shutdown(&self) {
    self.close_admission();
    self.drain_transitions().await;
    self.release_routes();
  }
}
