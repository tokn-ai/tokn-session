//! Default native storage and explicit offline migrations. Discovery is read-only.
use std::path::{Path, PathBuf};
use tokn_session_storage::{
  SessionPaths, directory_for_inspection, migrate_directory_with_alias_and_locks_checked, resolve_directory,
};

const MIGRATION_GUIDANCE: &str =
  "Stop all Session Hub servers, connectors, and desktop apps, then run `tokn-session-hub migrate-storage --offline`";

pub fn default_state_dir() -> Result<PathBuf, String> {
  default_state_dir_for_paths(&SessionPaths::discover()?)
}

pub fn default_state_dir_for_paths(paths: &SessionPaths) -> Result<PathBuf, String> {
  resolve_directory(&paths.legacy_hub_dir(), &paths.hub_dir()).map_err(|error| format!("{error}. {MIGRATION_GUIDANCE}"))
}

pub fn inspect_state_dir() -> Result<PathBuf, String> {
  inspect_state_dir_for_paths(&SessionPaths::discover()?)
}

/// Inspect legacy hosting before stopping it, without creating another identity.
pub fn inspect_state_dir_for_paths(paths: &SessionPaths) -> Result<PathBuf, String> {
  directory_for_inspection(&paths.legacy_hub_dir(), &paths.hub_dir())
    .map_err(|error| format!("{error}. {MIGRATION_GUIDANCE}"))
}

pub fn legacy_desktop_client_dir() -> Result<PathBuf, String> {
  dirs::config_dir()
    .map(|directory| directory.join("dev.tokn.session.viewer/hub-client"))
    .ok_or_else(|| "Cannot resolve the desktop configuration directory for migration".into())
}

/// The offline flag acknowledges that old servers have no lifetime ownership
/// lease. Connector and existing private-state locks still provide an enforced
/// check, and remain the same inode through the rename and compatibility alias.
pub fn migrate_legacy_storage(paths: &SessionPaths, offline: bool) -> Result<PathBuf, String> {
  require_offline(offline)?;
  migrate_directory_with_alias_and_locks_checked(
    &paths.legacy_hub_dir(),
    &paths.hub_dir(),
    &["host-enrollment.key.connector.lock", "host.key.connector.lock"],
    validate_hub_bundle,
  )?;
  default_state_dir_for_paths(paths)
}

/// Desktop clients have separate identities from CLI clients. Move their entire
/// bundle only while old app versions are stopped; they have no lifetime lease.
pub fn migrate_legacy_desktop_client(
  paths: &SessionPaths,
  legacy_client_dir: &Path,
  offline: bool,
) -> Result<PathBuf, String> {
  require_offline(offline)?;
  // Never initialize a destination inside a conflicting or unmigrated Hub.
  default_state_dir_for_paths(paths)?;
  migrate_directory_with_alias_and_locks_checked(
    legacy_client_dir,
    &paths.desktop_client_dir(),
    &["client-hosts.json.lock"],
    crate::onboarding::ClientStore::validate_existing,
  )?;
  resolve_directory(legacy_client_dir, &paths.desktop_client_dir())
}

fn present(path: &Path) -> Result<bool, String> {
  path
    .try_exists()
    .map_err(|error| format!("Could not inspect {}: {error}", path.display()))
}

fn validate_hub_bundle(bundle: &Path) -> Result<(), String> {
  use crate::{
    connector,
    onboarding::{self, ClientStore, HostProfile},
    secure::NoiseIdentity,
  };

  let profile = HostProfile::load(&bundle.join("host.json"))?;
  let enrollment = bundle.join("host-enrollment.key");
  let access = bundle.join("host-access.json");
  let noise = bundle.join("host-noise.key");
  let paired = profile.is_some() || present(&enrollment)? || present(&access)?;
  if paired {
    if profile.is_none() || !present(&enrollment)? || !present(&access)? || !present(&noise)? {
      return Err("Saved host identity or pairing bundle is incomplete; restore host.json, host-enrollment.key, host-noise.key, and host-access.json before migrating".into());
    }
    connector::validate_existing_identity(&enrollment)?;
    onboarding::validate_host_access_for_storage(&access)?;
  }
  let legacy_key = bundle.join("host.key");
  if present(&legacy_key)? {
    connector::validate_existing_identity(&legacy_key)?;
  }
  for key in [&noise, &bundle.join("client-noise.key")] {
    if present(key)? {
      NoiseIdentity::load(key)?;
    }
  }
  if present(&bundle.join("client-hosts.json"))? || present(&bundle.join("client.json"))? {
    ClientStore::validate_existing(bundle)?;
  }
  let desktop = bundle.join("desktop-client");
  if present(&desktop)? {
    ClientStore::validate_existing(&desktop)?;
  }
  let database = bundle.join("state.sqlite");
  if present(&database)? {
    let database = rusqlite::Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
      .map_err(|_| "Saved Hub database is invalid; restore it before migrating")?;
    let integrity: String = database
      .query_row("PRAGMA quick_check", [], |row| row.get(0))
      .map_err(|_| "Saved Hub database is invalid; restore it before migrating")?;
    if integrity != "ok" {
      return Err("Saved Hub database failed its integrity check; restore it before migrating".into());
    }
  }
  Ok(())
}

fn require_offline(offline: bool) -> Result<(), String> {
  if offline {
    Ok(())
  } else {
    Err(format!(
      "Offline storage migration requires --offline. {MIGRATION_GUIDANCE}"
    ))
  }
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use crate::{
    connector::{self, HostLease},
    onboarding::{self, ClientStore},
    secure::NoiseIdentity,
  };
  use std::fs;
  use tokn_session_storage::update_private_json;

  fn fixture() -> (tempfile::TempDir, SessionPaths) {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    (home, paths)
  }

  #[test]
  fn fresh_default_and_inspection_never_create_state() {
    let (_home, paths) = fixture();
    assert_eq!(default_state_dir_for_paths(&paths).unwrap(), paths.hub_dir());
    assert_eq!(inspect_state_dir_for_paths(&paths).unwrap(), paths.hub_dir());
    assert!(!paths.root().exists());
  }

  #[test]
  fn legacy_default_requires_offline_and_keeps_identity_bundle() {
    let (_home, paths) = fixture();
    let old_key = paths.legacy_hub_dir().join("host.key");
    connector::initialize_identity(&old_key).unwrap();
    let original_key = fs::read(&old_key).unwrap();
    assert!(
      default_state_dir_for_paths(&paths)
        .unwrap_err()
        .contains("migrate-storage --offline")
    );
    assert_eq!(inspect_state_dir_for_paths(&paths).unwrap(), paths.legacy_hub_dir());
    assert!(migrate_legacy_storage(&paths, false).is_err());
    assert!(!paths.root().exists());
    assert_eq!(migrate_legacy_storage(&paths, true).unwrap(), paths.hub_dir());
    assert_eq!(fs::read(&old_key).unwrap(), original_key);
    assert_eq!(default_state_dir_for_paths(&paths).unwrap(), paths.hub_dir());
  }

  #[test]
  fn active_connector_prevents_moving_its_identity() {
    let (_home, paths) = fixture();
    let key = paths.legacy_hub_dir().join("host-enrollment.key");
    let _lease = HostLease::acquire(&key).unwrap();
    assert!(migrate_legacy_storage(&paths, true).unwrap_err().contains("in use"));
    assert!(!paths.hub_dir().exists());
  }

  #[test]
  fn desktop_bundle_moves_independently_and_never_merges_trust() {
    let (home, paths) = fixture();
    let old_client = home.path().join("app-config/hub-client");
    let hub = url::Url::parse("https://hub.example").unwrap();
    let client = ClientStore::load_or_create(&old_client, &hub).unwrap();
    let host_id = uuid::Uuid::new_v4().to_string();
    client
      .save_host(onboarding::SavedHost {
        host_id: host_id.clone(),
        host_public_key: NoiseIdentity::generate().unwrap().public_key(),
        machine_address: None,
        name: None,
      })
      .unwrap();
    client.select_host(&host_id).unwrap();
    let original_key = fs::read(client.identity_file()).unwrap();
    let original_trust = fs::read(old_client.join("client-hosts.json")).unwrap();
    assert!(migrate_legacy_desktop_client(&paths, &old_client, false).is_err());
    migrate_legacy_storage(&paths, true).unwrap();
    assert_eq!(
      migrate_legacy_desktop_client(&paths, &old_client, true).unwrap(),
      paths.desktop_client_dir()
    );
    assert_eq!(fs::read(client.identity_file()).unwrap(), original_key);
    assert_eq!(fs::read(old_client.join("client-hosts.json")).unwrap(), original_trust);
    assert_eq!(
      ClientStore::load_existing(&paths.desktop_client_dir(), &hub)
        .unwrap()
        .selected_host()
        .unwrap(),
      Some(host_id)
    );
    migrate_legacy_desktop_client(&paths, &old_client, true).unwrap();
  }

  fn host_options(directory: PathBuf) -> crate::host_setup::HostOptions {
    crate::host_setup::HostOptions {
      hub: Some(url::Url::parse("https://hub.example").unwrap()),
      name: None,
      viewer_url: None,
      passkey_origin: None,
      state_dir: directory,
      totp_secret_file: None,
      viewer_token: None,
      allow_control: None,
      insecure_loopback: false,
      ice_servers: None,
    }
  }

  #[test]
  fn paired_bundle_and_legacy_authenticator_move_without_rotating_trust() {
    let (_home, paths) = fixture();
    crate::host_setup::prepare_host(host_options(paths.legacy_hub_dir())).unwrap();
    let access = paths.legacy_hub_dir().join("host-access.json");
    let device = NoiseIdentity::generate().unwrap().public_key();
    update_private_json(&access, 256 * 1024, |previous: Option<serde_json::Value>| {
      let mut state = previous.unwrap();
      state["version"] = serde_json::json!(1);
      state["devices"] = serde_json::json!({device: 900});
      Ok(state)
    })
    .unwrap();
    let original_access = fs::read(&access).unwrap();
    let key = paths.legacy_hub_dir().join("host-enrollment.key");
    let original_key = fs::read(&key).unwrap();
    migrate_legacy_storage(&paths, true).unwrap();
    assert_eq!(fs::read(&access).unwrap(), original_access);
    assert_eq!(fs::read(&key).unwrap(), original_key);
    assert!(
      onboarding::read_totp_secret(&access)
        .err()
        .unwrap()
        .contains("upgrade-sha256")
    );
  }

  #[test]
  fn orphan_pairing_or_semantic_invalid_key_never_moves_or_regenerates() {
    let (_home, paths) = fixture();
    onboarding::initialize_host_access(
      &paths.legacy_hub_dir().join("host-access.json"),
      &crate::pairing::TotpSecret::generate(),
    )
    .unwrap();
    assert!(migrate_legacy_storage(&paths, true).unwrap_err().contains("incomplete"));
    assert!(!paths.hub_dir().exists());
    assert!(!paths.legacy_hub_dir().join("host-enrollment.key").exists());
    assert!(!paths.legacy_hub_dir().join("host-noise.key").exists());
    let (_other_home, other) = fixture();
    update_private_json(
      &other.legacy_hub_dir().join("host.key"),
      128,
      |_: Option<serde_json::Value>| Ok(serde_json::json!({"version": 999, "secret_key": "invalid"})),
    )
    .unwrap();
    assert!(migrate_legacy_storage(&other, true).is_err());
    assert!(!other.hub_dir().exists());
  }

  #[test]
  fn incomplete_or_semantic_invalid_desktop_identity_is_preserved_for_repair() {
    let (home, paths) = fixture();
    let old_client = home.path().join("config/hub-client");
    let hub = url::Url::parse("https://hub.example").unwrap();
    let store = ClientStore::load_or_create(&old_client, &hub).unwrap();
    fs::remove_file(store.identity_file()).unwrap();
    assert!(migrate_legacy_desktop_client(&paths, &old_client, true).is_err());
    assert!(!store.identity_file().exists());
    assert!(!paths.desktop_client_dir().exists());
    NoiseIdentity::load_or_create(&store.identity_file()).unwrap();
    fs::write(old_client.join("client-hosts.json"), "{}").unwrap();
    assert!(migrate_legacy_desktop_client(&paths, &old_client, true).is_err());
    assert_eq!(fs::read_to_string(old_client.join("client-hosts.json")).unwrap(), "{}");
    assert!(!paths.desktop_client_dir().exists());
  }

  #[test]
  fn server_only_database_needs_no_connector_identity() {
    let (_home, paths) = fixture();
    drop(crate::store::Store::open(paths.legacy_hub_dir().join("state.sqlite")).unwrap());
    migrate_legacy_storage(&paths, true).unwrap();
    assert!(paths.hub_dir().join("state.sqlite").exists());
    assert!(!paths.hub_dir().join("host-enrollment.key").exists());
  }

  #[test]
  fn offline_client_restore_recreates_only_lock_metadata() {
    let (home, paths) = fixture();
    let old_client = home.path().join("config/hub-client");
    let hub = url::Url::parse("https://hub.example").unwrap();
    let store = ClientStore::load_or_create(&old_client, &hub).unwrap();
    let original_key = fs::read(store.identity_file()).unwrap();
    let original_trust = fs::read(old_client.join("client-hosts.json")).unwrap();
    fs::remove_file(old_client.join("client-hosts.json.lock")).unwrap();
    migrate_legacy_desktop_client(&paths, &old_client, true).unwrap();
    ClientStore::load_existing(&paths.desktop_client_dir(), &hub).unwrap();
    assert_eq!(fs::read(store.identity_file()).unwrap(), original_key);
    assert_eq!(fs::read(old_client.join("client-hosts.json")).unwrap(), original_trust);
  }
}
