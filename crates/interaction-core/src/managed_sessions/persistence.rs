use super::*;
use serde::{Deserialize, Serialize};
use std::{
  fs::{File, OpenOptions},
  path::Path,
};

const MAX_STATE_BYTES: u64 = 1024 * 1024;

pub struct SessionLease(File);

impl Drop for SessionLease {
  fn drop(&mut self) {
    // A concurrent fork can retain this open file description until exec.
    // Closing our descriptor alone would delay ownership handoff until then.
    let _ = self.0.unlock();
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
  /// Missing only while loading a legacy version-one catalog.
  #[serde(default)]
  pub provider: Option<crate::Provider>,
  pub request_id: String,
  pub project_id: String,
  pub folder: PathBuf,
  pub fingerprint: String,
  pub revision: u64,
  pub status: ManagedSessionStatus,
  pub session_id: Option<String>,
  pub session_file: Option<PathBuf>,
  pub session_key: Option<String>,
  pub message: Option<String>,
  #[serde(default)]
  pub last_turn_error: Option<String>,
  #[serde(skip)]
  pub dialog: Option<ManagedSessionDialog>,
}

impl Receipt {
  pub fn new(
    request: &CreateProjectSessionRequest,
    project: &RegisteredProject,
    provider: crate::Provider,
    fingerprint: String,
  ) -> Self {
    Self {
      provider: Some(provider),
      request_id: request.request_id.clone(),
      project_id: project.project_id.clone(),
      folder: project.folder_path.clone().into(),
      fingerprint,
      revision: 1,
      status: ManagedSessionStatus::Starting,
      session_id: None,
      session_file: None,
      session_key: None,
      message: None,
      last_turn_error: None,
      dialog: None,
    }
  }

  pub fn snapshot(&self) -> ManagedSessionSnapshot {
    ManagedSessionSnapshot {
      revision: self.revision,
      request_id: self.request_id.clone(),
      project_id: self.project_id.clone(),
      provider: self.provider.map(provider_name).unwrap_or("unknown").into(),
      status: self.status,
      session_id: self.session_id.clone(),
      session_key: self.session_key.clone(),
      message: self.message.clone(),
      last_turn_error: self.last_turn_error.clone(),
      pending_dialog: self.dialog.clone(),
    }
  }
}

#[derive(Default, Deserialize, Serialize)]
struct Saved {
  version: u8,
  receipts: Vec<Receipt>,
}

#[derive(Clone)]
pub struct ReceiptStore {
  path: Option<PathBuf>,
  ephemeral_locks: Option<Arc<tempfile::TempDir>>,
  legacy_provider: Option<crate::Provider>,
  lease_prefix: String,
  driver: Arc<dyn ManagedSessionDriver>,
  host: Arc<dyn ManagedSessionHost>,
}

impl ReceiptStore {
  pub fn new(
    configuration: ManagedSessionConfiguration,
    driver: Arc<dyn ManagedSessionDriver>,
    host: Arc<dyn ManagedSessionHost>,
  ) -> Result<(Self, Vec<Receipt>), String> {
    let path = configuration.state_path;
    if configuration.lease_prefix.is_empty()
      || !configuration
        .lease_prefix
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
      return Err("Invalid managed-session lease namespace".into());
    }
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = Self {
      path: path.clone(),
      ephemeral_locks: Some(Arc::new(temporary)),
      legacy_provider: configuration.legacy_provider,
      lease_prefix: configuration.lease_prefix,
      driver,
      host,
    };
    let mut saved = if let Some(path) = &path {
      tokn_session_storage::read_private_json::<Saved>(path, MAX_STATE_BYTES)?.unwrap_or_default()
    } else {
      Saved::default()
    };
    store.validate_saved(&mut saved)?;
    let mut restarted = Vec::new();
    for receipt in &mut saved.receipts {
      let lease = match store.lease(&receipt.request_id) {
        Ok(lease) => lease,
        Err(_) => {
          receipt.status = ManagedSessionStatus::Stopped;
          receipt.message = Some("This agent session is owned by another host process".into());
          continue;
        }
      };
      if let Some(latest) = store.latest(&receipt.request_id)? {
        *receipt = latest;
      }
      receipt.revision = receipt.revision.saturating_add(1);
      if receipt.status != ManagedSessionStatus::Failed {
        receipt.status = ManagedSessionStatus::Stopped;
        receipt.message =
          Some("The owning host runtime has stopped. Send a message to resume the persisted session.".into());
      }
      receipt.dialog = None;
      restarted.push((receipt.clone(), lease));
    }
    if let Some(path) = &path {
      // Revisions survive host restarts, so a delayed UI response cannot replace
      // a newer lifecycle snapshot after reconnecting.
      if !restarted.is_empty() {
        saved = update_saved(path, |current: Option<Saved>| {
          let mut current = current.unwrap_or_default();
          store.validate_saved(&mut current)?;
          for (receipt, _) in &restarted {
            merge(&mut current, receipt.clone());
          }
          current.version = 2;
          Ok(current)
        })?;
      } else {
        saved = tokn_session_storage::read_private_json::<Saved>(path, MAX_STATE_BYTES)?.unwrap_or_default();
        store.validate_saved(&mut saved)?;
      }
    }
    for receipt in &mut saved.receipts {
      if !restarted
        .iter()
        .any(|(restarted, _)| restarted.request_id == receipt.request_id)
      {
        receipt.status = ManagedSessionStatus::Stopped;
        receipt.message = Some("This agent session is owned by another host process".into());
        receipt.dialog = None;
      }
    }
    Ok((store, saved.receipts))
  }

  pub fn disabled(provider: crate::Provider) -> Self {
    Self {
      path: None,
      ephemeral_locks: None,
      legacy_provider: None,
      lease_prefix: "disabled".into(),
      driver: Arc::new(DisabledDriver(provider)),
      host: Arc::new(DisabledHost),
    }
  }

  /// Call only after acquiring this receipt's process lease when preparing a
  /// resume, so another owner cannot advance its revision during the handoff.
  pub fn latest(&self, id: &str) -> Result<Option<Receipt>, String> {
    let Some(path) = &self.path else {
      return Ok(None);
    };
    let mut saved = tokn_session_storage::read_private_json::<Saved>(path, MAX_STATE_BYTES)?.unwrap_or_default();
    self.validate_saved(&mut saved)?;
    Ok(saved.receipts.into_iter().find(|receipt| receipt.request_id == id))
  }

  pub fn admit(&self, receipt: Receipt) -> Result<(Receipt, bool), String> {
    self.validate_receipt(&receipt)?;
    let Some(path) = &self.path else {
      return Ok((receipt, true));
    };
    let mut inserted = false;
    let id = receipt.request_id.clone();
    let saved = update_saved(path, |current: Option<Saved>| {
      let mut current = current.unwrap_or_default();
      self.validate_saved(&mut current)?;
      if !current
        .receipts
        .iter()
        .any(|existing| existing.request_id == receipt.request_id)
      {
        if current.receipts.len() >= MAX_RECEIPTS {
          return Err("Managed session receipt limit reached".into());
        }
        current.receipts.push(receipt.clone());
        inserted = true;
      }
      current.version = 2;
      Ok(current)
    })?;
    Ok((
      saved
        .receipts
        .into_iter()
        .find(|receipt| receipt.request_id == id)
        .ok_or("Managed session receipt was not saved")?,
      inserted,
    ))
  }

  pub fn save(&self, receipt: Receipt) -> Result<(), String> {
    self.validate_receipt(&receipt)?;
    let Some(path) = &self.path else {
      return Ok(());
    };
    update_saved(path, |current: Option<Saved>| {
      let mut current = current.unwrap_or_default();
      self.validate_saved(&mut current)?;
      merge(&mut current, receipt.clone());
      current.version = 2;
      Ok(current)
    })
    .map(|_| ())
  }

  pub fn lease(&self, id: &str) -> Result<SessionLease, String> {
    validate_uuid(id, "creation request")?;
    let directory = self
      .path
      .as_ref()
      .and_then(|path| path.parent())
      .or_else(|| self.ephemeral_locks.as_ref().map(|directory| directory.path()))
      .ok_or("Managed session runtime is unavailable")?;
    let path = directory.join(format!("{}-{id}.lease", self.lease_prefix));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
      .open(&path)
      .map_err(|error| format!("Could not acquire managed session ownership: {error}"))?;
    if !file.metadata().map_err(|error| error.to_string())?.is_file() {
      return Err("Managed session ownership requires a regular lease file".into());
    }
    file
      .try_lock()
      .map_err(|error| format!("This agent session already has an owning host runtime: {error}"))?;
    Ok(SessionLease(file))
  }
}

/// Distinct owned actors share one atomic receipt file. The storage helper's
/// nonblocking lock can briefly be held by another lifecycle update, so retry
/// only that specific contention error within a fixed two-second budget. All
/// callers execute on a blocking thread (or blocking service setup).
fn update_saved(path: &Path, mut update: impl FnMut(Option<Saved>) -> Result<Saved, String>) -> Result<Saved, String> {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
  loop {
    match tokn_session_storage::update_private_json(path, MAX_STATE_BYTES, &mut update) {
      Err(error) if error.starts_with("Storage is in use;") && std::time::Instant::now() < deadline => {
        std::thread::sleep(std::time::Duration::from_millis(10));
      }
      result => return result,
    }
  }
}

impl ReceiptStore {
  fn validate_receipt(&self, receipt: &Receipt) -> Result<(), String> {
    self.validate_saved(&mut Saved {
      version: 2,
      receipts: vec![receipt.clone()],
    })
  }

  fn validate_saved(&self, saved: &mut Saved) -> Result<(), String> {
    if saved.version > 2 || (saved.version == 0 && !saved.receipts.is_empty()) || saved.receipts.len() > MAX_RECEIPTS {
      return Err("Unsupported managed session ownership file".into());
    }
    let mut ids = std::collections::HashSet::new();
    for receipt in &mut saved.receipts {
      if receipt.provider.is_none() && saved.version == 1 {
        receipt.provider = self.legacy_provider;
      }
      let provider = receipt.provider.ok_or(if saved.version == 1 {
        "Legacy managed-session receipts require an explicit provider migration"
      } else {
        "Managed session receipt has no provider identity"
      })?;
      if provider != self.driver.provider() {
        return Err("Managed session receipt belongs to a different provider".into());
      }
      validate_uuid(&receipt.request_id, "saved creation request")?;
      if !ids.insert(&receipt.request_id) || !receipt.folder.is_absolute() || receipt.fingerprint.len() != 64 {
        return Err("Invalid managed session ownership receipt".into());
      }
      if receipt.session_file.as_ref().is_some_and(|path| !path.is_absolute()) {
        return Err("Managed session path must be absolute".into());
      }
      match (&receipt.session_id, &receipt.session_file, &receipt.session_key) {
        (None, None, None) => {}
        (Some(id), Some(path), Some(key)) => {
          self.host.validate_identity(
            &ManagedHistoryIdentity {
              provider,
              session_id: id.clone(),
              session_file: path.clone(),
            },
            key,
          )?;
        }
        _ => return Err("Managed session ownership identity is incomplete".into()),
      }
    }
    Ok(())
  }

  pub fn validate_resume_target(&self, receipt: &Receipt) -> Result<(), String> {
    if !receipt.folder.is_dir()
      || std::fs::canonicalize(&receipt.folder).map_err(|error| error.to_string())? != receipt.folder
    {
      return Err("The registered project folder is unavailable".into());
    }
    let file = receipt
      .session_file
      .as_ref()
      .ok_or("The agent has not persisted a session file to resume")?;
    let id = receipt
      .session_id
      .as_deref()
      .ok_or("The agent session identity is unavailable")?;
    let identity = self
      .driver
      .validate_history(file, id, &receipt.folder)?
      .ok_or("The owned session file is unavailable")?;
    if identity.provider != self.driver.provider() || identity.session_id != id || &identity.session_file != file {
      return Err("Managed session ownership identity is inconsistent".into());
    }
    self.host.validate_identity(
      &identity,
      receipt
        .session_key
        .as_deref()
        .ok_or("The agent session key is unavailable")?,
    )
  }
}

fn merge(saved: &mut Saved, receipt: Receipt) {
  if let Some(existing) = saved
    .receipts
    .iter_mut()
    .find(|existing| existing.request_id == receipt.request_id)
  {
    if receipt.revision >= existing.revision {
      *existing = receipt;
    }
  } else {
    saved.receipts.push(receipt);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct TestDriver;
  impl ManagedSessionDriver for TestDriver {
    fn provider(&self) -> crate::Provider {
      crate::Provider::Pi
    }
    fn label(&self) -> &str {
      "test"
    }
    fn spawn(&self, _worker: ManagedWorker) -> crate::AdapterFuture<'static, ()> {
      Box::pin(async {})
    }
    fn validate_history(
      &self,
      path: &Path,
      session_id: &str,
      _folder: &Path,
    ) -> Result<Option<ManagedHistoryIdentity>, String> {
      Ok(Some(ManagedHistoryIdentity {
        provider: self.provider(),
        session_id: session_id.into(),
        session_file: path.into(),
      }))
    }
  }
  struct TestHost;
  impl ManagedSessionHost for TestHost {
    fn session_key(&self, identity: &ManagedHistoryIdentity) -> Result<String, String> {
      Ok(format!("{}:{}", identity.session_id, identity.session_file.display()))
    }
  }
  fn new_store(path: Option<PathBuf>) -> Result<(ReceiptStore, Vec<Receipt>), String> {
    ReceiptStore::new(
      ManagedSessionConfiguration {
        state_path: path,
        idle_timeout: IDLE_TIMEOUT,
        legacy_provider: Some(crate::Provider::Pi),
        lease_prefix: "managed-pi".into(),
      },
      Arc::new(TestDriver),
      Arc::new(TestHost),
    )
  }

  fn receipt(folder: &Path) -> Receipt {
    Receipt::new(
      &CreateProjectSessionRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        project_id: "project".into(),
        prompt: "private prompt is never persisted".into(),
        image_ids: Vec::new(),
      },
      &RegisteredProject {
        project_id: "project".into(),
        folder_path: folder.canonicalize().unwrap().to_string_lossy().into(),
        available: true,
      },
      crate::Provider::Pi,
      "a".repeat(64),
    )
  }

  #[test]
  fn durable_admission_is_idempotent_and_never_persists_prompt_text() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state/managed.json");
    let (store, _) = new_store(Some(path.clone())).unwrap();
    let original = receipt(directory.path());
    let (_, inserted) = store.admit(original.clone()).unwrap();
    assert!(inserted);
    let mut changed = original.clone();
    changed.fingerprint = "b".repeat(64);
    let (existing, inserted) = store.admit(changed).unwrap();
    assert!(!inserted);
    assert_eq!(existing.fingerprint, original.fingerprint);
    assert!(!std::fs::read_to_string(path).unwrap().contains("private prompt"));
  }

  #[test]
  fn restart_does_not_claim_a_live_owner_and_releases_lease_on_drop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("managed.json");
    let (store, _) = new_store(Some(path.clone())).unwrap();
    let mut original = receipt(directory.path());
    original.status = ManagedSessionStatus::Running;
    store.admit(original.clone()).unwrap();
    let lease = store.lease(&original.request_id).unwrap();
    assert!(store.lease(&original.request_id).is_err());
    let (_, concurrent) = new_store(Some(path.clone())).unwrap();
    assert_eq!(concurrent[0].revision, original.revision);
    assert!(concurrent[0].message.as_deref().unwrap().contains("another host"));
    let disk: Saved = tokn_session_storage::read_private_json(&path, MAX_STATE_BYTES)
      .unwrap()
      .unwrap();
    assert_eq!(disk.receipts[0].status, ManagedSessionStatus::Running);
    drop(lease);
    let (_, restarted) = new_store(Some(path)).unwrap();
    assert_eq!(restarted[0].status, ManagedSessionStatus::Stopped);
    assert_eq!(restarted[0].revision, original.revision + 1);
    assert!(store.lease(&original.request_id).is_ok());
  }

  #[cfg(unix)]
  #[test]
  fn releasing_ownership_unlocks_descriptors_inherited_by_another_child() {
    let directory = tempfile::tempdir().unwrap();
    let (store, _) = new_store(Some(directory.path().join("managed.json"))).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let lease = store.lease(&id).unwrap();
    // Model the shared file description a concurrent fork can retain before
    // exec closes its inherited descriptors. Keep it open through the handoff.
    let inherited = lease.0.try_clone().unwrap();
    assert!(store.lease(&id).is_err());
    drop(lease);
    let next = store
      .lease(&id)
      .expect("completed ownership must release its lock immediately");
    drop(inherited);
    assert!(
      store.lease(&id).is_err(),
      "closing the old descriptor must not unlock the new owner"
    );
    drop(next);
    assert!(store.lease(&id).is_ok());
  }

  #[test]
  fn stale_metadata_cannot_overwrite_a_later_revision() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("managed.json");
    let (store, _) = new_store(Some(path.clone())).unwrap();
    let original = receipt(directory.path());
    store.admit(original.clone()).unwrap();
    let mut newer = original.clone();
    newer.revision += 2;
    newer.status = ManagedSessionStatus::Idle;
    store.save(newer.clone()).unwrap();
    store.save(original).unwrap();
    let disk: Saved = tokn_session_storage::read_private_json(&path, MAX_STATE_BYTES)
      .unwrap()
      .unwrap();
    assert_eq!(disk.receipts[0].revision, newer.revision);
    assert_eq!(disk.receipts[0].status, ManagedSessionStatus::Idle);
  }

  #[test]
  fn current_catalogs_never_guess_missing_or_conflicting_provider_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("managed.json");
    let (store, _) = new_store(Some(path.clone())).unwrap();
    store.admit(receipt(directory.path())).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (provider, expected) in [
      (serde_json::Value::Null, "no provider identity"),
      (serde_json::json!("codex"), "different provider"),
    ] {
      let mut invalid = saved.clone();
      invalid["receipts"][0]["provider"] = provider;
      let bytes = serde_json::to_vec(&invalid).unwrap();
      std::fs::write(&path, &bytes).unwrap();
      let error = new_store(Some(path.clone())).err().unwrap();
      assert!(error.contains(expected), "{error}");
      assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
  }

  #[test]
  fn short_receipt_lock_contention_does_not_fail_an_independent_actor() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("managed.json");
    let (store, _) = new_store(Some(path.clone())).unwrap();
    let mut original = receipt(directory.path());
    store.admit(original.clone()).unwrap();
    let lock = OpenOptions::new()
      .read(true)
      .write(true)
      .open(directory.path().join("managed.json.lock"))
      .unwrap();
    lock.try_lock().unwrap();
    original.revision += 1;
    original.status = ManagedSessionStatus::Idle;
    let writer = std::thread::spawn(move || store.save(original));
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(!writer.is_finished());
    drop(lock);
    writer.join().unwrap().unwrap();
    let saved: Saved = tokn_session_storage::read_private_json(&path, MAX_STATE_BYTES)
      .unwrap()
      .unwrap();
    assert_eq!(saved.receipts[0].status, ManagedSessionStatus::Idle);
  }
}
