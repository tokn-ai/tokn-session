//! Durable, provider-neutral orchestration for host-owned agent sessions.
//! Provider drivers own native commands, state parsing and history validation;
//! this module owns admission, receipts, process leases and worker lifetimes.
mod contracts;
mod host;
mod message_receipts;
mod model_contracts;
mod models;
mod persistence;
mod runtimes;
mod worker_lifecycle;

use crate::session_ownership::{SessionLease as NativeSessionLease, SessionOwnership};
pub use contracts::*;
pub use host::ManagedHostContext;
pub use message_receipts::{ManagedMessageReceipts, managed_message_fingerprint};
pub use model_contracts::*;
use persistence::ReceiptStore;
pub use persistence::{Receipt, SessionLease};
use sha2::{Digest, Sha256};
use std::{
  collections::HashMap,
  path::PathBuf,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
pub use worker_lifecycle::ManagedWorkerLifecycle;

const MAX_ACTIVE: usize = 8;
const MAX_RECEIPTS: usize = 256;
const COMMAND_CAPACITY: usize = 16;
const QUEUE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct ManagedSessions {
  inner: Arc<Inner>,
}

struct Inner {
  records: Mutex<HashMap<String, Record>>,
  store: ReceiptStore,
  events: broadcast::Sender<ManagedSessionSnapshot>,
  driver: Arc<dyn ManagedSessionDriver>,
  host: Arc<dyn ManagedSessionHost>,
  unavailable: Option<String>,
  shutting_down: AtomicBool,
  shutdown: watch::Sender<bool>,
  idle_timeout: std::time::Duration,
  ownership: Option<SessionOwnership>,
  provider: crate::Provider,
}

struct Record {
  receipt: Receipt,
  generation: u64,
  sender: Option<mpsc::Sender<Command>>,
  start_gate: Arc<AsyncMutex<()>>,
  worker_state: Option<watch::Receiver<WorkerState>>,
  instance_id: String,
  worker_pid: Option<u32>,
  started_at_ms: Option<i64>,
  termination: Option<CancellationToken>,
  restarting: Option<watch::Receiver<bool>>,
  images_available: bool,
  native_lease: Option<NativeSessionLease>,
}

impl Record {
  fn new(receipt: Receipt) -> Self {
    Self {
      receipt,
      generation: 0,
      sender: None,
      start_gate: Arc::new(AsyncMutex::new(())),
      worker_state: None,
      instance_id: uuid::Uuid::new_v4().to_string(),
      worker_pid: None,
      started_at_ms: None,
      termination: None,
      restarting: None,
      images_available: false,
      native_lease: None,
    }
  }
}

pub struct OwnedInputStatus {
  pub message: String,
  pub resume_required: bool,
  pub images_available: bool,
}

impl ManagedSessions {
  /// Read durable state during blocking host setup. Async mutations use
  /// blocking tasks; ordinary lookups never access disk.
  pub fn new(
    configuration: ManagedSessionConfiguration,
    driver: Arc<dyn ManagedSessionDriver>,
    host: Arc<dyn ManagedSessionHost>,
    ownership: SessionOwnership,
  ) -> Result<Self, String> {
    let provider = driver.provider();
    let (store, receipts) = ReceiptStore::new(configuration.clone(), driver.clone(), host.clone())?;
    let records = receipts
      .into_iter()
      .map(|receipt| (receipt.request_id.clone(), Record::new(receipt)))
      .collect();
    let (events, _) = broadcast::channel(128);
    Ok(Self {
      inner: Arc::new(Inner {
        records: Mutex::new(records),
        store,
        events,
        driver,
        host,
        unavailable: None,
        shutting_down: AtomicBool::new(false),
        shutdown: watch::channel(false).0,
        idle_timeout: configuration.idle_timeout,
        ownership: Some(ownership),
        provider,
      }),
    })
  }

  pub fn disabled(message: String, provider: crate::Provider) -> Self {
    let (events, _) = broadcast::channel(128);
    let driver: Arc<dyn ManagedSessionDriver> = Arc::new(DisabledDriver(provider));
    let host: Arc<dyn ManagedSessionHost> = Arc::new(DisabledHost);
    Self {
      inner: Arc::new(Inner {
        records: Mutex::new(HashMap::new()),
        store: ReceiptStore::disabled(provider),
        events,
        driver,
        host,
        unavailable: Some(message),
        shutting_down: AtomicBool::new(false),
        shutdown: watch::channel(false).0,
        idle_timeout: IDLE_TIMEOUT,
        ownership: None,
        provider,
      }),
    }
  }

  pub fn available(&self) -> Result<(), String> {
    if self.inner.shutting_down.load(Ordering::Acquire) {
      return Err("Managed session runtime is shutting down".into());
    }
    self.inner.unavailable.as_ref().map_or(Ok(()), |message| {
      Err(format!("Managed sessions are unavailable: {message}"))
    })
  }

  pub async fn create(
    &self,
    request: CreateProjectSessionRequest,
    project: RegisteredProject,
  ) -> Result<ManagedSessionSnapshot, String> {
    self.create_with_images(request, project, Vec::new()).await
  }

  pub async fn create_with_images(
    &self,
    request: CreateProjectSessionRequest,
    project: RegisteredProject,
    images: Vec<ResolvedImage>,
  ) -> Result<ManagedSessionSnapshot, String> {
    let manager = self.clone();
    // Client cancellation must not abandon an admitted creation between its
    // durable receipt and process startup. Repeating the UUID only observes it.
    tokio::spawn(async move { manager.create_inner(request, project, images).await })
      .await
      .map_err(|error| error.to_string())?
  }

  async fn create_inner(
    &self,
    request: CreateProjectSessionRequest,
    project: RegisteredProject,
    images: Vec<ResolvedImage>,
  ) -> Result<ManagedSessionSnapshot, String> {
    self.available()?;
    validate_creation_request(&request)?;
    if request.image_ids.len() != images.len()
      || request
        .image_ids
        .iter()
        .zip(&images)
        .any(|(id, image)| id != &image.image_id)
    {
      return Err("Image attachments did not match the creation request".into());
    }
    if let Some(snapshot) = self.creation_receipt(&request)? {
      return Ok(snapshot);
    }
    if request.project_id != project.project_id || !project.available {
      return Err("The registered project is unavailable".into());
    }
    let fingerprint = creation_fingerprint(&request, &project.folder_path)?;
    let receipt = Receipt::new(&request, &project, self.inner.provider, fingerprint);
    {
      let mut records = self.inner.records.lock().unwrap();
      if let Some(existing) = records.get(&request.request_id) {
        return duplicate(&existing.receipt, &receipt);
      }
      check_capacity(&records, true, true)?;
      // Reserve in memory before awaiting durable admission, so simultaneous
      // calls cannot launch two processes for one creation request.
      records.insert(request.request_id.clone(), Record::new(receipt.clone()));
    }
    let store = self.inner.store.clone();
    let proposed = receipt.clone();
    let admitted = tokio::task::spawn_blocking(move || store.admit(proposed))
      .await
      .map_err(|error| error.to_string())?;
    let (admitted, inserted) = match admitted {
      Ok(value) => value,
      Err(error) => {
        self.inner.records.lock().unwrap().remove(&request.request_id);
        return Err(error);
      }
    };
    if !inserted {
      self
        .inner
        .records
        .lock()
        .unwrap()
        .insert(request.request_id.clone(), Record::new(admitted.clone()));
      return duplicate(&admitted, &receipt);
    }
    if let Err(error) = self
      .start(request.request_id.clone(), Some((request.prompt, images)), false)
      .await
    {
      self
        .inner
        .mutate(&request.request_id, 0, |receipt| {
          receipt.status = ManagedSessionStatus::Failed;
          receipt.message = Some(error.clone());
        })
        .await?;
    }
    Ok(self.get_by_id(&request.request_id).unwrap())
  }

  pub fn get(&self, request: &ManagedSessionRequest) -> Result<Option<ManagedSessionSnapshot>, String> {
    let records = self.inner.records.lock().unwrap();
    let key = resolve(&records, request, self.inner.host.as_ref())?;
    Ok(key.and_then(|key| records.get(&key).map(|record| record.receipt.snapshot())))
  }

  /// Look up an already accepted creation before consulting mutable project
  /// availability. A retry observes its receipt even if the folder moved later.
  pub fn creation_receipt(
    &self,
    request: &CreateProjectSessionRequest,
  ) -> Result<Option<ManagedSessionSnapshot>, String> {
    self.available()?;
    validate_creation_request(request)?;
    let records = self.inner.records.lock().unwrap();
    let Some(record) = records.get(&request.request_id) else {
      return Ok(None);
    };
    let fingerprint = creation_fingerprint(request, &record.receipt.folder.to_string_lossy())?;
    if fingerprint != record.receipt.fingerprint {
      return Err("Creation request ID was already used with different content".into());
    }
    Ok(Some(record.receipt.snapshot()))
  }

  pub async fn stop(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    self.available()?;
    let (id, sender) = self.runtime(&request)?;
    let (reply, receive) = oneshot::channel();
    tokio::time::timeout(QUEUE_DEADLINE, sender.send(Command::Abort(reply)))
      .await
      .map_err(|_| "Agent command queue is busy".to_string())?
      .map_err(|_| "Agent runtime has stopped".to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(20), receive)
      .await
      .map_err(|_| "Agent did not acknowledge Stop before its deadline".to_string())?
      .map_err(|_| "Agent runtime stopped before acknowledging Stop".to_string())??;
    self.get_by_id(&id).ok_or_else(|| "Managed session disappeared".into())
  }

  pub async fn resume(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    let manager = self.clone();
    tokio::spawn(async move { manager.resume_inner(request).await })
      .await
      .map_err(|error| error.to_string())?
  }

  async fn resume_inner(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    self.available()?;
    let id = {
      let records = self.inner.records.lock().unwrap();
      let id =
        resolve(&records, &request, self.inner.host.as_ref())?.ok_or("This session was not managed by this host")?;
      id
    };
    self.start(id.clone(), None, true).await?;
    self.get_by_id(&id).ok_or_else(|| "Managed session disappeared".into())
  }

  pub async fn respond_dialog(&self, response: ManagedSessionDialogResponse) -> Result<ManagedSessionSnapshot, String> {
    self.available()?;
    let request = ManagedSessionRequest {
      request_id: Some(response.request_id.clone()),
      session_key: None,
    };
    let (id, sender) = self.runtime(&request)?;
    let (reply, receive) = oneshot::channel();
    tokio::time::timeout(QUEUE_DEADLINE, sender.send(Command::Dialog(response, reply)))
      .await
      .map_err(|_| "Agent command queue is busy".to_string())?
      .map_err(|_| "Agent runtime has stopped".to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(10), receive)
      .await
      .map_err(|_| "Agent did not receive the dialog response before its deadline".to_string())?
      .map_err(|_| "Agent runtime stopped before receiving the dialog response".to_string())??;
    self.get_by_id(&id).ok_or_else(|| "Managed session disappeared".into())
  }

  pub fn input_status(&self, session_key: &str) -> Option<Result<OwnedInputStatus, DeliveryError>> {
    self
      .inner
      .records
      .lock()
      .unwrap()
      .values()
      .find(|record| owns_key(&record.receipt, session_key, self.inner.host.as_ref()))
      .map(|record| {
        self.available().map_err(DeliveryError::not_sent)?;
        let resume_required = record.sender.is_none();
        Ok(OwnedInputStatus {
          message: if resume_required {
            "Send a message to resume this host-owned agent session".into()
          } else {
            "Send a message to this host-owned agent session".into()
          },
          resume_required,
          images_available: !resume_required && record.images_available,
        })
      })
  }

  pub async fn checked_input_status(&self, session_key: &str) -> Option<Result<OwnedInputStatus, DeliveryError>> {
    let status = self.input_status(session_key)?;
    let Ok(status) = status else {
      return Some(status);
    };
    if !status.resume_required {
      return Some(Ok(status));
    }
    let (receipt, gate) = {
      let records = self.inner.records.lock().unwrap();
      let record = records
        .values()
        .find(|record| owns_key(&record.receipt, session_key, self.inner.host.as_ref()))?;
      (record.receipt.clone(), record.start_gate.clone())
    };
    let _guard = gate.lock().await;
    // A concurrent Send may already have resumed it. Capability reads share
    // the startup gate so their temporary lease cannot interfere with Send.
    let status = match self.input_status(session_key)? {
      Ok(status) if status.resume_required => status,
      status => return Some(status),
    };
    let store = self.inner.store.clone();
    let validation = tokio::task::spawn_blocking(move || prepare_start(&store, receipt, true))
      .await
      .map_err(|error| error.to_string())
      .and_then(|result| result);
    Some(validation.map(|_| status).map_err(DeliveryError::not_sent))
  }

  pub async fn submit_owned_input(
    &self,
    session_key: &str,
    request_id: &str,
    text: &str,
  ) -> Result<Admission, DeliveryError> {
    self
      .submit_owned_input_images(session_key, request_id, text, Vec::new())
      .await
  }

  pub async fn submit_owned_input_images(
    &self,
    session_key: &str,
    request_id: &str,
    text: &str,
    images: Vec<ResolvedImage>,
  ) -> Result<Admission, DeliveryError> {
    self
      .submit_owned_input_with_model(session_key, request_id, text, images, None)
      .await
      .map(|admission| match admission {
        Admission::WithModel {
          disposition: ModelInputDisposition::Started,
          ..
        } => Admission::Started,
        Admission::WithModel {
          disposition: ModelInputDisposition::Steered,
          ..
        } => Admission::Steered,
        admission => admission,
      })
  }

  pub async fn submit_owned_input_with_model(
    &self,
    session_key: &str,
    request_id: &str,
    text: &str,
    images: Vec<ResolvedImage>,
    selection: Option<MessageModelSelection>,
  ) -> Result<Admission, DeliveryError> {
    validate_uuid(request_id, "message request")
      .and_then(|()| validate_message(text, !images.is_empty()))
      .map_err(DeliveryError::not_sent)?;
    let request = ManagedSessionRequest {
      request_id: None,
      session_key: Some(session_key.into()),
    };
    let id = {
      let records = self.inner.records.lock().unwrap();
      resolve(&records, &request, self.inner.host.as_ref())
        .map_err(DeliveryError::not_sent)?
        .ok_or_else(|| DeliveryError::not_sent("This session was not managed by this host"))?
    };
    let gate = self.start_gate(&id).map_err(DeliveryError::not_sent)?;
    let guard = self.admission_gate(&id, gate).await.map_err(DeliveryError::not_sent)?;
    self
      .start_locked(id, None, true)
      .await
      .map_err(DeliveryError::not_sent)?;
    let (_, sender, mut worker_state) = self.runtime_state(&request).map_err(DeliveryError::not_sent)?;
    loop {
      let state = worker_state.borrow_and_update().clone();
      match state {
        WorkerState::Ready => break,
        WorkerState::Closing(error) | WorkerState::Stopped(error) => {
          return Err(DeliveryError::not_sent(
            error.unwrap_or_else(|| "Agent runtime has stopped".into()),
          ));
        }
        WorkerState::Starting => {}
      }
      worker_state
        .changed()
        .await
        .map_err(|_| DeliveryError::not_sent("Agent stopped before it was ready to receive the message"))?;
    }
    self.available().map_err(DeliveryError::not_sent)?;
    let (reply, receive) = oneshot::channel();
    tokio::time::timeout(
      QUEUE_DEADLINE,
      sender.send(Command::Prompt {
        request_id: request_id.into(),
        text: text.into(),
        images,
        selection,
        reply,
      }),
    )
    .await
    .map_err(|_| DeliveryError::not_sent("Agent command queue is busy"))?
    .map_err(|_| DeliveryError::not_sent("Agent runtime has stopped"))?;
    let result = receive.await.map_err(|_| {
      DeliveryError::uncertain(
        "Agent runtime stopped before acknowledging the message; check its history before retrying",
      )
    })?;
    drop(guard);
    result
  }

  pub fn subscribe(&self) -> broadcast::Receiver<ManagedSessionSnapshot> {
    self.inner.events.subscribe()
  }

  pub async fn shutdown(&self) {
    self.inner.shutting_down.store(true, Ordering::Release);
    self.inner.shutdown.send_replace(true);
    let workers: Vec<_> = self
      .inner
      .records
      .lock()
      .unwrap()
      .values()
      .filter_map(|record| record.worker_state.clone())
      .collect();
    let mut waits = tokio::task::JoinSet::new();
    for mut state in workers {
      waits.spawn(async move {
        loop {
          if matches!(*state.borrow_and_update(), WorkerState::Stopped(_)) {
            break;
          }
          if state.changed().await.is_err() {
            break;
          }
        }
      });
    }
    while waits.join_next().await.is_some() {}
  }

  fn get_by_id(&self, id: &str) -> Option<ManagedSessionSnapshot> {
    self
      .inner
      .records
      .lock()
      .unwrap()
      .get(id)
      .map(|record| record.receipt.snapshot())
  }

  fn runtime(&self, request: &ManagedSessionRequest) -> Result<(String, mpsc::Sender<Command>), String> {
    self.runtime_state(request).map(|(id, sender, _)| (id, sender))
  }

  fn runtime_state(
    &self,
    request: &ManagedSessionRequest,
  ) -> Result<(String, mpsc::Sender<Command>, watch::Receiver<WorkerState>), String> {
    let records = self.inner.records.lock().unwrap();
    let id =
      resolve(&records, request, self.inner.host.as_ref())?.ok_or("This session was not managed by this host")?;
    let sender = records[&id]
      .sender
      .clone()
      .ok_or("This host-owned agent session is stopped. Resume it first.")?;
    let state = records[&id]
      .worker_state
      .clone()
      .ok_or("Agent runtime is unavailable")?;
    Ok((id, sender, state))
  }

  async fn start(&self, id: String, prompt: Option<(String, Vec<ResolvedImage>)>, resume: bool) -> Result<(), String> {
    let gate = self.start_gate(&id)?;
    let _guard = self.admission_gate(&id, gate).await?;
    self.start_locked(id, prompt, resume).await
  }

  fn start_gate(&self, id: &str) -> Result<Arc<AsyncMutex<()>>, String> {
    self
      .inner
      .records
      .lock()
      .unwrap()
      .get(id)
      .map(|record| record.start_gate.clone())
      .ok_or_else(|| "Managed session disappeared".into())
  }

  /// The per-session gate serializes startup and prompt admission with idle
  /// retirement. Never hold the records mutex across filesystem or RPC waits.
  async fn start_locked(
    &self,
    id: String,
    prompt: Option<(String, Vec<ResolvedImage>)>,
    resume: bool,
  ) -> Result<(), String> {
    self.available()?;
    let validation = {
      let records = self.inner.records.lock().unwrap();
      let record = records.get(&id).ok_or("Managed session disappeared")?;
      if record.sender.is_some() {
        return Ok(());
      }
      record.receipt.clone()
    };
    let store = self.inner.store.clone();
    let ownership = self
      .inner
      .ownership
      .clone()
      .ok_or("Managed session runtime is unavailable")?;
    let (lease, latest, native_lease) = tokio::task::spawn_blocking(move || {
      let (lease, latest) = prepare_start(&store, validation, resume)?;
      let native_lease = latest
        .session_id
        .as_deref()
        .map(|id| ownership.acquire(latest.provider.ok_or("Managed session provider is missing")?, id))
        .transpose()?;
      Ok::<_, String>((lease, latest, native_lease))
    })
    .await
    .map_err(|error| error.to_string())??;
    let (sender, receive) = mpsc::channel(COMMAND_CAPACITY);
    let (worker_state, state) = watch::channel(WorkerState::Starting);
    let termination = CancellationToken::new();
    let (receipt, generation) = {
      let mut records = self.inner.records.lock().unwrap();
      self.available()?;
      let additional = records
        .get(&id)
        .is_some_and(|record| record.receipt.status != ManagedSessionStatus::Starting && record.sender.is_none());
      check_capacity(&records, additional, false)?;
      let record = records.get_mut(&id).ok_or("Managed session disappeared")?;
      if record.sender.is_some() {
        return Ok(());
      }
      if latest.revision >= record.receipt.revision {
        record.receipt = latest;
      }
      record.generation = record.generation.saturating_add(1);
      record.receipt.status = ManagedSessionStatus::Starting;
      record.receipt.message = None;
      record.receipt.revision = record.receipt.revision.saturating_add(1);
      record.sender = Some(sender);
      record.worker_state = Some(state);
      record.instance_id = uuid::Uuid::new_v4().to_string();
      record.worker_pid = None;
      record.started_at_ms = None;
      record.termination = Some(termination.clone());
      record.images_available = false;
      record.native_lease = native_lease.clone();
      (record.receipt.clone(), record.generation)
    };
    if let Err(error) = self.inner.publish(receipt.clone()).await {
      {
        let mut records = self.inner.records.lock().unwrap();
        if let Some(record) = records.get_mut(&id) {
          record.sender = None;
          record.receipt.status = ManagedSessionStatus::Failed;
          record.receipt.message = Some(error.clone());
          record.receipt.revision = record.receipt.revision.saturating_add(1);
          record.native_lease = None;
          let _ = self.inner.events.send(record.receipt.snapshot());
        }
      }
      drop(native_lease);
      drop(lease);
      worker_state.send_replace(WorkerState::Stopped(Some(error.clone())));
      return Err(error);
    }
    let host = ManagedHostContext {
      inner: Arc::downgrade(&self.inner),
      id: id.clone(),
      generation,
    };
    let worker = ManagedWorker {
      host,
      lease,
      native_lease,
      receipt,
      prompt,
      resume,
      commands: receive,
      worker_state,
      termination,
    };
    tokio::spawn(self.inner.driver.spawn(worker));
    Ok(())
  }
}

impl Inner {
  async fn publish(&self, receipt: Receipt) -> Result<(), String> {
    let store = self.store.clone();
    let saved = receipt.clone();
    let result = tokio::task::spawn_blocking(move || store.save(saved))
      .await
      .map_err(|error| error.to_string())
      .and_then(|result| result);
    // Lifecycle failures must still reach the UI when the ownership disk is
    // unavailable. The actor treats the persistence error as terminal.
    let _ = self.events.send(receipt.snapshot());
    result
  }

  async fn mutate(&self, id: &str, generation: u64, update: impl FnOnce(&mut Receipt)) -> Result<(), String> {
    let receipt = {
      let mut records = self.records.lock().unwrap();
      let Some(record) = records.get_mut(id).filter(|record| record.generation == generation) else {
        return Ok(());
      };
      update(&mut record.receipt);
      record.receipt.revision = record.receipt.revision.saturating_add(1);
      record.receipt.clone()
    };
    self.publish(receipt).await
  }
}

fn prepare_start(store: &ReceiptStore, validation: Receipt, resume: bool) -> Result<(SessionLease, Receipt), String> {
  let lease = store.lease(&validation.request_id)?;
  let latest = store.latest(&validation.request_id)?.unwrap_or(validation.clone());
  if latest.fingerprint != validation.fingerprint
    || latest.provider != validation.provider
    || latest.folder != validation.folder
    || latest.project_id != validation.project_id
  {
    return Err("Managed session ownership changed; reopen the host before resuming".into());
  }
  if !latest.folder.is_dir()
    || std::fs::canonicalize(&latest.folder).map_err(|error| error.to_string())? != latest.folder
  {
    return Err("The registered project folder is unavailable".into());
  }
  if resume {
    store.validate_resume_target(&latest)?;
  }
  Ok((lease, latest))
}

fn resolve(
  records: &HashMap<String, Record>,
  request: &ManagedSessionRequest,
  host: &dyn ManagedSessionHost,
) -> Result<Option<String>, String> {
  match (&request.request_id, &request.session_key) {
    (Some(id), None) => {
      validate_uuid(id, "creation request")?;
      Ok(records.contains_key(id).then(|| id.clone()))
    }
    (None, Some(key)) => Ok(
      records
        .iter()
        .find(|(_, record)| owns_key(&record.receipt, key, host))
        .map(|(id, _)| id.clone()),
    ),
    _ => Err("Supply exactly one managed session identity".into()),
  }
}

fn owns_key(receipt: &Receipt, key: &str, host: &dyn ManagedSessionHost) -> bool {
  receipt
    .session_key
    .as_deref()
    .is_some_and(|owned| host.keys_equal(owned, key))
}

fn duplicate(existing: &Receipt, proposed: &Receipt) -> Result<ManagedSessionSnapshot, String> {
  if existing.fingerprint != proposed.fingerprint {
    return Err("Creation request ID was already used with different content".into());
  }
  Ok(existing.snapshot())
}

fn creation_fingerprint(request: &CreateProjectSessionRequest, folder: &str) -> Result<String, String> {
  Ok(format!(
    "{:x}",
    Sha256::digest(
      if let Some(model) = &request.model {
        serde_json::to_vec(&(
          &request.project_id,
          folder,
          &request.prompt,
          &request.image_ids,
          (&model.model_provider, &model.model_id),
        ))
      } else if request.image_ids.is_empty() {
        serde_json::to_vec(&(&request.project_id, folder, &request.prompt))
      } else {
        serde_json::to_vec(&(&request.project_id, folder, &request.prompt, &request.image_ids))
      }
      .map_err(|error| error.to_string())?
    )
  ))
}

fn check_capacity(records: &HashMap<String, Record>, additional: bool, new_receipt: bool) -> Result<(), String> {
  let active = records
    .values()
    .filter(|record| record.sender.is_some() || record.receipt.status == ManagedSessionStatus::Starting)
    .count();
  if active + usize::from(additional) > MAX_ACTIVE {
    return Err(format!(
      "At most {MAX_ACTIVE} host-owned agent processes may run at once"
    ));
  }
  if records.len() + usize::from(new_receipt) > MAX_RECEIPTS {
    return Err("Managed session receipt limit reached".into());
  }
  Ok(())
}

fn validate_uuid(value: &str, kind: &str) -> Result<(), String> {
  uuid::Uuid::parse_str(value)
    .map(|_| ())
    .map_err(|_| format!("Invalid {kind} ID"))
}

pub fn validate_creation_request(request: &CreateProjectSessionRequest) -> Result<(), String> {
  validate_uuid(&request.request_id, "creation request")?;
  validate_image_ids(&request.image_ids)?;
  if let Some(model) = &request.model {
    validate_creation_model(model)?;
  }
  validate_message(&request.prompt, !request.image_ids.is_empty())
}

pub fn validate_creation_model(model: &crate::ModelIdentity) -> Result<(), String> {
  if !valid_identity(&model.model_id)
    || model
      .model_provider
      .as_ref()
      .is_some_and(|value| !valid_identity(value))
    || model.display_name.as_ref().is_some_and(|value| !valid_identity(value))
  {
    return Err("Invalid creation model selection".into());
  }
  Ok(())
}

pub(crate) fn validate_message(value: &str, has_images: bool) -> Result<(), String> {
  if value.trim().is_empty() && !has_images {
    return Err("Message cannot be empty".into());
  }
  if value.chars().count() > MAX_INPUT_LENGTH {
    return Err(format!("Message must be at most {MAX_INPUT_LENGTH} characters"));
  }
  if value
    .chars()
    .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
  {
    return Err("Message contains unsupported control characters".into());
  }
  Ok(())
}

pub fn validate_image_ids(ids: &[String]) -> Result<(), String> {
  if ids.len() > MAX_IMAGES {
    return Err(format!("Attach at most {MAX_IMAGES} images"));
  }
  let mut seen = std::collections::HashSet::new();
  for id in ids {
    if uuid::Uuid::parse_str(id).is_err() || !seen.insert(id) {
      return Err("Invalid or duplicate image attachment ID".into());
    }
  }
  Ok(())
}

fn provider_name(provider: crate::Provider) -> &'static str {
  match provider {
    crate::Provider::Codex => "codex",
    crate::Provider::Pi => "pi",
    crate::Provider::OpenCode => "opencode",
    crate::Provider::Dsh => "dsh",
  }
}

struct DisabledDriver(crate::Provider);
impl ManagedSessionDriver for DisabledDriver {
  fn provider(&self) -> crate::Provider {
    self.0
  }
  fn label(&self) -> &str {
    "agent"
  }
  fn spawn(&self, _worker: ManagedWorker) -> crate::AdapterFuture<'static, ()> {
    Box::pin(async {})
  }
  fn validate_history(
    &self,
    _path: &std::path::Path,
    _session_id: &str,
    _folder: &std::path::Path,
  ) -> Result<Option<ManagedHistoryIdentity>, String> {
    Err("Managed sessions are unavailable".into())
  }
}
struct DisabledHost;
impl ManagedSessionHost for DisabledHost {
  fn session_key(&self, _identity: &ManagedHistoryIdentity) -> Result<String, String> {
    Err("Managed sessions are unavailable".into())
  }
}

#[cfg(test)]
mod tests;
