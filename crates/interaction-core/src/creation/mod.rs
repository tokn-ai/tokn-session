//! Durable admission for explicit session creation. Runtime processes and native
//! wire operations remain owned by the runtime coordinator and host adapters.
mod persistence;
mod routes;
pub use routes::CreationRouteLedger;
#[cfg(test)]
mod tests;

use crate::runtime::{CreatedNativeSession, RuntimeSelection};
use crate::{AdapterFuture, ErrorKind, InteractionError, MessageInput, ModelIdentity, Provider};
use persistence::{RequestLease, Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
  collections::{BTreeMap, BTreeSet},
  path::PathBuf,
  sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, oneshot};

/// Live payload only: never serialized into the ownership catalog.
#[derive(Clone, Debug)]
pub struct CreationRequest {
  pub request_id: String,
  pub provider: Provider,
  pub project_id: String,
  pub working_directory: PathBuf,
  pub input: MessageInput,
  pub image_ids: Vec<String>,
  pub runtime: Option<RuntimeSelection>,
  /// Creation-time intent, separate from generic model-on-message support.
  pub model: Option<ModelIdentity>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CreationPhase {
  Admitted,
  NativeCreateUncertain,
  NativeKnown,
  FirstInputUncertain,
  HistoryPending,
  Ready,
  Failed,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FirstInputState {
  #[default]
  NotAttempted,
  Uncertain,
  Accepted,
  Rejected,
}

/// A receipt contains identities and progress, never first-input content,
/// credentials, upload bytes, or native environment/configuration values.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreationSnapshot {
  pub request_id: String,
  pub provider: Provider,
  pub project_id: String,
  pub working_directory: PathBuf,
  pub runtime: Option<RuntimeSelection>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<ModelIdentity>,
  pub fingerprint: String,
  pub revision: u64,
  pub phase: CreationPhase,
  pub planned_native_id: Option<String>,
  pub created: Option<CreatedNativeSession>,
  pub first_input: FirstInputState,
  pub session_key: Option<String>,
  pub message: Option<String>,
  #[serde(default)]
  pub cancel_requested: bool,
}

/// Implementations supply host policy and exact-source verification. Only
/// `create_native` and `send_initial` may activate or mutate a native session.
/// All methods must use bounded native operations; shutdown drains admitted work.
pub trait CreationHost: Send + Sync + 'static {
  fn plan_native_id(&self, provider: Provider, request_id: &str) -> Result<Option<String>, String>;
  fn create_native<'a>(
    &'a self,
    request: &'a CreationRequest,
    planned_native_id: Option<&'a str>,
  ) -> AdapterFuture<'a, Result<CreatedNativeSession, InteractionError>>;
  fn send_initial<'a>(
    &'a self,
    created: &'a CreatedNativeSession,
    request_id: &'a str,
    input: &'a MessageInput,
    model: Option<&'a ModelIdentity>,
  ) -> AdapterFuture<'a, Result<(), InteractionError>>;
  /// Cancellation is independent of a message awaiting native acknowledgement.
  /// It cancels the native turn and never closes a shared runtime.
  fn cancel_initial<'a>(
    &'a self,
    _created: &'a CreatedNativeSession,
  ) -> AdapterFuture<'a, Result<(), InteractionError>> {
    Box::pin(async {
      Err(InteractionError::unsupported(
        "This runtime cannot cancel the created turn",
      ))
    })
  }
  /// Read/verify/index exactly the returned native identity; never create,
  /// resume, or replay input. None means durable history is not ready yet.
  fn publish_history<'a>(&'a self, receipt: &'a CreationSnapshot) -> AdapterFuture<'a, Result<Option<String>, String>>;
}

#[derive(Clone)]
pub struct CreationManager(Arc<Inner>);

struct Inner {
  host: Arc<dyn CreationHost>,
  store: Store,
  state: Mutex<State>,
  admission: AsyncMutex<()>,
  progress: AsyncMutex<()>,
  events: broadcast::Sender<CreationSnapshot>,
  settled: Notify,
}
struct State {
  receipts: BTreeMap<String, CreationSnapshot>,
  gates: BTreeMap<String, Arc<AsyncMutex<()>>>,
  running: BTreeSet<String>,
  stopping: bool,
  active: usize,
}
struct Active(Arc<Inner>);
struct Running {
  inner: Arc<Inner>,
  id: String,
}
impl Drop for Running {
  fn drop(&mut self) {
    self
      .inner
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .running
      .remove(&self.id);
  }
}
impl Drop for Active {
  fn drop(&mut self) {
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    state.active = state.active.saturating_sub(1);
    drop(state);
    self.0.settled.notify_waiters();
  }
}

impl CreationManager {
  /// Blocking host setup. Restart loads receipts for observation only; even an
  /// admitted request whose first input is absent is never automatically replayed.
  pub fn new(state_path: Option<PathBuf>, host: Arc<dyn CreationHost>) -> Result<Self, String> {
    let (store, receipts) = Store::new(state_path)?;
    Ok(Self(Arc::new(Inner {
      host,
      store,
      state: Mutex::new(State {
        receipts: receipts
          .into_iter()
          .map(|receipt| (receipt.request_id.clone(), receipt))
          .collect(),
        gates: BTreeMap::new(),
        running: BTreeSet::new(),
        stopping: false,
        active: 0,
      }),
      admission: AsyncMutex::new(()),
      progress: AsyncMutex::new(()),
      events: broadcast::channel(128).0,
      settled: Notify::new(),
    })))
  }

  pub fn subscribe(&self) -> broadcast::Receiver<CreationSnapshot> {
    self.0.events.subscribe()
  }

  pub fn get(&self, request_id: &str) -> Result<Option<CreationSnapshot>, String> {
    validate_request_id(request_id)?;
    Ok(
      self
        .0
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .receipts
        .get(request_id)
        .cloned(),
    )
  }

  pub fn list(&self) -> Vec<CreationSnapshot> {
    self
      .0
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .receipts
      .values()
      .cloned()
      .collect()
  }

  /// Checks immutable intent before reading mutable project/configuration state.
  pub fn receipt(&self, request: &CreationRequest) -> Result<Option<CreationSnapshot>, InteractionError> {
    // Accepted staging IDs remain observable after upload bytes are consumed.
    // The host need not resolve mutable project state or image bytes on a retry.
    validate_intent(request)?;
    let Some(receipt) = self.get(&request.request_id).map_err(InteractionError::invalid)? else {
      return Ok(None);
    };
    check_fingerprint(&receipt, &fingerprint(request)?)?;
    Ok(Some(receipt))
  }

  pub async fn create(&self, request: CreationRequest) -> Result<CreationSnapshot, InteractionError> {
    validate_request(&request)?;
    let active = self.begin()?;
    let manager = self.clone();
    let (reply, receive) = oneshot::channel();
    tokio::spawn(async move {
      let _active = active;
      match manager.admit(&request).await {
        Ok((receipt, Some(lease))) => {
          let _running = Running {
            inner: manager.0.clone(),
            id: request.request_id.clone(),
          };
          let _ = reply.send(Ok(receipt.clone()));
          let gate = manager.gate(&request.request_id);
          let _guard = gate.lock().await;
          let _lease = lease;
          if let Err(error) = manager.run(request, receipt).await {
            // A persistence failure cannot authorize another attempt. Keep the
            // last conservative phase in memory and publish a visible failure.
            manager.report_failure(error.0, error.1);
          }
        }
        Ok((receipt, None)) => {
          let _ = reply.send(Ok(receipt));
        }
        Err(error) => {
          let _ = reply.send(Err(error));
        }
      }
    });
    receive
      .await
      .map_err(|_| InteractionError::uncertain("Creation admission was interrupted; check its receipt"))?
  }

  fn begin(&self) -> Result<Active, InteractionError> {
    let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
    if state.stopping {
      return Err(InteractionError::unavailable("Session creation is shutting down"));
    }
    state.active += 1;
    Ok(Active(self.0.clone()))
  }

  fn gate(&self, id: &str) -> Arc<AsyncMutex<()>> {
    self
      .0
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .gates
      .entry(id.to_owned())
      .or_default()
      .clone()
  }

  async fn admit(
    &self,
    request: &CreationRequest,
  ) -> Result<(CreationSnapshot, Option<RequestLease>), InteractionError> {
    let _guard = self.0.admission.lock().await;
    let fingerprint_value = fingerprint(request)?;
    if let Some(receipt) = self.get(&request.request_id).map_err(InteractionError::invalid)? {
      check_fingerprint(&receipt, &fingerprint_value)?;
      return Ok((receipt, None));
    }
    let planned_native_id = self
      .0
      .host
      .plan_native_id(request.provider, &request.request_id)
      .map_err(InteractionError::invalid)?;
    let receipt = CreationSnapshot {
      request_id: request.request_id.clone(),
      provider: request.provider,
      project_id: request.project_id.clone(),
      working_directory: request.working_directory.clone(),
      runtime: request.runtime.clone(),
      model: request.model.as_ref().map(crate::runtime::creation_model_identity),
      fingerprint: fingerprint_value,
      revision: 1,
      phase: CreationPhase::Admitted,
      planned_native_id,
      created: None,
      first_input: FirstInputState::NotAttempted,
      session_key: None,
      message: None,
      cancel_requested: false,
    };
    let store = self.0.store.clone();
    let proposed = receipt.clone();
    let (receipt, lease) = tokio::task::spawn_blocking(move || store.admit(proposed))
      .await
      .map_err(|_| InteractionError::uncertain("Could not confirm creation admission"))?
      .map_err(InteractionError::unavailable)?;
    check_fingerprint(&receipt, &fingerprint(request)?)?;
    {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      state.receipts.insert(receipt.request_id.clone(), receipt.clone());
      if lease.is_some() {
        state.running.insert(receipt.request_id.clone());
      }
    }
    let _ = self.0.events.send(receipt.clone());
    Ok((receipt, lease))
  }

  async fn run(&self, request: CreationRequest, mut receipt: CreationSnapshot) -> Result<(), (String, String)> {
    let id = receipt.request_id.clone();
    macro_rules! save {
      () => {
        self
          .save(&mut receipt)
          .await
          .map_err(|error| (id.clone(), error))?
      };
    }
    if self.cancelled(&id) {
      receipt.phase = CreationPhase::Failed;
      receipt.message = Some("Session creation was cancelled before its first input".into());
      save!();
      return Ok(());
    }
    receipt.phase = CreationPhase::NativeCreateUncertain;
    save!();
    match self
      .0
      .host
      .create_native(&request, receipt.planned_native_id.as_deref())
      .await
    {
      Ok(created) => {
        if created.provider != receipt.provider
          || created.native.working_directory != receipt.working_directory
          || receipt
            .runtime
            .as_ref()
            .is_some_and(|selection| selection.profile_id != created.profile_id)
          || receipt
            .planned_native_id
            .as_ref()
            .is_some_and(|id| id != &created.native.native_id)
        {
          receipt.message = Some("Native creation returned a different identity or working directory".into());
          save!();
          return Ok(());
        }
        receipt.created = Some(created);
        receipt.phase = CreationPhase::NativeKnown;
        save!();
      }
      Err(error) => {
        receipt.message = Some(error.message);
        if error.kind != ErrorKind::Uncertain {
          receipt.phase = CreationPhase::Failed;
        }
        save!();
        return Ok(());
      }
    }
    receipt.phase = CreationPhase::FirstInputUncertain;
    receipt.first_input = FirstInputState::Uncertain;
    if self.cancelled(&id) {
      receipt.phase = CreationPhase::Failed;
      receipt.first_input = FirstInputState::NotAttempted;
      receipt.message = Some("Session creation was cancelled before its first input".into());
      save!();
      self.publish(&mut receipt).await.map_err(|error| (id, error))?;
      return Ok(());
    }
    save!();
    if self.cancelled(&id) {
      receipt.phase = CreationPhase::Failed;
      receipt.first_input = FirstInputState::NotAttempted;
      receipt.message = Some("Session creation was cancelled before its first input".into());
      save!();
      self.publish(&mut receipt).await.map_err(|error| (id, error))?;
      return Ok(());
    }
    let created = receipt.created.as_ref().unwrap().clone();
    let send = self
      .0
      .host
      .send_initial(&created, &id, &request.input, request.model.as_ref());
    tokio::pin!(send);
    let poll_period = std::time::Duration::from_millis(500);
    let mut history_poll = tokio::time::interval_at(tokio::time::Instant::now() + poll_period, poll_period);
    history_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut publication_error = None;
    let result = loop {
      tokio::select! {
        result = &mut send => break result,
        _ = history_poll.tick(), if receipt.session_key.is_none() => {
          // Native preflight may await a question whose answer requires opening
          // the new conversation. Publish exact history while the one initial
          // admission is pending, without claiming that delivery completed.
          if let Err(error) = self.publish(&mut receipt).await {
            if publication_error.as_ref() != Some(&error) {
              self.report_failure(id.clone(), error.clone());
              receipt.message = Some(error.clone());
              publication_error = Some(error);
            }
          }
        }
      }
    };
    match result {
      Ok(()) => {
        receipt.first_input = FirstInputState::Accepted;
        receipt.phase = if receipt.session_key.is_some() {
          CreationPhase::Ready
        } else {
          CreationPhase::HistoryPending
        };
      }
      Err(error) => {
        receipt.message = Some(error.message);
        if error.kind != ErrorKind::Uncertain {
          receipt.first_input = FirstInputState::Rejected;
          receipt.phase = if receipt.session_key.is_some() {
            CreationPhase::Ready
          } else {
            CreationPhase::HistoryPending
          };
        }
      }
    }
    save!();
    self.publish(&mut receipt).await.map_err(|error| (id.clone(), error))
  }

  async fn save(&self, receipt: &mut CreationSnapshot) -> Result<(), String> {
    let _guard = self.0.progress.lock().await;
    if let Some(current) = self.get(&receipt.request_id)? {
      receipt.cancel_requested |= current.cancel_requested;
      receipt.revision = receipt.revision.max(current.revision);
    }
    receipt.revision = receipt.revision.saturating_add(1);
    self.persist(receipt).await
  }

  async fn persist(&self, receipt: &CreationSnapshot) -> Result<(), String> {
    let store = self.0.store.clone();
    let saved = receipt.clone();
    let result = tokio::task::spawn_blocking(move || store.save(saved))
      .await
      .map_err(|_| "Could not save creation progress".to_owned())
      .and_then(|result| result);
    let published = {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      let mut published = receipt.clone();
      // Cancellation is an immediate in-memory fence and can arrive while an
      // earlier progress write is awaiting disk. Never erase that fence.
      published.cancel_requested |= state
        .receipts
        .get(&receipt.request_id)
        .is_some_and(|current| current.cancel_requested);
      state.receipts.insert(receipt.request_id.clone(), published.clone());
      published
    };
    let _ = self.0.events.send(published);
    result
  }

  fn report_failure(&self, id: String, message: String) {
    let snapshot = {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      let Some(receipt) = state.receipts.get_mut(&id) else {
        return;
      };
      receipt.message = Some(message);
      receipt.revision = receipt.revision.saturating_add(1);
      receipt.clone()
    };
    let _ = self.0.events.send(snapshot);
  }

  async fn publish(&self, receipt: &mut CreationSnapshot) -> Result<(), String> {
    if receipt.session_key.is_some() {
      if receipt.phase == CreationPhase::HistoryPending {
        receipt.phase = CreationPhase::Ready;
        return self.save(receipt).await;
      }
      return Ok(());
    }
    if receipt.created.is_none() && receipt.planned_native_id.is_none() {
      return Ok(());
    }
    match self.0.host.publish_history(receipt).await {
      Ok(Some(key)) => {
        if key.is_empty() || key.len() > 8192 {
          return Err("Invalid published history key".into());
        }
        receipt.session_key = Some(key);
        if receipt.phase == CreationPhase::HistoryPending {
          receipt.phase = CreationPhase::Ready;
        }
        self.save(receipt).await
      }
      Ok(None) => Ok(()),
      Err(error) => {
        if receipt.message.as_ref() == Some(&error) {
          return Ok(());
        }
        receipt.message = Some(error);
        self.save(receipt).await
      }
    }
  }

  /// Read-only native reconciliation. It can publish verified history but never
  /// invokes creation, resume, or initial-input delivery, including after restart.
  pub async fn refresh(&self, request_id: &str) -> Result<Option<CreationSnapshot>, String> {
    validate_request_id(request_id)?;
    let _active = self.begin().map_err(|error| error.to_string())?;
    // The admitted workflow already polls exact history while its first native
    // input awaits a reply. Reads must not wait on that same workflow gate: the
    // caller may need its published key to answer the question blocking input.
    let running = {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      state
        .running
        .contains(request_id)
        .then(|| state.receipts.get(request_id).cloned())
        .flatten()
    };
    if let Some(receipt) = running {
      return Ok(Some(receipt));
    }
    if self.get(request_id)?.is_none() {
      return Ok(None);
    }
    let gate = self.gate(request_id);
    let _guard = gate.lock().await;
    let Some(mut receipt) = self.get(request_id)? else {
      return Ok(None);
    };
    let store = self.0.store.clone();
    let id = request_id.to_owned();
    let _lease = match tokio::task::spawn_blocking(move || store.lease(&id))
      .await
      .map_err(|_| "Could not inspect creation ownership")?
    {
      Ok(lease) => lease,
      // Another process may still be creating this session. Reading its durable
      // receipt is sufficient; do not issue concurrent native reconciliation.
      Err(_) => return Ok(Some(receipt)),
    };
    let store = self.0.store.clone();
    let id = request_id.to_owned();
    if let Some(latest) = tokio::task::spawn_blocking(move || store.latest(&id))
      .await
      .map_err(|_| "Could not read creation progress")??
    {
      receipt = latest;
      self
        .0
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .receipts
        .insert(receipt.request_id.clone(), receipt.clone());
    }
    self.publish(&mut receipt).await?;
    Ok(Some(receipt))
  }

  fn cancelled(&self, request_id: &str) -> bool {
    self
      .0
      .state
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .receipts
      .get(request_id)
      .is_some_and(|receipt| receipt.cancel_requested)
  }

  /// Fence initial delivery immediately, independently of a blocked native
  /// request. Completion drains the admitted workflow before acknowledging Stop.
  pub async fn cancel(&self, request_id: &str) -> Result<Option<CreationSnapshot>, String> {
    validate_request_id(request_id)?;
    let active = self.begin().map_err(|error| error.to_string())?;
    let manager = self.clone();
    let id = request_id.to_owned();
    tokio::spawn(async move {
      let _active = active;
      manager.cancel_inner(&id).await
    })
    .await
    .map_err(|_| "Creation cancellation was interrupted; inspect its status".to_owned())?
  }

  async fn cancel_inner(&self, request_id: &str) -> Result<Option<CreationSnapshot>, String> {
    let locally_running = {
      let state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      if !state.receipts.contains_key(request_id) {
        return Ok(None);
      }
      state.running.contains(request_id)
    };
    let _lease = if locally_running {
      None
    } else {
      let store = self.0.store.clone();
      let id = request_id.to_owned();
      let (lease, latest) = tokio::task::spawn_blocking(move || {
        let lease = store
          .lease(&id)
          .map_err(|_| "Another host owns this creation; cancel it on its owning connection")?;
        Ok::<_, String>((lease, store.latest(&id)?))
      })
      .await
      .map_err(|_| "Could not inspect creation ownership")??;
      if let Some(latest) = latest {
        self
          .0
          .state
          .lock()
          .unwrap_or_else(|error| error.into_inner())
          .receipts
          .insert(latest.request_id.clone(), latest);
      }
      Some(lease)
    };
    {
      let mut state = self.0.state.lock().unwrap_or_else(|error| error.into_inner());
      let Some(receipt) = state.receipts.get_mut(request_id) else {
        return Ok(None);
      };
      receipt.cancel_requested = true;
    }
    let receipt = {
      let _guard = self.0.progress.lock().await;
      let mut receipt = self.get(request_id)?.ok_or("Creation receipt is unavailable")?;
      receipt.cancel_requested = true;
      receipt.revision = receipt.revision.saturating_add(1);
      self.persist(&receipt).await?;
      receipt
    };
    // An early cancel can unblock a native send waiting on an approval. Its
    // failure does not remove the fence or skip draining the workflow.
    if let Some(created) = &receipt.created {
      if locally_running && receipt.first_input != FirstInputState::NotAttempted {
        let _ = self.0.host.cancel_initial(created).await;
      }
    }
    let gate = self.gate(request_id);
    let _guard = gate.lock().await;
    let receipt = self.get(request_id)?.ok_or("Creation receipt is unavailable")?;
    if let Some(created) = &receipt.created {
      if receipt.first_input != FirstInputState::NotAttempted {
        self
          .0
          .host
          .cancel_initial(created)
          .await
          .map_err(|error| error.to_string())?;
      }
    }
    Ok(Some(receipt))
  }

  pub async fn shutdown(&self) {
    self.0.state.lock().unwrap_or_else(|error| error.into_inner()).stopping = true;
    loop {
      let settled = self.0.settled.notified();
      tokio::pin!(settled);
      settled.as_mut().enable();
      if self.0.state.lock().unwrap_or_else(|error| error.into_inner()).active == 0 {
        return;
      }
      settled.await;
    }
  }
}

fn validate_request_id(id: &str) -> Result<(), String> {
  let parsed = uuid::Uuid::parse_str(id).map_err(|_| "Invalid creation request ID".to_owned())?;
  if parsed.to_string() != id {
    return Err("Creation request ID must be a canonical UUID".into());
  }
  Ok(())
}
fn validate_profile_id(id: &str) -> Result<(), String> {
  uuid::Uuid::parse_str(id)
    .map(|_| ())
    .map_err(|_| "Invalid runtime profile ID".into())
}
fn validate_request(request: &CreationRequest) -> Result<(), InteractionError> {
  validate_intent(request)?;
  if request.input.images.len() != request.image_ids.len() {
    return Err(InteractionError::invalid(
      "Image attachments did not match the creation request",
    ));
  }
  Ok(())
}
fn validate_intent(request: &CreationRequest) -> Result<(), InteractionError> {
  validate_request_id(&request.request_id).map_err(InteractionError::invalid)?;
  crate::managed_sessions::validate_image_ids(&request.image_ids).map_err(InteractionError::invalid)?;
  if request.project_id.is_empty()
    || request.project_id.len() > 128
    || request.project_id.chars().any(char::is_control)
    || !request.working_directory.is_absolute()
    || request.input.model.is_some()
    || request.input.expected_revision.is_some()
  {
    return Err(InteractionError::invalid("Invalid creation intent"));
  }
  if let Some(selection) = &request.runtime {
    validate_profile_id(&selection.profile_id).map_err(InteractionError::invalid)?;
  }
  if let Some(model) = &request.model {
    crate::runtime::validate_creation_model(model)?;
  }
  crate::managed_sessions::validate_message(&request.input.text, !request.image_ids.is_empty())
    .map_err(InteractionError::invalid)
}
fn fingerprint(request: &CreationRequest) -> Result<String, InteractionError> {
  let legacy = (
    request.provider,
    &request.project_id,
    &request.working_directory,
    &request.input.text,
    &request.image_ids,
    &request.runtime,
  );
  // Preserve pre-model receipts byte-for-byte when native defaults are chosen.
  let payload = match &request.model {
    None => serde_json::to_vec(&legacy),
    Some(model) => serde_json::to_vec(&(legacy, crate::runtime::creation_model_identity(model))),
  }
  .map_err(|_| InteractionError::invalid("Invalid creation intent"))?;
  Ok(format!("{:x}", Sha256::digest(payload)))
}
fn check_fingerprint(receipt: &CreationSnapshot, fingerprint: &str) -> Result<(), InteractionError> {
  if receipt.fingerprint == fingerprint {
    Ok(())
  } else {
    Err(InteractionError::new(
      ErrorKind::Conflict,
      "Creation request ID was already used with different content",
    ))
  }
}
