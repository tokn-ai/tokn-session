use super::*;
use std::{fs::File, path::Path};

const MAX_RECEIPTS: usize = 256;
const MAX_STATE_BYTES: u64 = 1024 * 1024;

#[derive(Default, Deserialize, Serialize)]
struct Saved {
  version: u8,
  receipts: Vec<CreationSnapshot>,
}

pub(super) struct RequestLease(File);
impl Drop for RequestLease {
  fn drop(&mut self) {
    let _ = self.0.unlock();
  }
}

#[derive(Clone)]
pub(super) struct Store {
  path: PathBuf,
  _temporary: Option<Arc<tempfile::TempDir>>,
}

impl Store {
  pub fn new(path: Option<PathBuf>) -> Result<(Self, Vec<CreationSnapshot>), String> {
    let temporary = if path.is_none() {
      Some(Arc::new(tempfile::tempdir().map_err(|error| error.to_string())?))
    } else {
      None
    };
    let path = path.unwrap_or_else(|| temporary.as_ref().unwrap().path().join("session-creations.json"));
    let store = Self {
      path,
      _temporary: temporary,
    };
    let saved = store.read()?;
    Ok((store, saved.receipts))
  }

  fn read(&self) -> Result<Saved, String> {
    let saved = tokn_session_storage::read_private_json::<Saved>(&self.path, MAX_STATE_BYTES)?.unwrap_or_default();
    validate(&saved)?;
    Ok(saved)
  }

  pub fn latest(&self, request_id: &str) -> Result<Option<CreationSnapshot>, String> {
    Ok(
      self
        .read()?
        .receipts
        .into_iter()
        .find(|receipt| receipt.request_id == request_id),
    )
  }

  pub fn lease(&self, request_id: &str) -> Result<RequestLease, String> {
    validate_request_id(request_id)?;
    let parent = self
      .path
      .parent()
      .ok_or("Creation receipt path needs a parent directory")?;
    tokn_session_storage::acquire_private_lock(&parent.join(format!("session-create-{request_id}.lease")))
      .map(RequestLease)
  }

  pub fn admit(&self, proposed: CreationSnapshot) -> Result<(CreationSnapshot, Option<RequestLease>), String> {
    validate_receipt(&proposed)?;
    if let Some(existing) = self.latest(&proposed.request_id)? {
      return Ok((existing, None));
    }
    let lease = match self.lease(&proposed.request_id) {
      Ok(lease) => lease,
      Err(error) => {
        return match self.latest(&proposed.request_id)? {
          Some(existing) => Ok((existing, None)),
          None => Err(error),
        };
      }
    };
    let mut inserted = false;
    let saved = update_saved(&self.path, |saved| {
      let mut saved = saved.unwrap_or_default();
      validate(&saved)?;
      if !saved
        .receipts
        .iter()
        .any(|receipt| receipt.request_id == proposed.request_id)
      {
        if saved.receipts.len() >= MAX_RECEIPTS {
          return Err("Session creation receipt limit reached".into());
        }
        saved.receipts.push(proposed.clone());
        inserted = true;
      }
      saved.version = 1;
      Ok(saved)
    })?;
    let receipt = saved
      .receipts
      .into_iter()
      .find(|receipt| receipt.request_id == proposed.request_id)
      .ok_or("Creation receipt was not saved")?;
    Ok((receipt, inserted.then_some(lease)))
  }

  pub fn save(&self, receipt: CreationSnapshot) -> Result<(), String> {
    validate_receipt(&receipt)?;
    update_saved(&self.path, |saved| {
      let mut saved = saved.ok_or("Creation admission is unavailable")?;
      validate(&saved)?;
      let current = saved
        .receipts
        .iter_mut()
        .find(|existing| existing.request_id == receipt.request_id)
        .ok_or("Creation admission is unavailable")?;
      if current.fingerprint != receipt.fingerprint
        || current.provider != receipt.provider
        || current.project_id != receipt.project_id
        || current.working_directory != receipt.working_directory
        || current.runtime != receipt.runtime
        || current.runtime_plan != receipt.runtime_plan
        || current.runtime_plan_id != receipt.runtime_plan_id
        || current.model != receipt.model
        || current.planned_native_id != receipt.planned_native_id
      {
        return Err("Immutable creation intent changed".into());
      }
      if receipt.revision <= current.revision {
        return Err("Creation receipt changed in another host".into());
      }
      *current = receipt.clone();
      Ok(saved)
    })
    .map(|_| ())
  }
}

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

fn validate(saved: &Saved) -> Result<(), String> {
  if saved.version > 1 || saved.version == 0 && !saved.receipts.is_empty() || saved.receipts.len() > MAX_RECEIPTS {
    return Err("Unsupported session creation receipt catalog".into());
  }
  let mut ids = std::collections::HashSet::new();
  for receipt in &saved.receipts {
    if !ids.insert(&receipt.request_id) {
      return Err("Duplicate session creation receipt".into());
    }
    validate_receipt(receipt)?;
  }
  Ok(())
}

fn validate_receipt(receipt: &CreationSnapshot) -> Result<(), String> {
  validate_request_id(&receipt.request_id)?;
  if receipt.project_id.is_empty()
    || receipt.project_id.len() > 128
    || !receipt.working_directory.is_absolute()
    || receipt.fingerprint.len() != 64
    || !receipt.fingerprint.chars().all(|ch| ch.is_ascii_hexdigit())
    || receipt.revision == 0
    || receipt
      .session_key
      .as_ref()
      .is_some_and(|key| key.is_empty() || key.len() > 8192)
    || receipt
      .planned_native_id
      .as_ref()
      .is_some_and(|id| !valid_native_id(id))
  {
    return Err("Invalid session creation receipt".into());
  }
  if let Some(selection) = &receipt.runtime {
    validate_profile_id(&selection.profile_id)?;
  }
  if let Some(plan) = &receipt.runtime_plan {
    let selection = plan.shared_runtime_selection()?;
    if plan.provider != receipt.provider
      || plan.working_directory != receipt.working_directory
      || receipt.runtime.as_ref().is_some_and(|runtime| runtime != &selection)
      || receipt
        .runtime_plan_id
        .as_ref()
        .is_some_and(|id| plan.fingerprint().as_ref() != Ok(id))
      || receipt
        .created
        .as_ref()
        .is_some_and(|created| created.profile_id != selection.profile_id)
    {
      return Err("Creation receipt runtime plan is inconsistent".into());
    }
  } else if receipt.runtime_plan_id.is_some() {
    return Err("Creation receipt runtime plan is missing".into());
  }
  if let Some(model) = &receipt.model {
    crate::runtime::validate_creation_model(model).map_err(|error| error.to_string())?;
    if model.display_name.is_some() {
      return Err("Creation receipts store model identity without display labels".into());
    }
  }
  if let Some(created) = &receipt.created {
    if created.provider != receipt.provider
      || created.native.working_directory != receipt.working_directory
      || receipt
        .runtime
        .as_ref()
        .is_some_and(|selection| selection.profile_id != created.profile_id)
      || !valid_native_id(&created.native.native_id)
      || created.target.runtime_id.is_empty()
      || created.target.runtime_id.len() > 512
      || created.target.instance_id.is_empty()
      || created.target.instance_id.len() > 512
      || created.target.session_id.as_deref() != Some(created.native.native_id.as_str())
      || created
        .native
        .history_path
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
      || receipt
        .planned_native_id
        .as_ref()
        .is_some_and(|id| id != &created.native.native_id)
    {
      return Err("Created native session identity is inconsistent".into());
    }
    validate_profile_id(&created.profile_id)?;
  }
  if matches!(
    receipt.phase,
    CreationPhase::NativeKnown
      | CreationPhase::FirstInputUncertain
      | CreationPhase::HistoryPending
      | CreationPhase::Ready
  ) && receipt.created.is_none()
    || receipt.first_input != FirstInputState::NotAttempted && receipt.created.is_none()
    || receipt.phase == CreationPhase::Ready && receipt.session_key.is_none()
  {
    return Err("Creation progress has no matching native identity".into());
  }
  Ok(())
}

fn valid_native_id(id: &str) -> bool {
  !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}
