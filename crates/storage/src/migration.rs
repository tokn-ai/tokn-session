#[cfg(unix)]
use crate::private::{acquire_lock, private_directory, sync_directory, update_private_json, validate_file};
use crate::private::{metadata, read_private_json};
use serde::{Deserialize, Serialize};
use std::{
  fs,
  path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Migration {
  version: u8,
  legacy: PathBuf,
  destination: PathBuf,
}

fn migration_file(destination: &Path) -> Result<PathBuf, String> {
  let name = destination
    .file_name()
    .ok_or("Migration destination needs a directory name")?;
  let parent = destination.parent().ok_or("Migration destination needs a parent")?;
  let mut name = name.to_owned();
  name.push(".storage-migration.json");
  Ok(parent.join(name))
}

fn pending(legacy: &Path, destination: &Path) -> Result<bool, String> {
  let Some(migration) = read_private_json::<Migration>(&migration_file(destination)?, 8192)? else {
    return Ok(false);
  };
  if migration.version != 1 || migration.legacy != legacy || migration.destination != destination {
    return Err(
      "Storage migration record belongs to different paths; restore the original stores before retrying".into(),
    );
  }
  Ok(true)
}

fn real_directory(path: &Path) -> Result<bool, String> {
  match metadata(path)? {
    Some(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
    Some(_) => Err(format!("Storage requires a real directory: {}", path.display())),
    None => Ok(false),
  }
}

fn exact_alias(legacy: &Path, destination: &Path) -> Result<bool, String> {
  let Some(metadata) = metadata(legacy)? else {
    return Ok(false);
  };
  if !metadata.file_type().is_symlink() {
    return Ok(false);
  }
  let target = fs::read_link(legacy).map_err(|error| error.to_string())?;
  if target != destination {
    return Err(format!(
      "Unrecognized storage alias {}; expected {}",
      legacy.display(),
      destination.display()
    ));
  }
  if !real_directory(destination)? {
    return Err(format!(
      "Storage alias points to missing state: {}",
      destination.display()
    ));
  }
  Ok(true)
}

/// Select the native destination without silently importing or choosing conflicting identities.
pub fn resolve_directory(legacy: &Path, destination: &Path) -> Result<PathBuf, String> {
  if pending(legacy, destination)? {
    return Err("Storage migration is incomplete; retry the offline migration before starting".into());
  }
  if exact_alias(legacy, destination)? {
    return Ok(destination.to_owned());
  }
  let old = real_directory(legacy)?;
  let new = real_directory(destination)?;
  match (old, new) {
    (true, true) => Err(format!(
      "Conflicting storage exists at {} and {}; do not merge identities",
      legacy.display(),
      destination.display()
    )),
    (true, false) => Err(format!(
      "Legacy storage remains at {}; stop its services and migrate it offline before starting",
      legacy.display()
    )),
    (false, _) => Ok(destination.to_owned()),
  }
}

/// Permit inspecting/stopping an unmigrated legacy service without creating new identities.
pub fn directory_for_inspection(legacy: &Path, destination: &Path) -> Result<PathBuf, String> {
  if pending(legacy, destination)? {
    return Err("Storage migration is incomplete; retry the offline migration before inspecting state".into());
  }
  if exact_alias(legacy, destination)? {
    return Ok(destination.to_owned());
  }
  let old = real_directory(legacy)?;
  let new = real_directory(destination)?;
  match (old, new) {
    (true, true) => Err(format!(
      "Conflicting storage exists at {} and {}; resolve it before changing either",
      legacy.display(),
      destination.display()
    )),
    (true, false) => Ok(legacy.to_owned()),
    (false, _) => Ok(destination.to_owned()),
  }
}

/// Rename a complete private bundle and leave an exact compatibility alias.
///
/// Callers must stop legacy processes that do not hold ownership locks. Existing
/// lock files are held until the old and new paths share the same directory.
/// A journal makes an interrupted rename retryable, without duplicating keys.
pub fn migrate_directory_with_alias(legacy: &Path, destination: &Path) -> Result<(), String> {
  migrate_directory_with_alias_and_locks(legacy, destination, &[])
}

/// Also acquire named ownership locks before inspecting a bundle, including
/// locks that a stopped connector has not created yet. Names are relative to
/// the bundle and must be direct filenames ending in `.lock`.
pub fn migrate_directory_with_alias_and_locks(
  legacy: &Path,
  destination: &Path,
  ownership_locks: &[&str],
) -> Result<(), String> {
  migrate_directory_with_alias_and_locks_checked(legacy, destination, ownership_locks, |_| Ok(()))
}

/// Validate provider-owned schemas while every bundle ownership lock is held.
/// The validator must only read: taking one of these same locks would deadlock.
pub fn migrate_directory_with_alias_and_locks_checked(
  legacy: &Path,
  destination: &Path,
  ownership_locks: &[&str],
  validate: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
  if !legacy.is_absolute()
    || !destination.is_absolute()
    || legacy == destination
    || legacy.starts_with(destination)
    || destination.starts_with(legacy)
  {
    return Err("Storage migration needs separate absolute directories".into());
  }
  #[cfg(not(unix))]
  {
    let _ = (ownership_locks, validate);
    // Migration support is unnecessary for fresh/native-only stores. Inspect
    // before returning so malformed journals and conflicting paths still fail.
    let was_pending = pending(legacy, destination)?;
    if !was_pending && exact_alias(legacy, destination)? {
      return Ok(());
    }
    let old = real_directory(legacy)?;
    let new = real_directory(destination)?;
    if old && new {
      return Err(format!(
        "Conflicting storage exists at {} and {}; migration never merges identities",
        legacy.display(),
        destination.display()
      ));
    }
    if !old && !was_pending {
      return Ok(());
    }
    Err(
      "Offline storage migration requires directory alias support; retain an explicit legacy path on this platform"
        .into(),
    )
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::symlink;
    let parent = destination.parent().ok_or("Migration destination needs a parent")?;
    private_directory(parent)?;
    let record = migration_file(destination)?;
    let mut lock_name = record.as_os_str().to_owned();
    lock_name.push(".migration-lock");
    let _migration_lock = acquire_lock(Path::new(&lock_name))?;
    let was_pending = pending(legacy, destination)?;
    if exact_alias(legacy, destination)? {
      if was_pending {
        fs::remove_file(&record).map_err(|error| error.to_string())?;
        sync_directory(parent)?;
      }
      return Ok(());
    }
    let old = real_directory(legacy)?;
    let new = real_directory(destination)?;
    if old && new {
      return Err(format!(
        "Conflicting storage exists at {} and {}; migration never merges identities",
        legacy.display(),
        destination.display()
      ));
    }
    if !old && !was_pending {
      // New installations and already-native stores have nothing to migrate.
      return Ok(());
    }
    if !old && !new {
      return Err("Storage migration lost both source and destination; restore its bundle before retrying".into());
    }
    let bundle = if old { legacy } else { destination };
    let _bundle_locks = validate_and_lock_bundle(bundle, ownership_locks)?;
    validate(bundle)?;
    if !was_pending {
      update_private_json(&record, 8192, |current: Option<Migration>| {
        if current.is_some() {
          return Err("Storage migration changed concurrently; retry".into());
        }
        Ok(Migration {
          version: 1,
          legacy: legacy.to_owned(),
          destination: destination.to_owned(),
        })
      })?;
    }
    if old {
      fs::rename(legacy, destination)
        .map_err(|error| format!("Could not move complete storage bundle: {error}; retry the offline migration"))?;
      sync_directory(parent)?;
      if let Some(old_parent) = legacy.parent() {
        sync_directory(old_parent)?;
      }
    }
    // Never replace anything that reappeared at the old location. The pending
    // record blocks defaults until an operator resolves such a conflict.
    symlink(destination, legacy).map_err(|error| {
      format!("Storage moved but its compatibility alias could not be created: {error}; retry the offline migration")
    })?;
    if let Some(old_parent) = legacy.parent() {
      sync_directory(old_parent)?;
    }
    fs::remove_file(&record)
      .map_err(|error| format!("Storage migrated but its completion record could not be cleared: {error}; retry"))?;
    sync_directory(parent)?;
    Ok(())
  }
}

#[cfg(unix)]
fn validate_and_lock_bundle(path: &Path, ownership_locks: &[&str]) -> Result<Vec<fs::File>, String> {
  use std::os::unix::fs::{MetadataExt, PermissionsExt};
  fn visit(
    path: &Path,
    held: &[PathBuf],
    locks: &mut Vec<fs::File>,
    directories: &mut Vec<PathBuf>,
  ) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    // SAFETY: geteuid has no memory side effects.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.file_type().is_symlink() {
      return Err(format!(
        "Migration requires owned real files/directories: {}",
        path.display()
      ));
    }
    if metadata.is_dir() {
      directories.push(path.to_owned());
      for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
        visit(
          &entry.map_err(|error| error.to_string())?.path(),
          held,
          locks,
          directories,
        )?;
      }
    } else if metadata.is_file() {
      let file = crate::private::open_private(path, false)?.ok_or("Storage disappeared while migrating")?;
      validate_file(&file)?;
      if path.extension().is_some_and(|extension| extension == "lock") && !held.iter().any(|held| held == path) {
        file.try_lock().map_err(|error| {
          format!(
            "Storage is in use at {}; stop its connector/client/server and retry: {error}",
            path.display()
          )
        })?;
        locks.push(file);
      }
      if path
        .extension()
        .is_some_and(|extension| extension == "json" || extension == "key")
      {
        read_private_json::<serde_json::Value>(path, 4 * 1024 * 1024)?;
      }
    } else {
      return Err(format!("Migration requires regular files: {}", path.display()));
    }
    Ok(())
  }
  let mut locks = Vec::new();
  let mut held = Vec::new();
  for name in ownership_locks {
    if Path::new(name).components().count() != 1 || !name.ends_with(".lock") || *name == ".lock" {
      return Err("Migration ownership locks must be direct .lock filenames".into());
    }
    let lock_path = path.join(name);
    if !held.contains(&lock_path) {
      locks.push(acquire_lock(&lock_path)?);
      held.push(lock_path);
    }
  }
  let mut directories = Vec::new();
  visit(path, &held, &mut locks, &mut directories)?;
  for directory in directories {
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())?;
  }
  Ok(locks)
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  use std::os::unix::fs::{MetadataExt, symlink};

  fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let old = directory.path().join("old/hub");
    let new = directory.path().join("new/hub");
    private_directory(&old).unwrap();
    update_private_json(&old.join("host.key"), 128, |_: Option<Vec<u32>>| Ok(vec![1, 2])).unwrap();
    (directory, old, new)
  }

  #[test]
  fn complete_bundle_and_lock_inode_move_once_and_old_alias_stays_usable() {
    let (_directory, old, new) = fixture();
    let old_lock = old.join("host.key.lock");
    let inode = fs::metadata(&old_lock).unwrap().ino();
    migrate_directory_with_alias(&old, &new).unwrap();
    assert_eq!(resolve_directory(&old, &new).unwrap(), new);
    assert_eq!(fs::metadata(&old_lock).unwrap().ino(), inode);
    assert_eq!(fs::metadata(new.join("host.key.lock")).unwrap().ino(), inode);
    assert_eq!(
      read_private_json::<Vec<u32>>(&old.join("host.key"), 128).unwrap(),
      Some(vec![1, 2])
    );
    migrate_directory_with_alias(&old, &new).unwrap();
    assert_eq!(fs::metadata(&new).unwrap().mode() & 0o777, 0o700);
  }

  #[test]
  fn running_lock_blocks_move_and_original_bundle_survives() {
    let (_directory, old, new) = fixture();
    let _active = acquire_lock(&old.join("host.key.lock")).unwrap();
    assert!(migrate_directory_with_alias(&old, &new).unwrap_err().contains("in use"));
    assert!(old.join("host.key").exists());
    assert!(!new.exists());
  }

  #[test]
  fn conflicting_or_unknown_aliases_are_never_merged() {
    let (directory, old, new) = fixture();
    private_directory(&new).unwrap();
    assert!(
      migrate_directory_with_alias(&old, &new)
        .unwrap_err()
        .contains("Conflicting")
    );
    fs::remove_dir(&new).unwrap();
    fs::remove_dir_all(&old).unwrap();
    symlink(directory.path(), &old).unwrap();
    assert!(resolve_directory(&old, &new).unwrap_err().contains("Unrecognized"));
  }

  #[test]
  fn interrupted_rename_restores_alias_without_copying_or_replacing_state() {
    let (_directory, old, new) = fixture();
    private_directory(new.parent().unwrap()).unwrap();
    update_private_json(&migration_file(&new).unwrap(), 8192, |_: Option<Migration>| {
      Ok(Migration {
        version: 1,
        legacy: old.clone(),
        destination: new.clone(),
      })
    })
    .unwrap();
    fs::rename(&old, &new).unwrap();
    assert!(resolve_directory(&old, &new).unwrap_err().contains("incomplete"));
    migrate_directory_with_alias(&old, &new).unwrap();
    assert_eq!(resolve_directory(&old, &new).unwrap(), new);
    assert!(!migration_file(&new).unwrap().exists());
  }

  #[test]
  fn pure_resolution_does_not_generate_state_for_an_unmigrated_bundle() {
    let (_directory, old, new) = fixture();
    assert!(resolve_directory(&old, &new).unwrap_err().contains("Legacy"));
    assert_eq!(directory_for_inspection(&old, &new).unwrap(), old);
    assert!(!new.exists());
  }

  #[test]
  fn malformed_bundle_or_journal_preserves_original_state() {
    let (_directory, old, new) = fixture();
    fs::write(old.join("host.key"), "invalid JSON").unwrap();
    assert!(
      migrate_directory_with_alias(&old, &new)
        .unwrap_err()
        .contains("Invalid private JSON")
    );
    assert_eq!(fs::read_to_string(old.join("host.key")).unwrap(), "invalid JSON");
    assert!(!new.exists());
    private_directory(new.parent().unwrap()).unwrap();
    update_private_json(&migration_file(&new).unwrap(), 8192, |_: Option<serde_json::Value>| {
      Ok(serde_json::json!({
        "version": 99, "legacy": old, "destination": new,
      }))
    })
    .unwrap();
    assert!(
      migrate_directory_with_alias(&old, &new)
        .unwrap_err()
        .contains("different paths")
    );
    assert!(!new.exists());
  }

  #[test]
  fn ownership_lock_names_cannot_escape_bundle() {
    let (_directory, old, new) = fixture();
    assert!(migrate_directory_with_alias_and_locks(&old, &new, &["../escaped.lock"]).is_err());
    assert!(!new.exists());
  }

  #[test]
  fn domain_validation_holds_known_and_existing_locks_before_any_move() {
    let (_directory, old, new) = fixture();
    let error = migrate_directory_with_alias_and_locks_checked(&old, &new, &["connector.lock"], |bundle| {
      assert!(acquire_lock(&bundle.join("connector.lock")).is_err());
      assert!(acquire_lock(&bundle.join("host.key.lock")).is_err());
      Err("Invalid provider identity".into())
    })
    .unwrap_err();
    assert_eq!(error, "Invalid provider identity");
    assert!(old.join("host.key").exists());
    assert!(!new.exists());
    assert!(!migration_file(&new).unwrap().exists());
  }
}

#[cfg(all(test, not(unix)))]
mod non_unix_tests {
  use super::*;

  #[test]
  fn absent_and_native_only_stores_need_no_alias_support() {
    let directory = tempfile::tempdir().unwrap();
    let old = directory.path().join("old/hub");
    let new = directory.path().join("new/hub");
    migrate_directory_with_alias(&old, &new).unwrap();
    assert!(!old.exists());
    assert!(!new.exists());
    fs::create_dir_all(&new).unwrap();
    migrate_directory_with_alias(&old, &new).unwrap();
    fs::create_dir_all(&old).unwrap();
    assert!(
      migrate_directory_with_alias(&old, &new)
        .unwrap_err()
        .contains("Conflicting")
    );
    fs::remove_dir(&new).unwrap();
    assert!(
      migrate_directory_with_alias(&old, &new)
        .unwrap_err()
        .contains("alias support")
    );
  }
}
