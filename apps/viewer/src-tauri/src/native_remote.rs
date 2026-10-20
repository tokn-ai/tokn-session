//! Resolve desktop identity storage lazily, outside the app setup/UI thread.
use std::path::PathBuf;
use tokio::sync::{OnceCell, RwLock};
use tokn_hub_remote::RemoteManager;
use tokn_session_storage::{SessionPaths, directory_for_inspection};

pub struct NativeRemote {
  paths: SessionPaths,
  legacy_client: PathBuf,
  initialized: OnceCell<RemoteManager>,
  stopped: RwLock<bool>,
}

impl NativeRemote {
  /// Listing runtimes must not generate or load desktop identity storage.
  pub async fn list_connections(&self) -> Vec<tokn_hub_remote::ConnectionInfo> {
    match self.initialized.get() {
      Some(manager) => manager.list_connections().await,
      None => Vec::new(),
    }
  }

  pub fn new(paths: SessionPaths, legacy_client: PathBuf) -> Self {
    Self {
      paths,
      legacy_client,
      initialized: OnceCell::new(),
      stopped: RwLock::new(false),
    }
  }

  pub async fn manager(&self) -> Result<&RemoteManager, String> {
    let stopped = self.stopped.read().await;
    if *stopped {
      return Err("Remote machine services are shutting down".into());
    }
    self
      .initialized
      .get_or_try_init(|| async {
        let paths = self.paths.clone();
        let legacy = self.legacy_client.clone();
        tokio::task::spawn_blocking(move || prepare(&paths, legacy))
          .await
          .map_err(|error| format!("Desktop identity storage initialization failed: {error}"))?
      })
      .await
  }

  pub async fn shutdown(&self) {
    let mut stopped = self.stopped.write().await;
    *stopped = true;
    if let Some(manager) = self.initialized.get() {
      manager.close_all().await;
    }
  }
}

fn prepare(paths: &SessionPaths, legacy: PathBuf) -> Result<RemoteManager, String> {
  // A real legacy Hub must migrate first. Creating desktop-client underneath
  // the destination Hub would turn that complete-bundle move into a conflict.
  let hub = tokn_session_hub::storage::inspect_state_dir_for_paths(paths)?;
  let client = directory_for_inspection(&legacy, &paths.desktop_client_dir())?;
  let existing = client
    .try_exists()
    .map_err(|error| format!("Could not inspect desktop client storage: {error}"))?;
  if hub == paths.legacy_hub_dir() && !existing {
    return Err(format!(
      "Legacy Hub storage remains at {}. Stop legacy services and run `tokn-session-hub migrate-storage --offline` before creating desktop client storage. Restore an existing client bundle instead of regenerating its keys.",
      hub.display()
    ));
  }
  if existing {
    // Validation is non-generating. The manager repeats it for each store
    // operation, so later deletion never silently replaces a legacy identity.
    tokn_session_hub::onboarding::ClientStore::load_existing(&client, &url::Url::parse("https://storage-check.invalid").unwrap())
      .map_err(|error| format!("Saved desktop client storage is incomplete or invalid: {error}. Restore the complete client bundle before reconnecting."))?;
    Ok(RemoteManager::existing(client))
  } else {
    Ok(RemoteManager::new(client))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokn_session_hub::{onboarding::ClientStore, secure::NoiseIdentity};

  #[tokio::test]
  async fn unused_and_shutdown_remote_services_do_not_create_storage() {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    let legacy = home.path().join("config/hub-client");
    let remote = NativeRemote::new(paths.clone(), legacy.clone());
    assert!(remote.list_connections().await.is_empty());
    assert!(remote.initialized.get().is_none());
    remote.shutdown().await;
    assert!(remote.list_connections().await.is_empty());
    assert!(remote.manager().await.is_err());
    assert!(!paths.root().exists());
    assert!(!legacy.exists());
  }

  #[tokio::test]
  async fn pending_hub_uses_existing_client_without_creating_new_hub() {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    std::fs::create_dir_all(paths.legacy_hub_dir()).unwrap();
    let legacy = home.path().join("config/hub-client");
    let hub = url::Url::parse("https://hub.example").unwrap();
    let client = ClientStore::load_or_create(&legacy, &hub).unwrap();
    let key = NoiseIdentity::load(&client.identity_file()).unwrap().public_key();
    let remote = NativeRemote::new(paths.clone(), legacy.clone());
    let status = remote.manager().await.unwrap().status(hub.as_str()).await.unwrap();
    assert_eq!(status.device_public_key, key);
    assert!(!paths.hub_dir().exists());
    std::fs::remove_file(client.identity_file()).unwrap();
    assert!(remote.manager().await.unwrap().status(hub.as_str()).await.is_err());
    assert!(!client.identity_file().exists());
  }

  #[test]
  fn partial_or_conflicting_client_bundles_fail_without_generating_keys() {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    let legacy = home.path().join("config/hub-client");
    std::fs::create_dir_all(&legacy).unwrap();
    assert!(prepare(&paths, legacy.clone()).is_err());
    assert_eq!(std::fs::read_dir(&legacy).unwrap().count(), 0);
    std::fs::create_dir_all(paths.desktop_client_dir()).unwrap();
    assert!(prepare(&paths, legacy).is_err());
    assert_eq!(std::fs::read_dir(paths.desktop_client_dir()).unwrap().count(), 0);
  }

  #[test]
  fn pending_legacy_hub_never_generates_client_identity() {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    std::fs::create_dir_all(paths.legacy_hub_dir()).unwrap();
    let legacy = home.path().join("config/hub-client");
    assert!(
      prepare(&paths, legacy.clone())
        .err()
        .unwrap()
        .contains("migrate-storage --offline")
    );
    assert!(!legacy.exists());
    assert!(!paths.hub_dir().exists());
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn migrated_client_alias_preserves_identity_and_new_installs_use_native_root() {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    let legacy = home.path().join("config/hub-client");
    let hub = url::Url::parse("https://hub.example").unwrap();
    let client = ClientStore::load_or_create(&legacy, &hub).unwrap();
    let key = NoiseIdentity::load(&client.identity_file()).unwrap().public_key();
    tokn_session_storage::migrate_directory_with_alias(&legacy, &paths.desktop_client_dir()).unwrap();
    let manager = prepare(&paths, legacy).unwrap();
    assert_eq!(manager.status(hub.as_str()).await.unwrap().device_public_key, key);
    let new_home = tempfile::tempdir().unwrap();
    let new_paths = SessionPaths::from_home(new_home.path());
    let manager = prepare(&new_paths, new_home.path().join("legacy/hub-client")).unwrap();
    assert!(!new_paths.root().exists());
    manager.status(hub.as_str()).await.unwrap();
    assert!(new_paths.desktop_client_dir().join("client-hosts.json").exists());
  }
}
