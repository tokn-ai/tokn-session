//! Endpoint-owned configuration and pairing authorization. Nothing here is supplied by the Hub.
use crate::{pairing::TotpSecret, secure::decode_public_key};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};
pub(crate) use tokn_hub_remote::persistence::{lock, lock_held};
use tokn_hub_remote::persistence::{read_required, write_atomic};
pub use tokn_hub_remote::{
  persistence::{read_optional, save_configuration},
  store::{ClientStore, SavedHost, canonical_hub, validate_uuid},
};
use webauthn_rs::prelude::{AuthenticationResult, Passkey};
use zeroize::Zeroize;

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
  /// Optional STUN services for direct WebRTC candidate discovery.
  #[serde(default)]
  pub ice_servers: Vec<String>,
}

impl HostProfile {
  pub fn load(path: &Path) -> Result<Option<Self>, String> {
    let value: Option<Self> = read_optional(path)?;
    if let Some(value) = &value {
      validate_uuid(&value.host_id)?;
      tokn_hub_transport::validate_stun_urls(&value.ice_servers)?;
      if value.version != 1 {
        return Err("Unsupported host configuration version".into());
      }
    }
    Ok(value)
  }

  pub fn save(&self, path: &Path) -> Result<(), String> {
    validate_uuid(&self.host_id)?;
    tokn_hub_transport::validate_stun_urls(&self.ice_servers)?;
    let _lock = lock(path)?;
    if let Some(previous) = Self::load(path)? {
      if previous.host_id != self.host_id {
        return Err("Cannot replace the saved host identity".into());
      }
    }
    write_atomic(path, self)
  }
}

/// Browser enrollment grants cannot read sessions. Browser login grants expire
/// and never become persistent app authorization.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeviceAuthorization {
  Native { paired_at: u64 },
  BrowserEnrollment { paired_at: u64, expires_at: u64 },
  BrowserSession { paired_at: u64, expires_at: u64 },
}
impl DeviceAuthorization {
  fn active(&self, now: u64, enrollment: bool) -> bool {
    match self {
      Self::Native { .. } => true,
      Self::BrowserEnrollment { paired_at, expires_at } => enrollment && now >= *paired_at && now < *expires_at,
      Self::BrowserSession { paired_at, expires_at } => now >= *paired_at && now < *expires_at,
    }
  }
}
const BROWSER_ENROLLMENT_SECONDS: u64 = 300;
const BROWSER_SESSION_SECONDS: u64 = 8 * 60 * 60;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostAccess {
  version: u8,
  totp_secret: String,
  devices: BTreeMap<String, DeviceAuthorization>,
  /// Local opt-in; pairing and agent input never imply process administration.
  #[serde(default, skip_serializing_if = "runtime_management_disabled")]
  runtime_management: bool,
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

fn runtime_management_disabled(enabled: &bool) -> bool {
  !enabled
}

impl Drop for HostAccess {
  fn drop(&mut self) {
    self.totp_secret.zeroize();
  }
}

fn host_access(path: &Path) -> Result<HostAccess, String> {
  let value: serde_json::Value = read_required(path)?;
  if value["version"] == 1 {
    return Err("Legacy pairing state requires `tokn-session-hub authenticator --upgrade-sha256`; rescan its QR and pair app devices again".into());
  }
  let state: HostAccess = serde_json::from_value(value).map_err(|_| "Invalid host pairing state")?;
  validate_host_access(&state)?;
  Ok(state)
}

fn validate_host_access(state: &HostAccess) -> Result<(), String> {
  if state.version != 2 || state.devices.len() > MAX_DEVICES || state.passkeys.len() > MAX_PASSKEYS {
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
  Ok(())
}

/// Read-only validation for moving a complete bundle. A valid legacy SHA1
/// record may move unchanged; upgrading its authenticator remains explicit.
pub fn validate_host_access_for_storage(path: &Path) -> Result<(), String> {
  let value: serde_json::Value = read_required(path)?;
  if value["version"] == 1 {
    legacy_host_access(value).map(|_| ())
  } else {
    host_access(path).map(|_| ())
  }
}

fn legacy_host_access(mut value: serde_json::Value) -> Result<HostAccess, String> {
  if value["version"] != 1 {
    return Err("Unsupported host pairing state".into());
  }
  TotpSecret::from_base32(value["totp_secret"].as_str().ok_or("Invalid legacy authenticator")?)?;
  let devices: BTreeMap<String, u64> =
    serde_json::from_value(value["devices"].clone()).map_err(|_| "Invalid legacy devices")?;
  if devices.len() > MAX_DEVICES {
    return Err("Invalid legacy devices".into());
  }
  for key in devices.keys() {
    decode_public_key(key)?;
  }
  // Decode the remaining fields with the current schema while keeping the
  // original seed, replay state, passkeys, and file entirely unchanged.
  value["version"] = serde_json::json!(2);
  value["devices"] = serde_json::json!({});
  let state: HostAccess = serde_json::from_value(value).map_err(|_| "Invalid legacy host pairing state")?;
  validate_host_access(&state)?;
  Ok(state)
}

/// Setup only; importing a seed never overwrites existing trust or replay state.
pub fn initialize_host_access(path: &Path, secret: &TotpSecret) -> Result<(), String> {
  let _lock = lock(path)?;
  if read_optional::<serde_json::Value>(path)?.is_some() {
    return Err("Authenticator is already configured; existing pairing state was preserved".into());
  }
  write_atomic(
    path,
    &HostAccess {
      version: 2,
      totp_secret: secret.to_base32(),
      devices: BTreeMap::new(),
      runtime_management: false,
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

/// Explicit local migration preserves host identities/passkeys, rotates the OTP
/// seed, and removes legacy device grants whose client type cannot be proven.
pub fn upgrade_authenticator(path: &Path) -> Result<(), String> {
  let _lock = lock(path)?;
  let value: serde_json::Value = read_required(path)?;
  if value["version"] == 2 {
    host_access(path)?;
    return Ok(());
  }
  let mut state = legacy_host_access(value)?;
  state.totp_secret = TotpSecret::generate().to_base32();
  state.last_used_step = None;
  write_atomic(path, &state)
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
  authorize_pairing(path, client_public_key, step, now, crate::pairing::ClientKind::Native)
}

pub fn authorize_pairing(
  path: &Path,
  client_public_key: &str,
  step: u64,
  now: u64,
  client_kind: crate::pairing::ClientKind,
) -> Result<(), String> {
  decode_public_key(client_public_key)?;
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  validate_step(&state, now, step)?;
  state.devices.retain(|_, grant| grant.active(now, true));
  if !state.devices.contains_key(client_public_key) && state.devices.len() >= MAX_DEVICES {
    return Err("Host paired-device limit reached; remove an unused device first".into());
  }
  let grant = match client_kind {
    crate::pairing::ClientKind::Native => DeviceAuthorization::Native { paired_at: now },
    crate::pairing::ClientKind::Browser => DeviceAuthorization::BrowserEnrollment {
      paired_at: now,
      expires_at: now.saturating_add(BROWSER_ENROLLMENT_SECONDS),
    },
  };
  state.devices.insert(client_public_key.into(), grant);
  state.last_used_step = Some(step);
  write_atomic(path, &state)
}

pub fn is_authorized(path: &Path, public_key: &str) -> Result<bool, String> {
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_err(|_| "Invalid host clock")?
    .as_secs();
  is_authorized_at(path, public_key, now)
}
pub(crate) fn is_authorized_at(path: &Path, public_key: &str, now: u64) -> Result<bool, String> {
  Ok(
    host_access(path)?
      .devices
      .get(public_key)
      .is_some_and(|grant| grant.active(now, false)),
  )
}
pub(crate) fn can_enroll_passkey_at(path: &Path, public_key: &str, now: u64) -> Result<bool, String> {
  Ok(
    host_access(path)?
      .devices
      .get(public_key)
      .is_some_and(|grant| grant.active(now, true)),
  )
}

pub fn devices(path: &Path) -> Result<BTreeMap<String, DeviceAuthorization>, String> {
  Ok(host_access(path)?.devices.clone())
}

/// Inspect host-local runtime administration without creating identity or trust.
pub fn runtime_management_enabled(path: &Path) -> Result<bool, String> {
  Ok(host_access(path)?.runtime_management)
}

/// This setting is provisioned on the host only. Existing connector requests
/// re-read it, so revocation takes effect without reconnecting or restarting.
pub fn set_runtime_management_enabled(path: &Path, enabled: bool) -> Result<(), String> {
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  state.runtime_management = enabled;
  write_atomic(path, &state)
}

/// Both authorization and the independent host opt-in are checked together.
/// Browser enrollment can register a passkey but cannot administer runtimes.
pub fn can_manage_runtimes(path: &Path, public_key: &str) -> Result<bool, String> {
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_err(|_| "Invalid host clock")?
    .as_secs();
  let state = host_access(path)?;
  Ok(
    state.runtime_management
      && state
        .devices
        .get(public_key)
        .is_some_and(|grant| grant.active(now, false)),
  )
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
  now: u64,
) -> Result<(), String> {
  let _lock = lock(path)?;
  let mut state = host_access(path)?;
  check_passkey_origin(&state, origin)?;
  if !state
    .devices
    .get(device_public_key)
    .is_some_and(|grant| grant.active(now, true))
  {
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
  state.devices.retain(|_, grant| grant.active(now, true));
  if !state.devices.contains_key(device_public_key) && state.devices.len() >= MAX_DEVICES {
    return Err("Host paired-device limit reached; remove an unused device first".into());
  }
  if !matches!(
    state.devices.get(device_public_key),
    Some(DeviceAuthorization::Native { .. })
  ) {
    state.devices.insert(
      device_public_key.into(),
      DeviceAuthorization::BrowserSession {
        paired_at: now,
        expires_at: now.saturating_add(BROWSER_SESSION_SECONDS),
      },
    );
  }
  write_atomic(path, &state)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::secure::NoiseIdentity;
  use std::fs;

  #[test]
  fn runtime_management_is_an_explicit_revocable_host_opt_in() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host-access.json");
    assert!(runtime_management_enabled(&path).is_err());
    assert!(!path.exists(), "Inspection must not create host trust");
    initialize_host_access(&path, &TotpSecret::generate()).unwrap();
    // Existing installations have no runtime permission field and stay denied.
    let mut old: serde_json::Value = read_required(&path).unwrap();
    old.as_object_mut().unwrap().remove("runtime_management");
    write_atomic(&path, &old).unwrap();
    assert!(!runtime_management_enabled(&path).unwrap());
    let native = NoiseIdentity::generate().unwrap().public_key();
    let browser = NoiseIdentity::generate().unwrap().public_key();
    let enrollment = NoiseIdentity::generate().unwrap().public_key();
    let expired = NoiseIdentity::generate().unwrap().public_key();
    let unknown = NoiseIdentity::generate().unwrap().public_key();
    let mut state = host_access(&path).unwrap();
    state
      .devices
      .insert(native.clone(), DeviceAuthorization::Native { paired_at: 1 });
    state.devices.insert(
      browser.clone(),
      DeviceAuthorization::BrowserSession {
        paired_at: 1,
        expires_at: u64::MAX,
      },
    );
    state.devices.insert(
      enrollment.clone(),
      DeviceAuthorization::BrowserEnrollment {
        paired_at: 1,
        expires_at: u64::MAX,
      },
    );
    state.devices.insert(
      expired.clone(),
      DeviceAuthorization::BrowserSession {
        paired_at: 1,
        expires_at: 2,
      },
    );
    write_atomic(&path, &state).unwrap();
    assert!(!can_manage_runtimes(&path, &native).unwrap());
    set_runtime_management_enabled(&path, true).unwrap();
    assert_eq!(
      read_required::<serde_json::Value>(&path).unwrap()["runtime_management"],
      true
    );
    for key in [&native, &browser] {
      assert!(can_manage_runtimes(&path, key).unwrap());
    }
    for key in [&enrollment, &expired, &unknown] {
      assert!(!can_manage_runtimes(&path, key).unwrap());
    }
    remove_device(&path, &browser).unwrap();
    assert!(!can_manage_runtimes(&path, &browser).unwrap());
    set_runtime_management_enabled(&path, false).unwrap();
    assert!(
      read_required::<serde_json::Value>(&path)
        .unwrap()
        .get("runtime_management")
        .is_none()
    );
    assert!(!can_manage_runtimes(&path, &native).unwrap());
    assert!(
      is_authorized(&path, &native).unwrap(),
      "Runtime opt-out preserves ordinary device access"
    );
  }

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
  fn browser_pairing_only_enrolls_while_native_pairing_is_persistent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host-access.json");
    initialize_host_access(&path, &TotpSecret::generate()).unwrap();
    let browser = NoiseIdentity::generate().unwrap().public_key();
    authorize_pairing(&path, &browser, 30, 900, crate::pairing::ClientKind::Browser).unwrap();
    assert!(!is_authorized_at(&path, &browser, 901).unwrap());
    assert!(can_enroll_passkey_at(&path, &browser, 901).unwrap());
    assert!(!can_enroll_passkey_at(&path, &browser, 899).unwrap());
    assert!(!can_enroll_passkey_at(&path, &browser, 1200).unwrap());
    let native = NoiseIdentity::generate().unwrap().public_key();
    authorize_device(&path, &native, 31, 930).unwrap();
    assert!(is_authorized_at(&path, &native, 930 + 365 * 86400).unwrap());
  }

  #[test]
  fn expired_browser_grants_do_not_exhaust_device_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host-access.json");
    initialize_host_access(&path, &TotpSecret::generate()).unwrap();
    let mut state = host_access(&path).unwrap();
    for _ in 0..MAX_DEVICES {
      state.devices.insert(
        NoiseIdentity::generate().unwrap().public_key(),
        DeviceAuthorization::BrowserSession {
          paired_at: 0,
          expires_at: 100,
        },
      );
    }
    write_atomic(&path, &state).unwrap();
    let native = NoiseIdentity::generate().unwrap().public_key();
    authorize_device(&path, &native, 30, 900).unwrap();
    assert_eq!(devices(&path).unwrap().len(), 1);
  }

  #[test]
  fn legacy_upgrade_is_explicit_rotates_seed_and_retires_unclassified_devices() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("host-access.json");
    let seed = TotpSecret::generate();
    let key = NoiseIdentity::generate().unwrap().public_key();
    let legacy = serde_json::json!({
      "version": 1, "totp_secret": seed.to_base32(), "devices": { key.clone(): 900 },
      "last_used_step": 30, "attempt_window_start": 900, "attempts": 3,
      "passkey_origin": "https://hub.example.com", "passkeys": [],
      "passkey_attempt_window_start": 901, "passkey_attempts": 4,
    });
    write_atomic(&path, &legacy).unwrap();
    assert!(is_authorized(&path, &key).unwrap_err().contains("upgrade-sha256"));
    assert_eq!(read_required::<serde_json::Value>(&path).unwrap(), legacy);
    upgrade_authenticator(&path).unwrap();
    let state = host_access(&path).unwrap();
    assert_ne!(state.totp_secret, seed.to_base32());
    assert!(state.devices.is_empty());
    assert_eq!(state.last_used_step, None);
    assert_eq!(state.attempts, 3);
    assert_eq!(state.passkey_attempts, 4);
    assert_eq!(state.passkey_origin.as_deref(), Some("https://hub.example.com"));
    let upgraded = read_required::<serde_json::Value>(&path).unwrap();
    upgrade_authenticator(&path).unwrap();
    assert_eq!(read_required::<serde_json::Value>(&path).unwrap(), upgraded);
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
