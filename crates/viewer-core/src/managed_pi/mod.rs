//! Pi processes explicitly created and owned by this viewer.
//!
//! The read-only provider remains the source of transcript data. This manager
//! publishes only lifecycle metadata and never manufactures history rows.
mod actor;
mod models;
mod persistence;
mod protocol;
mod runtimes;

#[cfg(test)]
use crate::input::ModelInputDisposition;
use crate::{
  images::ResolvedImage,
  input::{Admission, DeliveryError, MAX_INPUT_LENGTH},
  models::MessageModelSelection,
  workspace::{
    CreateProjectSessionRequest, ManagedSessionDialogResponse, ManagedSessionRequest, ManagedSessionSnapshot,
    ManagedSessionStatus, RegisteredProject,
  },
};
use persistence::{Receipt, ReceiptStore, SessionLease};
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

const MAX_ACTIVE: usize = 8;
const MAX_RECEIPTS: usize = 256;
const COMMAND_CAPACITY: usize = 16;
const QUEUE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[derive(Clone)]
pub(crate) struct ManagedPiSessions {
  inner: Arc<Inner>,
}

struct Inner {
  records: Mutex<HashMap<String, Record>>,
  store: ReceiptStore,
  events: broadcast::Sender<ManagedSessionSnapshot>,
  executable: PathBuf,
  unavailable: Option<String>,
  shutting_down: AtomicBool,
  shutdown: watch::Sender<bool>,
  idle_timeout: std::time::Duration,
}

struct Record {
  receipt: Receipt,
  generation: u64,
  sender: Option<mpsc::Sender<actor::Command>>,
  start_gate: Arc<AsyncMutex<()>>,
  worker_state: Option<watch::Receiver<WorkerState>>,
  instance_id: String,
  worker_pid: Option<u32>,
  started_at_ms: Option<i64>,
  termination: Option<CancellationToken>,
  restarting: Option<watch::Receiver<bool>>,
  images_available: bool,
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
    }
  }
}

#[derive(Clone)]
enum WorkerState {
  Starting,
  Ready,
  Closing(Option<String>),
  Stopped(Option<String>),
}

pub(crate) struct OwnedInputStatus {
  pub message: String,
  pub resume_required: bool,
  pub images_available: bool,
}

impl ManagedPiSessions {
  /// Called during blocking service setup. All async mutations persist through
  /// spawn_blocking; lookups never access disk.
  pub(crate) fn new(state_path: Option<PathBuf>) -> Result<Self, String> {
    let executable = std::env::var_os("TOKN_SESSION_PI_EXECUTABLE")
      .map(PathBuf::from)
      .unwrap_or_else(|| "pi".into());
    Self::with_executable(state_path, executable)
  }

  fn with_executable(state_path: Option<PathBuf>, executable: PathBuf) -> Result<Self, String> {
    Self::with_executable_and_idle_timeout(state_path, executable, IDLE_TIMEOUT)
  }

  fn with_executable_and_idle_timeout(
    state_path: Option<PathBuf>,
    executable: PathBuf,
    idle_timeout: std::time::Duration,
  ) -> Result<Self, String> {
    if executable.as_os_str().is_empty() {
      return Err("Pi executable path cannot be empty".into());
    }
    let (store, receipts) = ReceiptStore::new(state_path)?;
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
        executable,
        unavailable: None,
        shutting_down: AtomicBool::new(false),
        shutdown: watch::channel(false).0,
        idle_timeout,
      }),
    })
  }

  #[cfg(test)]
  pub(crate) fn with_test_executable(state_path: Option<PathBuf>, executable: PathBuf) -> Result<Self, String> {
    Self::with_executable(state_path, executable)
  }

  pub(crate) fn disabled(message: String) -> Self {
    let (events, _) = broadcast::channel(128);
    Self {
      inner: Arc::new(Inner {
        records: Mutex::new(HashMap::new()),
        store: ReceiptStore::disabled(),
        events,
        executable: "pi".into(),
        unavailable: Some(message),
        shutting_down: AtomicBool::new(false),
        shutdown: watch::channel(false).0,
        idle_timeout: IDLE_TIMEOUT,
      }),
    }
  }

  pub(crate) fn available(&self) -> Result<(), String> {
    if self.inner.shutting_down.load(Ordering::Acquire) {
      return Err("Managed Pi runtime is shutting down".into());
    }
    self.inner.unavailable.as_ref().map_or(Ok(()), |message| {
      Err(format!("Managed Pi sessions are unavailable: {message}"))
    })
  }

  #[cfg(test)]
  pub(crate) async fn create(
    &self,
    request: CreateProjectSessionRequest,
    project: RegisteredProject,
  ) -> Result<ManagedSessionSnapshot, String> {
    self.create_with_images(request, project, Vec::new()).await
  }

  pub(crate) async fn create_with_images(
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
    let receipt = Receipt::new(&request, &project, fingerprint);
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

  pub(crate) fn get(&self, request: &ManagedSessionRequest) -> Result<Option<ManagedSessionSnapshot>, String> {
    let records = self.inner.records.lock().unwrap();
    let key = resolve(&records, request)?;
    Ok(key.and_then(|key| records.get(&key).map(|record| record.receipt.snapshot())))
  }

  /// Look up an already accepted creation before consulting mutable project
  /// availability. A retry observes its receipt even if the folder moved later.
  pub(crate) fn creation_receipt(
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

  pub(crate) async fn stop(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    self.available()?;
    let (id, sender) = self.runtime(&request)?;
    let (reply, receive) = oneshot::channel();
    tokio::time::timeout(QUEUE_DEADLINE, sender.send(actor::Command::Abort(reply)))
      .await
      .map_err(|_| "Pi command queue is busy".to_string())?
      .map_err(|_| "Pi runtime has stopped".to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(20), receive)
      .await
      .map_err(|_| "Pi did not acknowledge Stop before its deadline".to_string())?
      .map_err(|_| "Pi runtime stopped before acknowledging Stop".to_string())??;
    self.get_by_id(&id).ok_or_else(|| "Managed session disappeared".into())
  }

  pub(crate) async fn resume(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    let manager = self.clone();
    tokio::spawn(async move { manager.resume_inner(request).await })
      .await
      .map_err(|error| error.to_string())?
  }

  async fn resume_inner(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    self.available()?;
    let id = {
      let records = self.inner.records.lock().unwrap();
      let id = resolve(&records, &request)?.ok_or("This session was not created by this viewer")?;
      id
    };
    self.start(id.clone(), None, true).await?;
    self.get_by_id(&id).ok_or_else(|| "Managed session disappeared".into())
  }

  pub(crate) async fn respond_dialog(
    &self,
    response: ManagedSessionDialogResponse,
  ) -> Result<ManagedSessionSnapshot, String> {
    self.available()?;
    let request = ManagedSessionRequest {
      request_id: Some(response.request_id.clone()),
      session_key: None,
    };
    let (id, sender) = self.runtime(&request)?;
    let (reply, receive) = oneshot::channel();
    tokio::time::timeout(QUEUE_DEADLINE, sender.send(actor::Command::Dialog(response, reply)))
      .await
      .map_err(|_| "Pi command queue is busy".to_string())?
      .map_err(|_| "Pi runtime has stopped".to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(10), receive)
      .await
      .map_err(|_| "Pi did not receive the dialog response before its deadline".to_string())?
      .map_err(|_| "Pi runtime stopped before receiving the dialog response".to_string())??;
    self.get_by_id(&id).ok_or_else(|| "Managed session disappeared".into())
  }

  pub(crate) fn input_status(&self, session_key: &str) -> Option<Result<OwnedInputStatus, DeliveryError>> {
    self
      .inner
      .records
      .lock()
      .unwrap()
      .values()
      .find(|record| owns_key(&record.receipt, session_key))
      .map(|record| {
        self.available().map_err(DeliveryError::not_sent)?;
        let resume_required = record.sender.is_none();
        Ok(OwnedInputStatus {
          message: if resume_required {
            "Send a message to resume this viewer-owned Pi session".into()
          } else {
            "Send a message to this viewer-owned Pi session".into()
          },
          resume_required,
          images_available: !resume_required && record.images_available,
        })
      })
  }

  pub(crate) async fn checked_input_status(
    &self,
    session_key: &str,
  ) -> Option<Result<OwnedInputStatus, DeliveryError>> {
    let status = self.input_status(session_key)?;
    let Ok(status) = status else {
      return Some(status);
    };
    if !status.resume_required {
      return Some(Ok(status));
    }
    let (receipt, gate) = {
      let records = self.inner.records.lock().unwrap();
      let record = records.values().find(|record| owns_key(&record.receipt, session_key))?;
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

  #[cfg(test)]
  pub(crate) async fn submit_owned_input(
    &self,
    session_key: &str,
    request_id: &str,
    text: &str,
  ) -> Result<Admission, DeliveryError> {
    self
      .submit_owned_input_images(session_key, request_id, text, Vec::new())
      .await
  }

  #[cfg(test)]
  pub(crate) async fn submit_owned_input_images(
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

  pub(crate) async fn submit_owned_input_with_model(
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
      resolve(&records, &request)
        .map_err(DeliveryError::not_sent)?
        .ok_or_else(|| DeliveryError::not_sent("This session was not created by this viewer"))?
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
            error.unwrap_or_else(|| "Pi runtime has stopped".into()),
          ));
        }
        WorkerState::Starting => {}
      }
      worker_state
        .changed()
        .await
        .map_err(|_| DeliveryError::not_sent("Pi stopped before it was ready to receive the message"))?;
    }
    self.available().map_err(DeliveryError::not_sent)?;
    let (reply, receive) = oneshot::channel();
    tokio::time::timeout(
      QUEUE_DEADLINE,
      sender.send(actor::Command::Prompt {
        request_id: request_id.into(),
        text: text.into(),
        images,
        selection,
        reply,
      }),
    )
    .await
    .map_err(|_| DeliveryError::not_sent("Pi command queue is busy"))?
    .map_err(|_| DeliveryError::not_sent("Pi runtime has stopped"))?;
    let result = receive.await.map_err(|_| {
      DeliveryError::uncertain("Pi runtime stopped before acknowledging the message; check its history before retrying")
    })?;
    drop(guard);
    result
  }

  pub(crate) fn subscribe(&self) -> broadcast::Receiver<ManagedSessionSnapshot> {
    self.inner.events.subscribe()
  }

  pub(crate) async fn shutdown(&self) {
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

  fn runtime(&self, request: &ManagedSessionRequest) -> Result<(String, mpsc::Sender<actor::Command>), String> {
    self.runtime_state(request).map(|(id, sender, _)| (id, sender))
  }

  fn runtime_state(
    &self,
    request: &ManagedSessionRequest,
  ) -> Result<(String, mpsc::Sender<actor::Command>, watch::Receiver<WorkerState>), String> {
    let records = self.inner.records.lock().unwrap();
    let id = resolve(&records, request)?.ok_or("This session was not created by this viewer")?;
    let sender = records[&id]
      .sender
      .clone()
      .ok_or("This viewer-owned Pi session is stopped. Resume it first.")?;
    let state = records[&id].worker_state.clone().ok_or("Pi runtime is unavailable")?;
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
    let (lease, latest) = tokio::task::spawn_blocking(move || prepare_start(&store, validation, resume))
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
      (record.receipt.clone(), record.generation)
    };
    if let Err(error) = self.inner.publish(receipt.clone()).await {
      let mut records = self.inner.records.lock().unwrap();
      if let Some(record) = records.get_mut(&id) {
        record.sender = None;
        record.receipt.status = ManagedSessionStatus::Failed;
        record.receipt.message = Some(error.clone());
        record.receipt.revision = record.receipt.revision.saturating_add(1);
        let _ = self.inner.events.send(record.receipt.snapshot());
      }
      worker_state.send_replace(WorkerState::Stopped(Some(error.clone())));
      return Err(error);
    }
    let inner = Arc::downgrade(&self.inner);
    let executable = self.inner.executable.clone();
    tokio::spawn(actor::run(
      inner,
      lease,
      executable,
      receipt,
      generation,
      prompt,
      resume,
      receive,
      worker_state,
      termination,
    ));
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
    || latest.folder != validation.folder
    || latest.project_id != validation.project_id
  {
    return Err("Managed Pi ownership changed; reopen the viewer before resuming".into());
  }
  if !latest.folder.is_dir()
    || std::fs::canonicalize(&latest.folder).map_err(|error| error.to_string())? != latest.folder
  {
    return Err("The registered project folder is unavailable".into());
  }
  if resume {
    validate_resume_target(&latest)?;
  }
  Ok((lease, latest))
}

fn validate_resume_target(receipt: &Receipt) -> Result<(), String> {
  if !receipt.folder.is_dir()
    || std::fs::canonicalize(&receipt.folder).map_err(|error| error.to_string())? != receipt.folder
  {
    return Err("The registered project folder is unavailable".into());
  }
  let file = receipt
    .session_file
    .as_ref()
    .ok_or("Pi has not persisted a session file to resume")?;
  let session_id = receipt
    .session_id
    .as_deref()
    .ok_or("Pi session identity is unavailable")?;
  if persistence::validate_session(file, session_id, &receipt.folder)?.is_none() {
    return Err("The owned Pi session file is unavailable".into());
  }
  Ok(())
}

fn resolve(records: &HashMap<String, Record>, request: &ManagedSessionRequest) -> Result<Option<String>, String> {
  match (&request.request_id, &request.session_key) {
    (Some(id), None) => {
      validate_uuid(id, "creation request")?;
      Ok(records.contains_key(id).then(|| id.clone()))
    }
    (None, Some(key)) => Ok(
      records
        .iter()
        .find(|(_, record)| owns_key(&record.receipt, key))
        .map(|(id, _)| id.clone()),
    ),
    _ => Err("Supply exactly one managed session identity".into()),
  }
}

fn owns_key(receipt: &Receipt, key: &str) -> bool {
  let Some(owned) = receipt.session_key.as_deref() else {
    return false;
  };
  owned == key
    || crate::model::decode_session_key(owned)
      .ok()
      .zip(crate::model::decode_session_key(key).ok())
      .is_some_and(|(owned, requested)| owned == requested)
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
      if request.image_ids.is_empty() {
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
      "At most {MAX_ACTIVE} viewer-owned Pi processes may run at once"
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

pub(crate) fn validate_creation_request(request: &CreateProjectSessionRequest) -> Result<(), String> {
  validate_uuid(&request.request_id, "creation request")?;
  crate::input::validate_image_ids(&request.image_ids)?;
  validate_message(&request.prompt, !request.image_ids.is_empty())
}

fn validate_message(value: &str, has_images: bool) -> Result<(), String> {
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

#[cfg(test)]
mod tests;
