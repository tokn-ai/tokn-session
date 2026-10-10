//! Endpoint-owned configuration and pairing authorization. Nothing here is supplied by the Hub.
use crate::{pairing::TotpSecret, secure::decode_public_key};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
  collections::BTreeMap,
  fs::{self, File, OpenOptions},
  io::{Read, Write},
  path::{Path, PathBuf},
};
use url::Url;
use uuid::Uuid;
use webauthn_rs::prelude::{AuthenticationResult, Passkey};
use zeroize::{Zeroize, Zeroizing};

const MAX_STATE: u64 = 256 * 1024;
const MAX_DEVICES: usize = 64;
const ATTEMPT_WINDOW: u64 = 300;
const MAX_ATTEMPTS: u32 = 5;
const MAX_PASSKEYS: usize = 32;
const MAX_PASSKEY_ATTEMPTS: u32 = 20;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProfile {
  pub version: u8,
  pub host_id: String,
  pub hub_url: String,
  pub name: String,
  pub viewer_url: String,
  pub allow_control: bool,
  pub insecure_loopback: bool,
  /// Stable browser UI origin. Older profiles derive it from the Hub URL.
  #[serde(default)]
  pub passkey_origin: Option<String>,
}

impl HostProfile {
  pub fn load(path: &Path) -> Result<Option<Self>, String> {
    let value: Option<Self> = read_optional(path)?;
    if let Some(value) = &value {
      validate_uuid(&value.host_id)?;
      if value.version != 1 {
        return Err("Unsupported host configuration version".into());
      }
    }
    Ok(value)
  }

  pub fn save(&self, path: &Path) -> Result<(), String> {
    validate_uuid(&self.host_id)?;
    let _lock = lock(path)?;
    if let Some(previous) = Self::load(path)? {
      if previous.host_id != self.host_id {
        return Err("Cannot replace the saved host identity".into());
      }
    }
    write_atomic(path, self)
  }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostAccess {
  version: u8,
  totp_secret: String,
  devices: BTreeMap<String, u64>,
  last_used_step: Option<u64>,
  attempt_window_start: u64,
  attempts: u32,
  #[serde(default)]
  passkey_origin: Option<String>,
  #[serde(default)]
  passkeys: Vec<Passkey>,
  #[serde(default)]
  passkey_attempt_window_start: u64,
  #[serde(default)]
  passkey_attempts: u32,
}

impl Drop for HostAccess {
  fn drop(&mut self) {
    self.totp_secret.zeroize();
  }
}

fn host_access(path: &Path) -> Result<HostAccess, String> {
  let state: HostAccess = read_required(path)?;
  if state.version != 1 || state.devices.len() > MAX_DEVICES || state.passkeys.len() > MAX_PASSKEYS {
    return Err("Invalid host pairing state".into());
  }
  TotpSecret::from_base32(&state.totp_secret)?;
  for key in state.devices.keys() {
    decode_public_key(key)?;
  }
  if !state.passkeys.is_empty() && state.passkey_origin.is_none() {
    return Err("Registered host passkeys have no saved browser origin".into());
  }
  let mut credentials = std::collections::HashSet::new();
  for passkey in &state.passkeys {
    if !credentials.insert(passkey.cred_id()) {
      return Err("Duplicate registered host passkey".into());
    }
  }
  Ok(state)
}

/// Setup only; importing a seed never overwrites existing trust or replay state.
pub fn initialize_host_access(path: &Path, secret: &TotpSecret) -> Result<(), String> {
  let _lock = lock(path)?;
  if read_optional::<HostAccess>(path)?.is_some() {
    return Err("Authenticator is already configured; existing pairing state was preserved".into());
  }
  write_atomic(
    path,
    &HostAccess {
      version: 1,
      totp_secret: secret.to_base32(),
      devices: BTreeMap::new(),
      last_used_step: None,
      attempt_window_start: 0,
      attempts: 0,
      passkey_origin: None,
      passkeys: Vec::new(),
      passkey_attempt_window_start: 0,
      passkey_attempts: 0,
    },
  )
}

pub fn read_totp_secret(path: &Path) -> Result<TotpSecret, String> {
  TotpSecret::from_base32(&host_access(path)?.totp_secret)
}

fn validate_step(state: &HostAccess, now: u64, step: u64) -> Result<(), String> {
  if (now / 30).abs_diff(step) > 1 {
    return Err("Pairing code expired; check device clocks and use a fresh code".into());
  }
  if state.last_used_step.is_some_and(|used| step <= used) {
    return Err("This authenticator time step was already used; wait for a fresh code".into());
  }
  Ok(())
}

/// Reserve an attempt on disk before doing PAKE work. Reconnecting, changing
/// client keys, or restarting the host never resets this host-wide budget.
pub fn begin_pairing(path: &Path, now: u64, step: u64) -> Result<(), String> {
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  validate_step(&state, now, step)?;
  if now < state.attempt_window_start {
    return Err("Host clock moved backwards; pairing is temporarily unavailable".into());
  }
  if now.saturating_sub(state.attempt_window_start) >= ATTEMPT_WINDOW {
    state.attempt_window_start = now;
    state.attempts = 0;
  }
  if state.attempts >= MAX_ATTEMPTS {
    return Err("Too many pairing attempts; try again in five minutes".into());
  }
  state.attempts += 1;
  write_atomic(path, &state)
}

/// Called only after PAKE key confirmation. Consume the time step and record
/// the verified client atomically before sending the authenticated success ack.
pub fn authorize_device(path: &Path, client_public_key: &str, step: u64, now: u64) -> Result<(), String> {
  decode_public_key(client_public_key)?;
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  validate_step(&state, now, step)?;
  if !state.devices.contains_key(client_public_key) && state.devices.len() >= MAX_DEVICES {
    return Err("Host paired-device limit reached; remove an unused device first".into());
  }
  state.devices.insert(client_public_key.into(), now);
  state.last_used_step = Some(step);
  write_atomic(path, &state)
}

pub fn is_authorized(path: &Path, public_key: &str) -> Result<bool, String> {
  Ok(host_access(path)?.devices.contains_key(public_key))
}

pub fn devices(path: &Path) -> Result<BTreeMap<String, u64>, String> {
  Ok(host_access(path)?.devices.clone())
}

pub fn remove_device(path: &Path, public_key: &str) -> Result<(), String> {
  decode_public_key(public_key)?;
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  if state.devices.remove(public_key).is_none() {
    return Err("Device is not paired with this host".into());
  }
  write_atomic(path, &state)
}

/// WebAuthn challenges are high-entropy, but unauthenticated starts still have
/// a bounded, persisted host-wide budget across channels and restarts.
pub(crate) fn begin_passkey(path: &Path, now: u64) -> Result<(), String> {
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  if now < state.passkey_attempt_window_start {
    return Err("Host clock moved backwards; passkey authentication is temporarily unavailable".into());
  }
  if now.saturating_sub(state.passkey_attempt_window_start) >= ATTEMPT_WINDOW {
    state.passkey_attempt_window_start = now;
    state.passkey_attempts = 0;
  }
  if state.passkey_attempts >= MAX_PASSKEY_ATTEMPTS {
    return Err("Too many passkey attempts; try again in five minutes".into());
  }
  state.passkey_attempts += 1;
  write_atomic(path, &state)
}

pub(crate) fn host_passkeys(path: &Path, origin: &str) -> Result<Vec<Passkey>, String> {
  let state = host_access(path)?;
  check_passkey_origin(&state, origin)?;
  Ok(state.passkeys.clone())
}

/// Reject a configuration change before writing host.json if existing
/// credentials would move to another RP or become unavailable.
pub fn validate_host_passkey_origin(path: &Path, origin: Option<&str>) -> Result<(), String> {
  let state = host_access(path)?;
  if state
    .passkey_origin
    .as_deref()
    .is_some_and(|saved| Some(saved) != origin)
  {
    return Err("Host passkey origin changed; restore the configured browser origin".into());
  }
  Ok(())
}

fn check_passkey_origin(state: &HostAccess, origin: &str) -> Result<(), String> {
  if state.passkey_origin.as_deref().is_some_and(|saved| saved != origin) {
    return Err("Host passkey origin changed; restore the configured browser origin".into());
  }
  Ok(())
}

/// Device authorization must remain valid at completion, not just when the
/// enrollment starts. The saved credential and its RP origin are host-owned.
pub(crate) fn register_host_passkey(
  path: &Path,
  origin: &str,
  device_public_key: &str,
  passkey: Passkey,
) -> Result<(), String> {
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  check_passkey_origin(&state, origin)?;
  if !state.devices.contains_key(device_public_key) {
    return Err("Device authorization was revoked during passkey enrollment".into());
  }
  if state.passkeys.len() >= MAX_PASSKEYS {
    return Err("Host passkey limit reached".into());
  }
  if state.passkeys.iter().any(|saved| saved.cred_id() == passkey.cred_id()) {
    return Err("This passkey is already registered with this host".into());
  }
  state.passkey_origin = Some(origin.into());
  state.passkeys.push(passkey);
  write_atomic(path, &state)
}

/// Authentication and the new device authorization persist atomically. A
/// removed credential or changed origin cannot finish an old ceremony.
pub(crate) fn authorize_passkey_device(
  path: &Path,
  origin: &str,
  device_public_key: &str,
  authentication: &AuthenticationResult,
  now: u64,
) -> Result<(), String> {
  decode_public_key(device_public_key)?;
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  check_passkey_origin(&state, origin)?;
  let passkey = state
    .passkeys
    .iter_mut()
    .find(|passkey| passkey.cred_id() == authentication.cred_id())
    .ok_or("Passkey is no longer registered with this host")?;
  passkey.update_credential(authentication);
  if !state.devices.contains_key(device_public_key) && state.devices.len() >= MAX_DEVICES {
    return Err("Host paired-device limit reached; remove an unused device first".into());
  }
  state.devices.insert(device_public_key.into(), now);
  write_atomic(path, &state)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SavedHost {
  pub host_id: String,
  pub host_public_key: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub machine_address: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HubHosts {
  hosts: Vec<SavedHost>,
  selected_host: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientHosts {
  version: u8,
  client_id: String,
  hubs: BTreeMap<String, HubHosts>,
}

#[derive(Clone)]
pub struct ClientStore {
  state_dir: PathBuf,
  hub: String,
}

impl ClientStore {
  pub fn load_or_create(state_dir: &Path, hub: &Url) -> Result<Self, String> {
    let store = Self {
      state_dir: state_dir.into(),
      hub: canonical_hub(hub)?,
    };
    let path = store.path();
    let _lock = lock(&path)?;
    match read_optional::<ClientHosts>(&path)? {
      Some(state) => {
        validate_client_state(&state)?;
        if !store.identity_file().is_file() {
          return Err("Saved client identity is missing; restore its key before reconnecting".into());
        }
      }
      None => {
        if store.identity_file().try_exists().map_err(|e| e.to_string())? {
          return Err(
            "Saved host pins are missing for an existing client identity; restore client-hosts.json before pairing"
              .into(),
          );
        }
        crate::secure::NoiseIdentity::load_or_create(&store.identity_file())?;
        write_atomic(
          &path,
          &ClientHosts {
            version: 1,
            client_id: Uuid::new_v4().to_string(),
            hubs: BTreeMap::new(),
          },
        )?;
      }
    }
    Ok(store)
  }

  fn path(&self) -> PathBuf {
    self.state_dir.join("client-hosts.json")
  }

  pub fn identity_file(&self) -> PathBuf {
    self.state_dir.join("client-noise.key")
  }

  fn read(&self) -> Result<ClientHosts, String> {
    let state = read_required(&self.path())?;
    validate_client_state(&state)?;
    Ok(state)
  }

  pub fn hosts(&self) -> Result<Vec<SavedHost>, String> {
    Ok(
      self
        .read()?
        .hubs
        .get(&self.hub)
        .map(|hub| hub.hosts.clone())
        .unwrap_or_default(),
    )
  }

  pub fn save_host(&self, host: SavedHost) -> Result<(), String> {
    validate_uuid(&host.host_id)?;
    decode_public_key(&host.host_public_key)?;
    validate_host_metadata(&host)?;
    let path = self.path();
    let _lock = lock(&path)?;
    let mut state = self.read()?;
    if !state.hubs.contains_key(&self.hub) && state.hubs.len() >= 32 {
      return Err("Saved Hub limit reached".into());
    }
    let hub = state.hubs.entry(self.hub.clone()).or_default();
    if let Some(existing) = hub.hosts.iter().find(|value| value.host_id == host.host_id) {
      if existing.host_public_key != host.host_public_key {
        return Err(
          "Host identity changed; the saved key was preserved. Restore the host key before reconnecting".into(),
        );
      }
      return Ok(());
    }
    if hub.hosts.len() >= 64 {
      return Err("Saved host limit reached".into());
    }
    hub.hosts.push(host);
    validate_client_state(&state)?;
    write_atomic(&path, &state)
  }

  /// Remember display metadata for an already-pinned host. The caller verifies
  /// the directory mapping before entering this atomic local transaction.
  pub fn remember_metadata(&self, host_id: &str, machine_address: &str, name: &str) -> Result<SavedHost, String> {
    validate_uuid(host_id)?;
    tokn_hub_client_core::address::parse_machine_address(machine_address)?;
    tokn_hub_client_core::address::validate_machine_name(name)?;
    let path = self.path();
    let _lock = lock(&path)?;
    let mut state = self.read()?;
    let hub = state
      .hubs
      .get_mut(&self.hub)
      .ok_or("Pair this machine before remembering its address")?;
    if hub
      .hosts
      .iter()
      .any(|host| host.host_id != host_id && host.machine_address.as_deref() == Some(machine_address))
    {
      return Err("This saved machine address belongs to a different UUID; its original identity was preserved".into());
    }
    let host = hub
      .hosts
      .iter_mut()
      .find(|host| host.host_id == host_id)
      .ok_or("Pair this machine before remembering its address")?;
    if host
      .machine_address
      .as_deref()
      .is_some_and(|address| address != machine_address)
    {
      return Err("This host already has a different saved machine address".into());
    }
    host.machine_address = Some(machine_address.into());
    host.name = Some(name.into());
    let remembered = host.clone();
    validate_client_state(&state)?;
    write_atomic(&path, &state)?;
    Ok(remembered)
  }

  pub fn selected_host(&self) -> Result<Option<String>, String> {
    Ok(
      self
        .read()?
        .hubs
        .get(&self.hub)
        .and_then(|hub| hub.selected_host.clone()),
    )
  }

  /// Forget a local pin without replacing this client's identity or revoking
  /// its device key on the remote host.
  pub fn forget_host(&self, host_id: &str) -> Result<(), String> {
    let path = self.path();
    let _lock = lock(&path)?;
    let mut state = self.read()?;
    let hub = state
      .hubs
      .get_mut(&self.hub)
      .ok_or("No hosts have been paired with this Hub")?;
    if !hub.hosts.iter().any(|host| host.host_id == host_id) {
      return Err("This host has no saved identity".into());
    }
    hub.hosts.retain(|host| host.host_id != host_id);
    if hub.selected_host.as_deref() == Some(host_id) {
      hub.selected_host = None;
    }
    write_atomic(&path, &state)
  }

  pub fn select_host(&self, host_id: &str) -> Result<(), String> {
    let path = self.path();
    let _lock = lock(&path)?;
    let mut state = self.read()?;
    let hub = state
      .hubs
      .get_mut(&self.hub)
      .ok_or("No hosts have been paired with this Hub")?;
    if !hub.hosts.iter().any(|host| host.host_id == host_id) {
      return Err("Pair this host before selecting it".into());
    }
    hub.selected_host = Some(host_id.into());
    write_atomic(&path, &state)
  }
}

fn validate_client_state(state: &ClientHosts) -> Result<(), String> {
  if state.version != 1 || state.hubs.len() > 32 {
    return Err("Invalid saved client configuration".into());
  }
  validate_uuid(&state.client_id)?;
  for hub in state.hubs.values() {
    if hub.hosts.len() > 64 {
      return Err("Too many saved hosts".into());
    }
    let mut ids = std::collections::HashSet::new();
    let mut addresses = std::collections::HashSet::new();
    for host in &hub.hosts {
      validate_uuid(&host.host_id)?;
      decode_public_key(&host.host_public_key)?;
      validate_host_metadata(host)?;
      if host
        .machine_address
        .as_ref()
        .is_some_and(|address| !addresses.insert(address))
      {
        return Err("Duplicate saved machine address".into());
      }
      if !ids.insert(&host.host_id) {
        return Err("Duplicate saved host identity".into());
      }
    }
    if hub
      .selected_host
      .as_ref()
      .is_some_and(|selected| !ids.contains(selected))
    {
      return Err("Selected host has no saved identity".into());
    }
  }
  Ok(())
}

fn validate_host_metadata(host: &SavedHost) -> Result<(), String> {
  if let Some(address) = &host.machine_address {
    tokn_hub_client_core::address::parse_machine_address(address)?;
  }
  if let Some(name) = &host.name {
    tokn_hub_client_core::address::validate_machine_name(name)?;
  }
  Ok(())
}

pub fn validate_uuid(value: &str) -> Result<(), String> {
  let uuid = Uuid::parse_str(value).map_err(|_| "Host ID must be a UUID")?;
  if uuid.is_nil() || uuid.to_string() != value {
    return Err("Host ID must be a canonical nonzero UUID".into());
  }
  Ok(())
}

pub fn canonical_hub(hub: &Url) -> Result<String, String> {
  if !hub.username().is_empty()
    || hub.password().is_some()
    || hub.query().is_some()
    || hub.fragment().is_some()
    || hub.path() != "/"
    || hub.host_str().is_none()
  {
    return Err("Hub URL must be an origin without credentials, paths, queries, or fragments".into());
  }
  let mut result = hub.clone();
  match hub.scheme() {
    "https" | "wss" => result.set_scheme("https").map_err(|_| "Invalid Hub origin")?,
    "http" | "ws" => result.set_scheme("http").map_err(|_| "Invalid Hub origin")?,
    _ => return Err("Hub URL must use HTTPS (or explicit loopback HTTP for development)".into()),
  }
  Ok(result.to_string())
}

pub fn read_optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
  reject_symlink(path)?;
  let mut options = OpenOptions::new();
  options.read(true);
  nofollow(&mut options);
  let mut file = match options.open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(format!("Could not open private state {}: {error}", path.display())),
  };
  validate_private(&file)?;
  let mut bytes = Zeroizing::new(Vec::new());
  (&mut file)
    .take(MAX_STATE + 1)
    .read_to_end(&mut bytes)
    .map_err(|e| e.to_string())?;
  if bytes.len() as u64 > MAX_STATE {
    return Err("Private state exceeds its size limit".into());
  }
  serde_json::from_slice(&bytes).map(Some).map_err(|_| {
    format!(
      "Invalid private state {}; restore it instead of replacing it",
      path.display()
    )
  })
}

fn read_required<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
  read_optional(path)?.ok_or_else(|| format!("Required private state is missing: {}", path.display()))
}

fn nofollow(options: &mut OpenOptions) {
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
}

fn reject_symlink(path: &Path) -> Result<(), String> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() => Err("Private state cannot be a symlink".into()),
    Ok(_) => Ok(()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(format!("Could not inspect private state: {error}")),
  }
}

fn validate_private(file: &File) -> Result<(), String> {
  let metadata = file.metadata().map_err(|e| e.to_string())?;
  if !metadata.is_file() || metadata.len() > MAX_STATE {
    return Err("Private state must be a bounded regular file".into());
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid does not access memory or mutate process state.
    if metadata.mode() & 0o777 != 0o600 || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
      return Err("Private state must be owned by the current user, mode 0600, with no hard links".into());
    }
  }
  Ok(())
}

fn lock(path: &Path) -> Result<File, String> {
  if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
  }
  let mut name = path.as_os_str().to_owned();
  name.push(".lock");
  let lock_path = PathBuf::from(name);
  reject_symlink(&lock_path)?;
  let mut options = OpenOptions::new();
  options.read(true).write(true).create(true).truncate(false);
  nofollow(&mut options);
  let file = options
    .open(lock_path)
    .map_err(|e| format!("Could not lock private state: {e}"))?;
  validate_private(&file)?;
  file
    .try_lock()
    .map_err(|_| "Private state is being updated by another process or cannot be locked; retry shortly")?;
  Ok(file)
}

fn write_atomic(path: &Path, value: &impl Serialize) -> Result<(), String> {
  let bytes = Zeroizing::new(serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?);
  if bytes.len() as u64 > MAX_STATE {
    return Err("Private state exceeds its size limit".into());
  }
  let mut name = path.as_os_str().to_owned();
  name.push(format!(".{}.tmp", Uuid::new_v4()));
  let temp = PathBuf::from(name);
  let result = (|| {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    nofollow(&mut options);
    let mut file = options.open(&temp).map_err(|e| e.to_string())?;
    file
      .write_all(&bytes)
      .and_then(|_| file.sync_all())
      .map_err(|e| e.to_string())?;
    fs::rename(&temp, path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
      File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())?;
    }
    Ok(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(temp);
  }
  result
}

pub fn save_configuration(path: &Path, value: &impl Serialize) -> Result<(), String> {
  let _lock = lock(path)?;
  // Existing malformed, linked or exposed configuration is never replaced silently.
  let _ = read_optional::<serde_json::Value>(path)?;
  write_atomic(path, value)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::secure::NoiseIdentity;

  #[test]
  fn attempts_and_used_steps_survive_reopening_and_device_revocation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host-access.json");
    initialize_host_access(&path, &TotpSecret::generate()).unwrap();
    for _ in 0..MAX_ATTEMPTS {
      begin_pairing(&path, 900, 30).unwrap();
    }
    assert!(begin_pairing(&path, 901, 30).is_err());
    begin_pairing(&path, 1200, 40).unwrap();
    let key = NoiseIdentity::generate().unwrap().public_key();
    authorize_device(&path, &key, 40, 1200).unwrap();
    assert!(is_authorized(&path, &key).unwrap());
    assert!(authorize_device(&path, &key, 40, 1201).is_err());
    remove_device(&path, &key).unwrap();
    assert!(!is_authorized(&path, &key).unwrap());
    assert!(begin_pairing(&path, 1201, 40).is_err());
    assert!(initialize_host_access(&path, &TotpSecret::generate()).is_err());
  }

  #[test]
  fn passkey_attempt_budget_persists_without_consuming_authenticator_attempts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host-access.json");
    initialize_host_access(&path, &TotpSecret::generate()).unwrap();
    for _ in 0..MAX_PASSKEY_ATTEMPTS {
      begin_passkey(&path, 900).unwrap();
    }
    assert!(begin_passkey(&path, 901).is_err());
    assert!(begin_passkey(&path, 899).is_err());
    begin_pairing(&path, 901, 30).unwrap();
    begin_passkey(&path, 1200).unwrap();
  }

  #[test]
  fn existing_authenticator_state_without_passkey_fields_remains_valid() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host-access.json");
    let secret = TotpSecret::generate();
    initialize_host_access(&path, &secret).unwrap();
    let mut state: serde_json::Value = read_required(&path).unwrap();
    for field in [
      "passkey_origin",
      "passkeys",
      "passkey_attempt_window_start",
      "passkey_attempts",
    ] {
      state.as_object_mut().unwrap().remove(field);
    }
    write_atomic(&path, &state).unwrap();
    assert_eq!(read_totp_secret(&path).unwrap().to_base32(), secret.to_base32());
    assert!(host_passkeys(&path, "https://hub.example.com").unwrap().is_empty());
    begin_passkey(&path, 900).unwrap();
    begin_pairing(&path, 900, 30).unwrap();
  }

  #[test]
  fn client_pins_are_per_hub_and_cannot_be_silently_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let hub = Url::parse("https://hub.example").unwrap();
    let store = ClientStore::load_or_create(directory.path(), &hub).unwrap();
    let mut host = SavedHost {
      host_id: Uuid::new_v4().to_string(),
      host_public_key: NoiseIdentity::generate().unwrap().public_key(),
      machine_address: None,
      name: None,
    };
    store.save_host(host.clone()).unwrap();
    store.select_host(&host.host_id).unwrap();
    let reopened = ClientStore::load_or_create(directory.path(), &hub).unwrap();
    assert_eq!(reopened.selected_host().unwrap(), Some(host.host_id.clone()));
    host.host_public_key = NoiseIdentity::generate().unwrap().public_key();
    assert!(reopened.save_host(host.clone()).is_err());
    let other = ClientStore::load_or_create(directory.path(), &Url::parse("https://other.example").unwrap()).unwrap();
    assert!(other.hosts().unwrap().is_empty());
    other.save_host(host).unwrap();
    fs::remove_file(store.path()).unwrap();
    assert!(ClientStore::load_or_create(directory.path(), &hub).is_err());
    fs::remove_file(store.identity_file()).unwrap();
    // A separately intact catalog also fails closed if only its key disappears.
    let second_dir = tempfile::tempdir().unwrap();
    let second = ClientStore::load_or_create(second_dir.path(), &hub).unwrap();
    fs::remove_file(second.identity_file()).unwrap();
    assert!(ClientStore::load_or_create(second_dir.path(), &hub).is_err());
  }

  #[test]
  fn client_metadata_preserves_legacy_pins_selection_and_alias_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let hub = Url::parse("https://hub.example").unwrap();
    let store = ClientStore::load_or_create(directory.path(), &hub).unwrap();
    let host: SavedHost = serde_json::from_value(serde_json::json!({
      "host_id": Uuid::new_v4().to_string(), "host_public_key": NoiseIdentity::generate().unwrap().public_key(),
    }))
    .unwrap();
    assert_eq!(host.machine_address, None);
    assert_eq!(host.name, None);
    store.save_host(host.clone()).unwrap();
    store.select_host(&host.host_id).unwrap();
    let remembered = store
      .remember_metadata(&host.host_id, "alice:workstation", "Workstation")
      .unwrap();
    assert_eq!(remembered.host_public_key, host.host_public_key);
    assert_eq!(remembered.machine_address.as_deref(), Some("alice:workstation"));
    // The same UUID/key with no metadata is an ordinary authenticated reconnect,
    // not an identity change and not a request to erase remembered labels.
    store.save_host(host.clone()).unwrap();
    let reopened = ClientStore::load_or_create(directory.path(), &hub).unwrap();
    assert_eq!(reopened.hosts().unwrap(), vec![remembered.clone()]);
    assert_eq!(reopened.selected_host().unwrap(), Some(host.host_id.clone()));
    let renamed = reopened
      .remember_metadata(&host.host_id, "alice:workstation", "Workstation display")
      .unwrap();
    assert_eq!(renamed.name.as_deref(), Some("Workstation display"));
    assert!(
      reopened
        .remember_metadata(&host.host_id, "alice:other", "Workstation")
        .is_err()
    );
    let second = SavedHost {
      host_id: Uuid::new_v4().to_string(),
      host_public_key: NoiseIdentity::generate().unwrap().public_key(),
      machine_address: None,
      name: None,
    };
    reopened.save_host(second.clone()).unwrap();
    assert!(
      reopened
        .remember_metadata(&second.host_id, "alice:workstation", "Another host")
        .is_err()
    );
    assert!(
      reopened
        .remember_metadata(&second.host_id, "Alice:second", "Another host")
        .is_err()
    );
    let other_hub =
      ClientStore::load_or_create(directory.path(), &Url::parse("https://other.example").unwrap()).unwrap();
    other_hub.save_host(second.clone()).unwrap();
    other_hub
      .remember_metadata(&second.host_id, "alice:workstation", "Other Hub")
      .unwrap();
    assert_eq!(reopened.hosts().unwrap()[0], renamed);
    assert_eq!(reopened.selected_host().unwrap(), Some(host.host_id));
  }

  #[cfg(unix)]
  #[test]
  fn damaged_missing_or_exposed_state_fails_closed() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("access.json");
    assert!(is_authorized(&path, "anything").is_err());
    initialize_host_access(&path, &TotpSecret::generate()).unwrap();
    let linked = directory.path().join("linked.json");
    symlink(&path, &linked).unwrap();
    assert!(read_totp_secret(&linked).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_totp_secret(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, "broken").unwrap();
    assert!(read_totp_secret(&path).is_err());
  }
}
