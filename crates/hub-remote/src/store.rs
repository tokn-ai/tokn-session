//! Persisted native device identity, per-Hub host pins, and remembered selection.
use crate::persistence::{
  lock, nofollow, read_optional, read_required, reject_symlink, validate_private, write_atomic,
};
use serde::{Deserialize, Serialize};
use std::{
  collections::BTreeMap,
  fs::OpenOptions,
  path::{Path, PathBuf},
};
use tokn_hub_client_core::secure::decode_public_key;
use url::Url;
use uuid::Uuid;

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
  /// Validate an existing bundle without generating keys or taking a new lock.
  /// Callers that migrate/write state must already hold its ownership lock.
  pub fn validate_existing(state_dir: &Path) -> Result<(), String> {
    let state: ClientHosts = read_required(&state_dir.join("client-hosts.json"))?;
    validate_client_state(&state)?;
    tokn_hub_client_core::secure::NoiseIdentity::load(&state_dir.join("client-noise.key"))?;
    Ok(())
  }

  /// Open an existing identity/trust bundle without generating missing keys.
  /// Used by upgraded desktops that continue using unmigrated legacy storage.
  pub fn load_existing(state_dir: &Path, hub: &Url) -> Result<Self, String> {
    let store = Self {
      state_dir: state_dir.into(),
      hub: canonical_hub(hub)?,
    };
    let path = store.path();
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    reject_symlink(&lock_path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    nofollow(&mut options);
    let lock = options.open(&lock_path).map_err(
      |_| "Saved desktop client ownership lock is missing; restore the complete client bundle before reconnecting",
    )?;
    validate_private(&lock)?;
    lock
      .try_lock()
      .map_err(|_| "Saved desktop client state is being updated; retry shortly")?;
    Self::validate_existing(state_dir)?;
    Ok(store)
  }

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
        tokn_hub_client_core::secure::NoiseIdentity::load_or_create(&store.identity_file())?;
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tokn_hub_client_core::secure::NoiseIdentity;

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
  fn existing_client_never_generates_or_repairs_an_incomplete_bundle() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("client");
    let hub = Url::parse("https://hub.example").unwrap();
    assert!(ClientStore::load_existing(&root, &hub).is_err());
    assert!(!root.exists());
    let store = ClientStore::load_or_create(&root, &hub).unwrap();
    let before = fs::read(store.identity_file()).unwrap();
    let existing = ClientStore::load_existing(&root, &hub).unwrap();
    assert!(existing.hosts().unwrap().is_empty());
    assert_eq!(fs::read(store.identity_file()).unwrap(), before);
    fs::remove_file(store.path()).unwrap();
    assert!(ClientStore::load_existing(&root, &hub).is_err());
    assert!(!store.path().exists());
    assert_eq!(fs::read(store.identity_file()).unwrap(), before);
    fs::remove_file(store.identity_file()).unwrap();
    assert!(ClientStore::load_existing(&root, &hub).is_err());
    assert!(!store.identity_file().exists());
  }

  #[tokio::test]
  async fn native_manager_never_replaces_damaged_or_partial_device_state() {
    use crate::RemoteManager;

    let directory = tempfile::tempdir().unwrap();
    let hub = "https://hub.example";
    let manager = RemoteManager::new(directory.path().to_owned());
    let initial = manager.status(hub).await.unwrap();
    let key = directory.path().join("client-noise.key");
    let pins = directory.path().join("client-hosts.json");
    let original_key = fs::read(&key).unwrap();
    let original_pins = fs::read(&pins).unwrap();

    let reopened = RemoteManager::new(directory.path().to_owned());
    assert_eq!(
      reopened.status(hub).await.unwrap().device_public_key,
      initial.device_public_key
    );
    fs::write(&key, b"broken identity").unwrap();
    assert!(reopened.status(hub).await.is_err());
    assert_eq!(fs::read(&key).unwrap(), b"broken identity");
    assert_eq!(fs::read(&pins).unwrap(), original_pins);

    fs::write(&key, &original_key).unwrap();
    fs::write(&pins, b"broken pins").unwrap();
    assert!(reopened.status(hub).await.is_err());
    assert_eq!(fs::read(&pins).unwrap(), b"broken pins");
    assert_eq!(fs::read(&key).unwrap(), original_key);

    fs::remove_file(&pins).unwrap();
    assert!(reopened.status(hub).await.is_err());
    assert!(!pins.exists());
    assert_eq!(fs::read(&key).unwrap(), original_key);
    fs::write(&pins, &original_pins).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(&pins, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::remove_file(&key).unwrap();
    assert!(reopened.status(hub).await.is_err());
    assert!(!key.exists());
    assert_eq!(fs::read(&pins).unwrap(), original_pins);
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
}
