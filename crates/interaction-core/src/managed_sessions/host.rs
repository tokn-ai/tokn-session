//! Narrow worker access to generation-fenced supervisor state.
use super::*;
use std::{path::Path, sync::Weak};
use tokio::sync::OwnedMutexGuard;

/// A worker can report its own lifecycle and request shared ownership actions.
/// It cannot inspect or mutate the supervisor's registry or another worker.
#[derive(Clone)]
pub struct ManagedHostContext {
  pub(super) inner: Weak<Inner>,
  pub(super) id: String,
  pub(super) generation: u64,
}

impl ManagedHostContext {
  pub fn instance_id(&self) -> Option<String> {
    let inner = self.inner.upgrade()?;
    let records = inner.records.lock().unwrap();
    records
      .get(&self.id)
      .filter(|record| record.generation == self.generation)
      .map(|record| record.instance_id.clone())
  }

  pub fn is_shutting_down(&self) -> bool {
    self
      .inner
      .upgrade()
      .is_none_or(|inner| inner.shutting_down.load(Ordering::Acquire))
  }

  pub fn shutdown(&self) -> Result<watch::Receiver<bool>, String> {
    self
      .inner
      .upgrade()
      .map(|inner| inner.shutdown.subscribe())
      .ok_or("Managed session runtime has stopped".into())
  }

  pub fn idle_timeout(&self) -> std::time::Duration {
    self
      .inner
      .upgrade()
      .map(|inner| inner.idle_timeout)
      .unwrap_or(IDLE_TIMEOUT)
  }

  pub fn worker_started(&self, pid: Option<u32>) {
    let Some(inner) = self.inner.upgrade() else {
      return;
    };
    let snapshot = {
      let mut records = inner.records.lock().unwrap();
      records
        .get_mut(&self.id)
        .filter(|record| record.generation == self.generation)
        .map(|record| {
          record.worker_pid = pid;
          record.started_at_ms = Some(now_ms());
          record.receipt.snapshot()
        })
    };
    if let Some(snapshot) = snapshot {
      let _ = inner.events.send(snapshot);
    }
  }

  pub async fn acquire_native_ownership(&self, native_id: &str) -> Result<NativeSessionLease, String> {
    let inner = self.inner.upgrade().ok_or("Managed session runtime has stopped")?;
    let ownership = inner
      .ownership
      .clone()
      .ok_or("Managed session runtime is unavailable")?;
    let provider = inner.provider;
    let native_id = native_id.to_owned();
    let lease = tokio::task::spawn_blocking(move || ownership.acquire(provider, &native_id))
      .await
      .map_err(|error| error.to_string())??;
    let mut records = inner.records.lock().unwrap();
    let record = records
      .get_mut(&self.id)
      .filter(|record| record.generation == self.generation)
      .ok_or("Managed session worker changed while claiming native session ownership")?;
    record.native_lease = Some(lease.clone());
    Ok(lease)
  }

  pub fn set_images_available(&self, available: bool) {
    if let Some(inner) = self.inner.upgrade() {
      if let Some(record) = inner
        .records
        .lock()
        .unwrap()
        .get_mut(&self.id)
        .filter(|record| record.generation == self.generation)
      {
        record.images_available = available;
      }
    }
  }

  pub async fn validate_history(
    &self,
    path: &Path,
    session_id: &str,
    folder: &Path,
  ) -> Result<Option<(PathBuf, String)>, String> {
    let inner = self.inner.upgrade().ok_or("Managed session runtime has stopped")?;
    let driver = inner.driver.clone();
    let host = inner.host.clone();
    let path = path.to_owned();
    let session_id = session_id.to_owned();
    let folder = folder.to_owned();
    tokio::task::spawn_blocking(move || {
      let Some(identity) = driver.validate_history(&path, &session_id, &folder)? else {
        return Ok(None);
      };
      if identity.provider != driver.provider()
        || identity.session_id != session_id
        || !identity.session_file.is_absolute()
      {
        return Err("Managed session history identity is inconsistent".into());
      }
      let key = host.session_key(&identity)?;
      Ok(Some((identity.session_file, key)))
    })
    .await
    .map_err(|error| error.to_string())?
  }

  /// A driver owns lifecycle fields for one generation. Immutable creation
  /// intent stays supervisor-owned and revisions are always advanced here.
  pub async fn update(&self, receipt: Receipt) -> Result<(), String> {
    let Some(inner) = self.inner.upgrade() else {
      return Ok(());
    };
    {
      let records = inner.records.lock().unwrap();
      let Some(record) = records
        .get(&self.id)
        .filter(|record| record.generation == self.generation)
      else {
        return Ok(());
      };
      if receipt.request_id != record.receipt.request_id
        || receipt.provider != record.receipt.provider
        || receipt.project_id != record.receipt.project_id
        || receipt.folder != record.receipt.folder
        || receipt.fingerprint != record.receipt.fingerprint
      {
        return Err("Managed session worker changed immutable creation intent".into());
      }
    }
    match (&receipt.session_id, &receipt.session_file, &receipt.session_key) {
      (None, None, None) => {}
      (Some(session_id), Some(session_file), Some(key)) => {
        inner.host.validate_identity(
          &ManagedHistoryIdentity {
            provider: inner.provider,
            session_id: session_id.clone(),
            session_file: session_file.clone(),
          },
          key,
        )?;
      }
      _ => return Err("Managed session ownership identity is incomplete".into()),
    }
    inner
      .mutate(&self.id, self.generation, |current| {
        let revision = current.revision;
        *current = receipt;
        current.revision = revision;
      })
      .await
  }

  pub async fn retirement_gate(&self) -> Option<OwnedMutexGuard<()>> {
    let gate = self.gate()?;
    Some(gate.lock_owned().await)
  }

  pub fn try_retirement_gate(&self) -> Option<OwnedMutexGuard<()>> {
    self.gate()?.try_lock_owned().ok()
  }

  fn gate(&self) -> Option<Arc<AsyncMutex<()>>> {
    let inner = self.inner.upgrade()?;
    let records = inner.records.lock().unwrap();
    records
      .get(&self.id)
      .filter(|record| record.generation == self.generation)
      .map(|record| record.start_gate.clone())
  }

  pub async fn finish(&self, error: Option<String>) -> Option<ManagedSessionSnapshot> {
    let inner = self.inner.upgrade()?;
    let receipt = {
      let records = inner.records.lock().unwrap();
      let record = records
        .get(&self.id)
        .filter(|record| record.generation == self.generation)?;
      let mut receipt = record.receipt.clone();
      receipt.status = if error.is_some() {
        ManagedSessionStatus::Failed
      } else {
        ManagedSessionStatus::Stopped
      };
      receipt.message = error;
      receipt.dialog = None;
      receipt.revision = receipt.revision.saturating_add(1);
      receipt
    };
    let store = inner.store.clone();
    let saved = receipt.clone();
    let _ = tokio::task::spawn_blocking(move || store.save(saved)).await;
    let mut records = inner.records.lock().unwrap();
    if let Some(record) = records
      .get_mut(&self.id)
      .filter(|record| record.generation == self.generation)
    {
      record.receipt = receipt.clone();
      record.sender = None;
      record.worker_pid = None;
      record.started_at_ms = None;
      record.termination = None;
      record.images_available = false;
    }
    Some(receipt.snapshot())
  }

  pub fn release_native_ownership(&self) {
    if let Some(inner) = self.inner.upgrade() {
      if let Some(record) = inner
        .records
        .lock()
        .unwrap()
        .get_mut(&self.id)
        .filter(|record| record.generation == self.generation)
      {
        record.native_lease = None;
      }
    }
  }

  /// Publish after cleanup has released both leases; observers may immediately
  /// attempt another start when they receive the stopped snapshot.
  pub fn publish_finished(&self, snapshot: Option<ManagedSessionSnapshot>) {
    if let (Some(inner), Some(snapshot)) = (self.inner.upgrade(), snapshot) {
      let _ = inner.events.send(snapshot);
    }
  }
}

fn now_ms() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis()
    .try_into()
    .unwrap_or(i64::MAX)
}
