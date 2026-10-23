//! Host-owned runtime factories and ephemeral, instance-fenced session bindings.
mod factory;
#[cfg(test)]
mod tests;

use crate::rpc_profiles::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
  collections::BTreeMap,
  fs::File,
  path::PathBuf,
  sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
  },
};
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast};
use tokn_interaction_core::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcStartRuntimeRequest {
  pub request_id: String,
  pub profile_id: String,
  pub expected_revision: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcCloseRuntimeRequest {
  pub request_id: String,
  pub runtime_id: String,
  pub expected_instance_id: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RpcCloseRuntimeResponse {
  pub runtime_id: String,
  pub instance_id: String,
  pub closed: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct RpcRuntimeInfo {
  pub runtime_id: String,
  pub instance_id: String,
  pub profile_id: String,
  pub profile_revision: u64,
  pub name: String,
  pub provider: Provider,
  pub ownership: Ownership,
  pub status: String,
  pub capabilities: Capabilities,
  pub session_count: usize,
  pub last_error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RpcRuntimeInventory {
  pub version: u32,
  pub runtimes: Vec<RpcRuntimeInfo>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcBindSessionRequest {
  pub request_id: String,
  pub session_key: String,
  pub profile_id: String,
  pub expected_instance_id: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcSessionBindingRequest {
  pub session_key: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RpcSessionBinding {
  pub session_key: String,
  pub profile_id: String,
  pub target: SessionTarget,
  pub capabilities: Capabilities,
}

#[derive(Clone)]
pub(crate) struct RpcRuntimeManager(Arc<Inner>);
struct Inner {
  profiles: Arc<RpcProfileStore>,
  interactions: InteractionService,
  lease_directory: Option<PathBuf>,
  state: Mutex<State>,
  changes: broadcast::Sender<()>,
  stopping: AtomicBool,
  settled: Notify,
}
struct State {
  runtimes: BTreeMap<String, Record>,
  quarantined: BTreeMap<String, QuarantinedRecord>,
  bindings: BTreeMap<String, RpcSessionBinding>,
  binding_owners: BTreeMap<String, (String, String)>,
  gates: BTreeMap<String, Weak<AsyncMutex<()>>>,
  receipts: ReceiptLedger<Result<ActionReply, String>>,
  active_actions: usize,
}
struct Record {
  info: RpcRuntimeInfo,
  handle: factory::Handle,
  _lease: Option<File>,
  pump: tokio::task::JoinHandle<()>,
}
struct QuarantinedRecord {
  info: RpcRuntimeInfo,
  _handle: Option<factory::Handle>,
  _lease: Option<File>,
}
impl Drop for Record {
  fn drop(&mut self) {
    self.pump.abort();
  }
}
#[derive(Clone)]
enum ActionReply {
  Started(RpcRuntimeInfo),
  Closed(RpcCloseRuntimeResponse),
  Bound(RpcSessionBinding),
}

struct ActiveAction {
  inner: Arc<Inner>,
  id: String,
  complete: bool,
}
impl Drop for ActiveAction {
  fn drop(&mut self) {
    let mut state = self.inner.state.lock().unwrap_or_else(|error| error.into_inner());
    if !self.complete {
      state.receipts.retain_outcome(
        &self.id,
        Err("Runtime action was interrupted; reconcile its state before another attempt".into()),
      );
    }
    state.active_actions = state.active_actions.saturating_sub(1);
    drop(state);
    self.inner.settled.notify_waiters();
  }
}

impl RpcRuntimeManager {
  pub(crate) fn new(path: Option<PathBuf>, interactions: InteractionService) -> Self {
    let lease_directory = path
      .as_ref()
      .and_then(|path| path.parent())
      .map(|path| path.join("rpc-leases"));
    Self(Arc::new(Inner {
      profiles: Arc::new(RpcProfileStore::configured(path)),
      interactions,
      lease_directory,
      state: Mutex::new(State {
        runtimes: BTreeMap::new(),
        quarantined: BTreeMap::new(),
        bindings: BTreeMap::new(),
        binding_owners: BTreeMap::new(),
        gates: BTreeMap::new(),
        receipts: ReceiptLedger::new(512),
        active_actions: 0,
      }),
      changes: broadcast::channel(32).0,
      stopping: AtomicBool::new(false),
      settled: Notify::new(),
    }))
  }
  pub(crate) fn profiles(&self) -> Result<Arc<RpcProfileStore>, String> {
    Ok(self.0.profiles.clone())
  }
  pub(crate) fn subscribe(&self) -> broadcast::Receiver<()> {
    self.0.changes.subscribe()
  }
  pub(crate) fn changed(&self) {
    let _ = self.0.changes.send(());
  }
  fn ensure_open(&self) -> Result<(), String> {
    if self.0.stopping.load(Ordering::Acquire) {
      Err("Runtime manager is shutting down".into())
    } else {
      Ok(())
    }
  }
  fn gate(&self, id: &str) -> Arc<AsyncMutex<()>> {
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    state.gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = state.gates.get(id).and_then(Weak::upgrade) {
      return gate;
    }
    let gate = Arc::new(AsyncMutex::new(()));
    state.gates.insert(id.to_owned(), Arc::downgrade(&gate));
    gate
  }
  pub(crate) fn inventory(&self) -> RpcRuntimeInventory {
    let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    RpcRuntimeInventory {
      version: 1,
      runtimes: state
        .runtimes
        .values()
        .map(|record| &record.info)
        .chain(state.quarantined.values().map(|record| &record.info))
        .map(|record| {
          let mut info = record.clone();
          info.session_count = state
            .bindings
            .values()
            .filter(|binding| binding.target.runtime_id == info.runtime_id)
            .count();
          info
        })
        .collect(),
    }
  }
  pub(crate) async fn remove_profile(
    &self,
    request: RpcRemoveProfileRequest,
  ) -> Result<RpcRuntimeProfilesSnapshot, String> {
    validate_uuid(&request.profile_id)?;
    self.ensure_open()?;
    let gate = self.gate(&request.profile_id);
    let guard = gate.lock_owned().await;
    self.ensure_open()?;
    {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if state
        .quarantined
        .values()
        .any(|record| record.info.profile_id == request.profile_id)
      {
        return Err("Runtime cleanup was not confirmed; this profile remains reserved until host shutdown".into());
      }
      if state
        .runtimes
        .values()
        .any(|record| record.info.profile_id == request.profile_id)
      {
        return Err("Stop or detach this runtime before deleting its profile".into());
      }
    }
    let profiles = self.profiles()?;
    let lease_path = self
      .0
      .lease_directory
      .as_ref()
      .map(|directory| directory.join(format!("{}.lock", request.profile_id)));
    let result = tokio::task::spawn_blocking(move || {
      // Cancellation must not release the profile gate before the write ends.
      let _guard = guard;
      let _lease = lease_path
        .map(|path| tokn_session_storage::acquire_private_lock(&path))
        .transpose()?;
      profiles.remove(request)
    })
    .await
    .map_err(|_| "Could not remove runtime profile")??;
    self.changed();
    Ok(result)
  }
  pub(crate) async fn start(&self, request: RpcStartRuntimeRequest) -> Result<RpcRuntimeInfo, String> {
    validate_uuid(&request.request_id)?;
    validate_uuid(&request.profile_id)?;
    let fingerprint: [u8; 32] =
      Sha256::digest(serde_json::to_vec(&request).map_err(|_| "Invalid start request")?).into();
    let manager = self.clone();
    match self
      .action(&request.request_id, "start", fingerprint, {
        let request = request.clone();
        async move { manager.start_inner(request).await.map(ActionReply::Started) }
      })
      .await?
    {
      ActionReply::Started(info) => Ok(info),
      _ => Err("Request ID belongs to another runtime action".into()),
    }
  }
  pub(crate) async fn close(&self, request: RpcCloseRuntimeRequest) -> Result<RpcCloseRuntimeResponse, String> {
    validate_uuid(&request.request_id)?;
    if request.runtime_id.len() > 512 || request.expected_instance_id.len() > 512 {
      return Err("Invalid runtime identity".into());
    }
    let fingerprint: [u8; 32] =
      Sha256::digest(serde_json::to_vec(&request).map_err(|_| "Invalid close request")?).into();
    let manager = self.clone();
    match self
      .action(&request.request_id, "close", fingerprint, {
        let request = request.clone();
        async move { manager.close_inner(request).await.map(ActionReply::Closed) }
      })
      .await?
    {
      ActionReply::Closed(info) => Ok(info),
      _ => Err("Request ID belongs to another runtime action".into()),
    }
  }
  async fn action(
    &self,
    id: &str,
    scope: &str,
    fingerprint: [u8; 32],
    operation: impl std::future::Future<Output = Result<ActionReply, String>> + Send + 'static,
  ) -> Result<ActionReply, String> {
    {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(result) = state
        .receipts
        .lookup(id, scope, &fingerprint)
        .map_err(|_| "Runtime action ID conflicts")?
      {
        return result;
      }
      self.ensure_open()?;
      state
        .receipts
        .admit(
          id.to_owned(),
          scope.to_owned(),
          fingerprint,
          Err("Runtime action is still pending; check status before retrying".into()),
        )
        .map_err(|_| "Runtime action receipt limit reached")?;
      state.active_actions += 1;
    }
    let manager = self.clone();
    let id = id.to_owned();
    tokio::spawn(async move {
      let mut active = ActiveAction {
        inner: manager.0.clone(),
        id: id.clone(),
        complete: false,
      };
      let result = operation.await;
      {
        let mut state = manager.0.state.lock().unwrap_or_else(|error| error.into_inner());
        if result.is_err() {
          state.receipts.retain_outcome(&id, result.clone());
        } else {
          state.receipts.complete(&id, result.clone());
        }
      }
      active.complete = true;
      result
    })
    .await
    .map_err(|_| "Runtime action was interrupted; inspect status before retrying".to_owned())?
  }
  async fn start_inner(&self, request: RpcStartRuntimeRequest) -> Result<RpcRuntimeInfo, String> {
    self.ensure_open()?;
    let gate = self.gate(&request.profile_id);
    let _guard = gate.lock().await;
    self.ensure_open()?;
    let profiles = self.profiles()?;
    let id = request.profile_id.clone();
    let revision = request.expected_revision;
    let resolved = tokio::task::spawn_blocking(move || {
      let resolved = profiles.resolve_at_revision(&id, revision)?;
      resolved.validate_launch_ready()?;
      Ok::<_, String>(resolved)
    })
    .await
    .map_err(|_| "Could not read runtime profile")??;
    let runtime_id = format!("rpc:{}", request.profile_id);
    {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if state.quarantined.contains_key(&runtime_id) {
        return Err("Runtime cleanup was not confirmed; this profile remains reserved until host shutdown".into());
      }
      if let Some(record) = state.runtimes.get(&runtime_id) {
        return if record.info.status == "connected" && record.info.profile_revision == revision {
          Ok(record.info.clone())
        } else {
          Err("Close this runtime instance before applying a new configuration or reconnecting".into())
        };
      }
      if state.runtimes.len() + state.quarantined.len() >= 32 {
        return Err("Runtime limit reached; stop an existing runtime".into());
      }
    }
    let lease = if let Some(directory) = &self.0.lease_directory {
      let path = directory.join(format!("{}.lock", request.profile_id));
      Some(
        tokio::task::spawn_blocking(move || tokn_session_storage::acquire_private_lock(&path))
          .await
          .map_err(|_| "Could not acquire runtime ownership")??,
      )
    } else {
      None
    };
    // Another viewer may have deleted this profile after the first read but
    // before we acquired ownership. Recheck while deletion is fenced by lease.
    let profiles = self.profiles()?;
    let profile_id = request.profile_id.clone();
    let revision = resolved.revision;
    tokio::task::spawn_blocking(move || profiles.resolve_at_revision(&profile_id, revision).map(|_| ()))
      .await
      .map_err(|_| "Could not recheck runtime configuration")??;
    let handle = match factory::open(&runtime_id, &resolved).await {
      Ok(handle) => handle,
      Err(error) => {
        let reason = factory::redact_error(&error, &resolved);
        if resolved.profile.mode == RpcRuntimeMode::Managed {
          // Launch errors do not currently prove whether a child was spawned
          // and reaped. Reserve ownership rather than risk a second process.
          self.retain_unconfirmed_start(None, lease, &resolved);
          return Err(format!(
            "{reason} Startup cleanup was not confirmed; reserved until host shutdown."
          ));
        }
        return Err(reason);
      }
    };
    let adapter = handle.adapter();
    let mut events = adapter.subscribe();
    let native = adapter.descriptor();
    if let Err(error) = adapter
      .execute(InteractionRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        target: SessionTarget {
          runtime_id: native.runtime_id,
          instance_id: native.instance_id,
          session_id: None,
        },
        command: Command::Metadata,
      })
      .await
    {
      let reason = factory::redact_error(&error.to_string(), &resolved);
      return Err(self.retire_unregistered(handle, lease, &resolved, reason).await);
    }
    if self.0.stopping.load(Ordering::Acquire) {
      return Err(
        self
          .retire_unregistered(handle, lease, &resolved, "Runtime manager is shutting down".into())
          .await,
      );
    }
    let descriptor = match self.0.interactions.register(adapter.clone()) {
      Ok(descriptor) => descriptor,
      Err(error) => {
        let reason = factory::redact_error(&error.to_string(), &resolved);
        return Err(self.retire_unregistered(handle, lease, &resolved, reason).await);
      }
    };
    let info = runtime_info(descriptor, &resolved, "connected", None);
    let weak = Arc::downgrade(&self.0);
    let instance = info.instance_id.clone();
    let runtime = info.runtime_id.clone();
    let (ready, start) = tokio::sync::oneshot::channel::<()>();
    let pump = tokio::spawn(async move {
      if start.await.is_err() {
        return;
      }
      loop {
        let event = events.recv().await;
        let Some(inner) = weak.upgrade() else {
          return;
        };
        let mut state = inner.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(record) = state
          .runtimes
          .get_mut(&runtime)
          .filter(|record| record.info.instance_id == instance)
        else {
          return;
        };
        match event {
          Ok(RuntimeEvent {
            kind: RuntimeEventKind::Native(_),
            ..
          }) => continue,
          Ok(RuntimeEvent {
            kind: RuntimeEventKind::Disconnected { .. },
            ..
          }) => {
            record.info.status = "disconnected".into();
            record.info.last_error = Some("RPC connection ended; close this instance before reconnecting".into());
            state.bindings.retain(|_, binding| binding.target.runtime_id != runtime);
          }
          Err(broadcast::error::RecvError::Lagged(_)) => {
            record.info.status = "recovery_required".into();
            state.bindings.retain(|_, binding| binding.target.runtime_id != runtime);
          }
          Err(_) => return,
          _ => {}
        }
        drop(state);
        let _ = inner.changes.send(());
      }
    });
    self
      .0
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .runtimes
      .insert(
        runtime_id,
        Record {
          info: info.clone(),
          handle,
          _lease: lease,
          pump,
        },
      );
    let _ = ready.send(());
    self.changed();
    Ok(info)
  }
  async fn retire_unregistered(
    &self,
    handle: factory::Handle,
    lease: Option<File>,
    profile: &RpcResolvedProfile,
    reason: String,
  ) -> String {
    if handle.adapter().close().await.is_ok() {
      return reason;
    }
    self.retain_unconfirmed_start(Some(handle), lease, profile);
    format!("{reason} Cleanup was not confirmed; this profile remains reserved until host shutdown.")
  }
  fn retain_unconfirmed_start(
    &self,
    handle: Option<factory::Handle>,
    lease: Option<File>,
    profile: &RpcResolvedProfile,
  ) {
    // Some adapters consume their child while attempting cleanup. A second close
    // can succeed without proving that first child exited, so never retry-unlock.
    let info = runtime_info(
      handle
        .as_ref()
        .map(|handle| handle.adapter().descriptor())
        .unwrap_or_else(|| RuntimeDescriptor {
          runtime_id: format!("rpc:{}", profile.profile.profile_id),
          instance_id: uuid::Uuid::new_v4().to_string(),
          provider: profile.profile.provider,
          ownership: Ownership::Managed,
          capabilities: Capabilities::default(),
        }),
      profile,
      "cleanup_failed",
      Some("Startup failed and cleanup was not confirmed; reserved until host shutdown".into()),
    );
    self
      .0
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .quarantined
      .insert(
        info.runtime_id.clone(),
        QuarantinedRecord {
          info,
          _handle: handle,
          _lease: lease,
        },
      );
    self.changed();
  }
  async fn close_inner(&self, request: RpcCloseRuntimeRequest) -> Result<RpcCloseRuntimeResponse, String> {
    self.ensure_open()?;
    if self
      .0
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .quarantined
      .contains_key(&request.runtime_id)
    {
      return Err("Runtime cleanup was not confirmed; this instance remains reserved until host shutdown".into());
    }
    let profile = self
      .0
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .runtimes
      .get(&request.runtime_id)
      .map(|record| record.info.profile_id.clone())
      .ok_or("Unknown RPC runtime")?;
    let gate = self.gate(&profile);
    let _guard = gate.lock().await;
    {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      let record = state
        .runtimes
        .get_mut(&request.runtime_id)
        .filter(|record| record.info.instance_id == request.expected_instance_id)
        .ok_or("Runtime instance changed; refresh its status")?;
      record.info.status = "closing".into();
    }
    self.changed();
    let result = self
      .0
      .interactions
      .detach(&request.runtime_id, &request.expected_instance_id)
      .await;
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    if let Err(_error) = result {
      if let Some(record) = state.runtimes.get_mut(&request.runtime_id) {
        record.info.last_error =
          Some("Cleanup was not confirmed; this instance remains reserved until host shutdown".into());
      }
      drop(state);
      self.changed();
      return Err("Runtime cleanup was not confirmed; inspect the host before retrying".into());
    }
    state.runtimes.remove(&request.runtime_id);
    state
      .binding_owners
      .retain(|_, (owner, _)| owner != &request.runtime_id);
    state
      .bindings
      .retain(|_, binding| binding.target.runtime_id != request.runtime_id);
    drop(state);
    self.changed();
    Ok(RpcCloseRuntimeResponse {
      runtime_id: request.runtime_id,
      instance_id: request.expected_instance_id,
      closed: true,
    })
  }
  pub(crate) fn binding(&self, key: &str) -> Option<RpcSessionBinding> {
    self
      .0
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .bindings
      .get(key)
      .cloned()
  }
  /// Host-verified callers only. A binding never comes from a client-supplied path.
  pub(crate) async fn bind(
    &self,
    request_id: String,
    key: String,
    profile_id: String,
    instance: String,
    native_id: String,
    provider: Provider,
  ) -> Result<RpcSessionBinding, String> {
    validate_uuid(&request_id)?;
    validate_uuid(&profile_id)?;
    let fingerprint: [u8; 32] = Sha256::digest(
      serde_json::to_vec(&(&key, &profile_id, &instance, &native_id, provider))
        .map_err(|_| "Invalid binding target")?,
    )
    .into();
    let manager = self.clone();
    match self
      .action(&request_id, "bind", fingerprint, async move {
        manager
          .bind_inner(key, profile_id, instance, native_id, provider)
          .await
          .map(ActionReply::Bound)
      })
      .await?
    {
      ActionReply::Bound(binding) => Ok(binding),
      _ => Err("Request ID belongs to another runtime action".into()),
    }
  }
  async fn bind_inner(
    &self,
    key: String,
    profile_id: String,
    instance: String,
    native_id: String,
    provider: Provider,
  ) -> Result<RpcSessionBinding, String> {
    self.ensure_open()?;
    let owner_key = format!("{provider:?}:{native_id}");
    let session_gate = self.gate(&format!("session:{owner_key}"));
    let _session_guard = session_gate.lock().await;
    let gate = self.gate(&profile_id);
    let _guard = gate.lock().await;
    self.ensure_open()?;
    let (info, handle) = {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      let record = state
        .runtimes
        .values()
        .find(|record| {
          record.info.profile_id == profile_id
            && record.info.instance_id == instance
            && record.info.provider == provider
            && record.info.status == "connected"
        })
        .ok_or("No matching connected runtime instance")?;
      (record.info.clone(), record.handle.clone())
    };
    let target = SessionTarget {
      runtime_id: info.runtime_id.clone(),
      instance_id: instance,
      session_id: Some(native_id.clone()),
    };
    if let Some(binding) = self.binding(&key) {
      if binding.target == target {
        return Ok(binding);
      }
    }
    {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if let Some((runtime, instance)) = state.binding_owners.get(&owner_key) {
        if runtime != &target.runtime_id || instance != &target.instance_id {
          return Err("This session already has another runtime owner; detach it first".into());
        }
      }
      if state.binding_owners.len() >= 1024 && !state.binding_owners.contains_key(&owner_key) {
        return Err("Runtime session-owner limit reached".into());
      }
      if state.bindings.len() >= 1024 && !state.bindings.contains_key(&key) {
        return Err("Runtime session-binding limit reached".into());
      }
      // A failed/uncertain resume still reserves ownership until this exact
      // runtime closes; a second runtime must not silently take over its work.
      state
        .binding_owners
        .insert(owner_key, (target.runtime_id.clone(), target.instance_id.clone()));
    }
    handle.bind_native(&native_id).await?;
    for (id, command) in [("state", Command::State), ("pending", Command::PendingInteractions)] {
      self
        .0
        .interactions
        .execute(InteractionRequest {
          request_id: format!("bind-{id}-{}", uuid::Uuid::new_v4()),
          target: target.clone(),
          command,
        })
        .await
        .map_err(|_| "Could not recover live session state")?;
    }
    let binding = RpcSessionBinding {
      session_key: key.clone(),
      profile_id,
      target,
      capabilities: info.capabilities,
    };
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    if state.bindings.len() >= 1024 && !state.bindings.contains_key(&key) {
      return Err("Runtime session-binding limit reached".into());
    }
    self.ensure_open()?;
    if !state
      .runtimes
      .get(&binding.target.runtime_id)
      .is_some_and(|record| record.info.instance_id == binding.target.instance_id && record.info.status == "connected")
    {
      return Err("Runtime disconnected while recovering the session binding".into());
    }
    state.bindings.insert(key, binding.clone());
    drop(state);
    self.changed();
    Ok(binding)
  }
  pub(crate) async fn shutdown(&self) {
    self.0.stopping.store(true, Ordering::Release);
    loop {
      let settled = self.0.settled.notified();
      tokio::pin!(settled);
      settled.as_mut().enable();
      if self
        .0
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .active_actions
        == 0
      {
        break;
      }
      settled.await;
    }
    let _ = self.0.interactions.shutdown().await;
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    state.runtimes.clear();
    state.quarantined.clear();
    state.bindings.clear();
    state.binding_owners.clear();
  }
}
fn runtime_info(
  descriptor: RuntimeDescriptor,
  profile: &RpcResolvedProfile,
  status: &str,
  last_error: Option<String>,
) -> RpcRuntimeInfo {
  RpcRuntimeInfo {
    runtime_id: descriptor.runtime_id,
    instance_id: descriptor.instance_id,
    profile_id: profile.profile.profile_id.clone(),
    profile_revision: profile.revision,
    name: profile.profile.name.clone(),
    provider: descriptor.provider,
    ownership: descriptor.ownership,
    status: status.into(),
    capabilities: descriptor.capabilities,
    session_count: 0,
    last_error,
  }
}
fn validate_uuid(id: &str) -> Result<(), String> {
  uuid::Uuid::parse_str(id)
    .map(|_| ())
    .map_err(|_| "Invalid runtime request or profile ID".into())
}
