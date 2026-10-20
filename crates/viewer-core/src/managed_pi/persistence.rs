use super::*;
use crate::workspace::ManagedSessionDialog;
use serde::{Deserialize, Serialize};
use std::{
  fs::{File, OpenOptions},
  path::Path,
};

const MAX_STATE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Receipt {
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
  pub fn new(request: &CreateProjectSessionRequest, project: &RegisteredProject, fingerprint: String) -> Self {
    Self {
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
      provider: "pi".into(),
      status: self.status,
      session_id: self.session_id.clone(),
      session_key: self.session_key.clone(),
      session: None,
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
pub(super) struct ReceiptStore {
  path: Option<PathBuf>,
  ephemeral_locks: Option<Arc<tempfile::TempDir>>,
}

impl ReceiptStore {
  pub fn new(path: Option<PathBuf>) -> Result<(Self, Vec<Receipt>), String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = Self {
      path: path.clone(),
      ephemeral_locks: Some(Arc::new(temporary)),
    };
    let mut saved = if let Some(path) = &path {
      tokn_session_storage::read_private_json::<Saved>(path, MAX_STATE_BYTES)?.unwrap_or_default()
    } else {
      Saved::default()
    };
    validate_saved(&saved)?;
    let mut restarted = Vec::new();
    for receipt in &mut saved.receipts {
      let lease = match store.lease(&receipt.request_id) {
        Ok(lease) => lease,
        Err(_) => {
          receipt.status = ManagedSessionStatus::Stopped;
          receipt.message = Some("This Pi session is owned by another viewer process".into());
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
          Some("The owning viewer runtime has stopped. Send a message to resume the persisted session.".into());
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
          validate_saved(&current)?;
          for (receipt, _) in &restarted {
            merge(&mut current, receipt.clone());
          }
          current.version = 1;
          Ok(current)
        })?;
      } else {
        saved = tokn_session_storage::read_private_json::<Saved>(path, MAX_STATE_BYTES)?.unwrap_or_default();
        validate_saved(&saved)?;
      }
    }
    for receipt in &mut saved.receipts {
      if !restarted
        .iter()
        .any(|(restarted, _)| restarted.request_id == receipt.request_id)
      {
        receipt.status = ManagedSessionStatus::Stopped;
        receipt.message = Some("This Pi session is owned by another viewer process".into());
        receipt.dialog = None;
      }
    }
    Ok((store, saved.receipts))
  }

  pub fn disabled() -> Self {
    Self {
      path: None,
      ephemeral_locks: None,
    }
  }

  /// Call only after acquiring this receipt's process lease when preparing a
  /// resume, so another owner cannot advance its revision during the handoff.
  pub fn latest(&self, id: &str) -> Result<Option<Receipt>, String> {
    let Some(path) = &self.path else {
      return Ok(None);
    };
    let saved = tokn_session_storage::read_private_json::<Saved>(path, MAX_STATE_BYTES)?.unwrap_or_default();
    validate_saved(&saved)?;
    Ok(saved.receipts.into_iter().find(|receipt| receipt.request_id == id))
  }

  pub fn admit(&self, receipt: Receipt) -> Result<(Receipt, bool), String> {
    let Some(path) = &self.path else {
      return Ok((receipt, true));
    };
    let mut inserted = false;
    let id = receipt.request_id.clone();
    let saved = update_saved(path, |current: Option<Saved>| {
      let mut current = current.unwrap_or_default();
      validate_saved(&current)?;
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
      current.version = 1;
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
    let Some(path) = &self.path else {
      return Ok(());
    };
    update_saved(path, |current: Option<Saved>| {
      let mut current = current.unwrap_or_default();
      validate_saved(&current)?;
      merge(&mut current, receipt.clone());
      current.version = 1;
      Ok(current)
    })
    .map(|_| ())
  }

  pub fn lease(&self, id: &str) -> Result<File, String> {
    validate_uuid(id, "creation request")?;
    let directory = self
      .path
      .as_ref()
      .and_then(|path| path.parent())
      .or_else(|| self.ephemeral_locks.as_ref().map(|directory| directory.path()))
      .ok_or("Managed Pi runtime is unavailable")?;
    let path = directory.join(format!("managed-pi-{id}.lease"));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
      .open(&path)
      .map_err(|error| format!("Could not acquire managed Pi ownership: {error}"))?;
    if !file.metadata().map_err(|error| error.to_string())?.is_file() {
      return Err("Managed Pi ownership requires a regular lease file".into());
    }
    file
      .try_lock()
      .map_err(|error| format!("This Pi session already has an owning viewer runtime: {error}"))?;
    Ok(file)
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

fn validate_saved(saved: &Saved) -> Result<(), String> {
  if saved.version > 1 || (saved.version == 0 && !saved.receipts.is_empty()) || saved.receipts.len() > MAX_RECEIPTS {
    return Err("Unsupported managed Pi ownership file".into());
  }
  let mut ids = std::collections::HashSet::new();
  for receipt in &saved.receipts {
    validate_uuid(&receipt.request_id, "saved creation request")?;
    if !ids.insert(&receipt.request_id) || !receipt.folder.is_absolute() || receipt.fingerprint.len() != 64 {
      return Err("Invalid managed Pi ownership receipt".into());
    }
    if receipt.session_file.as_ref().is_some_and(|path| !path.is_absolute()) {
      return Err("Managed Pi session path must be absolute".into());
    }
    match (&receipt.session_id, &receipt.session_file, &receipt.session_key) {
      (None, None, None) => {}
      (Some(id), Some(path), Some(key)) => {
        let locator = crate::model::decode_session_key(key)?;
        if locator.provider != crate::model::ViewerProvider::Pi
          || &locator.session_id != id
          || &locator.source_path != path
        {
          return Err("Managed Pi ownership identity is inconsistent".into());
        }
      }
      _ => return Err("Managed Pi ownership identity is incomplete".into()),
    }
  }
  Ok(())
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

/// Only the native header is read, with a strict bound independent of the
/// provider's historical reader. A planned file name alone is never identity.
pub(super) fn validate_session(
  path: &Path,
  session_id: &str,
  folder: &Path,
) -> Result<Option<(PathBuf, String)>, String> {
  use std::io::{BufRead, BufReader, Read};
  let file = match File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(format!("Could not inspect Pi history: {error}")),
  };
  if !file.metadata().map_err(|error| error.to_string())?.is_file() {
    return Err("Pi session history must be a regular file".into());
  }
  let mut line = String::new();
  BufReader::new(file.take(65537))
    .read_line(&mut line)
    .map_err(|error| error.to_string())?;
  if line.len() > 65536 {
    return Err("Pi session header exceeds its size limit".into());
  }
  if !line.ends_with('\n') {
    return Ok(None);
  }
  let header: serde_json::Value = serde_json::from_str(&line).map_err(|_| "Pi persisted an invalid session header")?;
  if header.get("type").and_then(|value| value.as_str()) != Some("session")
    || header.get("id").and_then(|value| value.as_str()) != Some(session_id)
  {
    return Err("Pi persisted a different session identity".into());
  }
  let cwd = header
    .get("cwd")
    .and_then(|value| value.as_str())
    .ok_or("Pi session header has no working directory")?;
  if std::fs::canonicalize(cwd).map_err(|error| error.to_string())? != folder
    || std::fs::canonicalize(folder).map_err(|error| error.to_string())? != folder
  {
    return Err("Pi session working directory differs from its registered project".into());
  }
  let path = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
  let key = crate::model::encode_session_key(&crate::model::SessionLocator {
    version: 1,
    provider: crate::model::ViewerProvider::Pi,
    session_id: session_id.into(),
    source_path: path.clone(),
  })?;
  Ok(Some((path, key)))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn receipt(folder: &Path) -> Receipt {
    Receipt::new(
      &CreateProjectSessionRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        project_id: "project".into(),
        prompt: "private prompt is never persisted".into(),
      },
      &RegisteredProject {
        project_id: "project".into(),
        group_key: "group".into(),
        folder_path: folder.canonicalize().unwrap().to_string_lossy().into(),
        name: "Project".into(),
        created_at_ms: 0,
        available: true,
      },
      "a".repeat(64),
    )
  }

  #[test]
  fn durable_admission_is_idempotent_and_never_persists_prompt_text() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state/managed.json");
    let (store, _) = ReceiptStore::new(Some(path.clone())).unwrap();
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
    let (store, _) = ReceiptStore::new(Some(path.clone())).unwrap();
    let mut original = receipt(directory.path());
    original.status = ManagedSessionStatus::Running;
    store.admit(original.clone()).unwrap();
    let lease = store.lease(&original.request_id).unwrap();
    assert!(store.lease(&original.request_id).is_err());
    let (_, concurrent) = ReceiptStore::new(Some(path.clone())).unwrap();
    assert_eq!(concurrent[0].revision, original.revision);
    assert!(concurrent[0].message.as_deref().unwrap().contains("another viewer"));
    let disk: Saved = tokn_session_storage::read_private_json(&path, MAX_STATE_BYTES)
      .unwrap()
      .unwrap();
    assert_eq!(disk.receipts[0].status, ManagedSessionStatus::Running);
    drop(lease);
    let (_, restarted) = ReceiptStore::new(Some(path)).unwrap();
    assert_eq!(restarted[0].status, ManagedSessionStatus::Stopped);
    assert_eq!(restarted[0].revision, original.revision + 1);
    assert!(store.lease(&original.request_id).is_ok());
  }

  #[test]
  fn header_identity_requires_complete_matching_native_header() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().canonicalize().unwrap();
    let path = directory.path().join("native.jsonl");
    assert!(validate_session(&path, "native-id", &folder).unwrap().is_none());
    std::fs::write(&path, "{\"type\":\"session\"").unwrap();
    assert!(validate_session(&path, "native-id", &folder).unwrap().is_none());
    let header = serde_json::json!({"type":"session","id":"native-id","cwd":folder});
    std::fs::write(&path, format!("{header}\n")).unwrap();
    let (_, key) = validate_session(&path, "native-id", &folder).unwrap().unwrap();
    let locator = crate::model::decode_session_key(&key).unwrap();
    assert_eq!(locator.session_id, "native-id");
    assert_eq!(locator.source_path, path.canonicalize().unwrap());
    assert!(validate_session(&path, "wrong-id", &folder).is_err());
    let other = tempfile::tempdir().unwrap();
    assert!(validate_session(&path, "native-id", &other.path().canonicalize().unwrap()).is_err());
    std::fs::write(&path, format!("{}\n", "x".repeat(65537))).unwrap();
    assert!(
      validate_session(&path, "native-id", &folder)
        .unwrap_err()
        .contains("size limit")
    );
  }

  #[test]
  fn stale_metadata_cannot_overwrite_a_later_revision() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("managed.json");
    let (store, _) = ReceiptStore::new(Some(path.clone())).unwrap();
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
  fn short_receipt_lock_contention_does_not_fail_an_independent_actor() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("managed.json");
    let (store, _) = ReceiptStore::new(Some(path.clone())).unwrap();
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
