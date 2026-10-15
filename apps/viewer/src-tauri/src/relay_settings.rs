//! Non-secret reader configuration imports once; the native file always wins.
use crate::relay::RelaySettings;
use std::{
  fs,
  io::Read,
  path::{Path, PathBuf},
};
use tokn_session_storage::{SessionPaths, read_private_json, update_private_json};

const LIMIT: u64 = 16_384;

pub fn load(path: &Path) -> Result<RelaySettings, String> {
  let settings = read_private_json::<RelaySettings>(path, LIMIT)?.unwrap_or_default();
  settings.validate()?;
  Ok(settings)
}

pub fn save(path: &Path, settings: &RelaySettings) -> Result<(), String> {
  settings.validate()?;
  update_private_json(path, LIMIT, |current: Option<RelaySettings>| {
    if let Some(current) = current {
      current.validate()?;
    }
    Ok(settings.clone())
  })?;
  Ok(())
}

pub fn resolve(paths: &SessionPaths, legacy: &Path) -> Result<PathBuf, String> {
  let path = paths.viewer_dir().join("relay.json");
  // Read first, so an existing malformed native configuration is never replaced
  // with old preferences or defaults. Neither missing read creates its parent.
  if let Some(settings) = read_private_json::<RelaySettings>(&path, LIMIT)? {
    settings.validate()?;
    return Ok(path);
  }
  let Some(settings) = legacy_settings(legacy)? else {
    return Ok(path);
  };
  update_private_json(&path, LIMIT, |current: Option<RelaySettings>| {
    let settings = current.unwrap_or(settings);
    settings.validate()?;
    Ok(settings)
  })?;
  Ok(path)
}

fn legacy_settings(path: &Path) -> Result<Option<RelaySettings>, String> {
  let metadata = match fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(format!("Could not inspect legacy Relay settings: {error}")),
  };
  if !metadata.is_file() || metadata.len() > LIMIT {
    return Err("Legacy Relay settings must be a bounded regular file; restore relay.json before importing it".into());
  }
  let mut options = fs::OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options.open(path).map_err(|error| error.to_string())?;
  let metadata = file.metadata().map_err(|error| error.to_string())?;
  if !metadata.is_file() || metadata.len() > LIMIT {
    return Err("Legacy Relay settings must be a bounded regular file".into());
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    // The old non-secret writer used mode 0644. Require ownership and reject
    // shared write permission without demanding the new private-file mode.
    // SAFETY: geteuid only reads the current effective user ID.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 || metadata.nlink() != 1 {
      return Err(
        "Legacy Relay settings must belong to this user, have no hard links, and not be writable by others".into(),
      );
    }
  }
  let mut bytes = Vec::new();
  file
    .take(LIMIT + 1)
    .read_to_end(&mut bytes)
    .map_err(|error| error.to_string())?;
  decode_legacy(bytes)
}

fn decode_legacy(bytes: Vec<u8>) -> Result<Option<RelaySettings>, String> {
  if bytes.len() as u64 > LIMIT {
    return Err("Legacy Relay settings exceed their size limit".into());
  }
  let settings: RelaySettings = serde_json::from_slice(&bytes)
    .map_err(|_| "Invalid legacy Relay settings; restore relay.json instead of replacing it")?;
  settings.validate()?;
  Ok(Some(settings))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn first_import_preserves_legacy_and_native_values_win_afterwards() {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    let legacy = home.path().join("relay.json");
    let settings = RelaySettings {
      include_native: false,
      ..Default::default()
    };
    fs::write(&legacy, serde_json::to_vec(&settings).unwrap()).unwrap();
    let path = resolve(&paths, &legacy).unwrap();
    assert_eq!(load(&path).unwrap(), settings);
    assert!(legacy.exists());
    save(&path, &RelaySettings::default()).unwrap();
    fs::write(&legacy, "broken legacy").unwrap();
    assert_eq!(resolve(&paths, &legacy).unwrap(), path);
    assert_eq!(load(&path).unwrap(), RelaySettings::default());
    assert!(!paths.hub_dir().exists());
  }

  #[test]
  fn malformed_files_are_not_replaced_and_missing_reads_are_pure() {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    let legacy = home.path().join("relay.json");
    let path = resolve(&paths, &legacy).unwrap();
    assert!(!paths.root().exists());
    fs::write(&legacy, "broken").unwrap();
    assert!(resolve(&paths, &legacy).is_err());
    assert!(!paths.root().exists());
    fs::remove_file(&legacy).unwrap();
    save(&path, &RelaySettings::default()).unwrap();
    fs::write(&path, "broken native").unwrap();
    assert!(resolve(&paths, &legacy).is_err());
    assert!(save(&path, &RelaySettings::default()).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "broken native");
  }

  #[cfg(unix)]
  #[test]
  fn import_rejects_aliases_and_non_private_destination() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    let source = home.path().join("source.json");
    let legacy = home.path().join("relay.json");
    fs::write(&source, serde_json::to_vec(&RelaySettings::default()).unwrap()).unwrap();
    symlink(&source, &legacy).unwrap();
    assert!(resolve(&paths, &legacy).is_err());
    fs::remove_file(&legacy).unwrap();
    let path = resolve(&paths, &legacy).unwrap();
    save(&path, &RelaySettings::default()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(resolve(&paths, &legacy).is_err());
  }
}
