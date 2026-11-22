use serde::{Serialize, de::DeserializeOwned};
use std::{
  fs::{self, File, OpenOptions},
  io::{Read, Write},
  path::{Path, PathBuf},
};

pub(crate) fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
  match fs::symlink_metadata(path) {
    Ok(metadata) => Ok(Some(metadata)),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
    Err(error) => Err(format!("Could not inspect {}: {error}", path.display())),
  }
}

pub(crate) fn validate_file(file: &File) -> Result<(), String> {
  let metadata = file.metadata().map_err(|error| error.to_string())?;
  if !metadata.is_file() {
    return Err("Private storage must be a regular file".into());
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no memory side effects.
    if metadata.mode() & 0o777 != 0o600 || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
      return Err("Private storage must be owned by the current user, mode 0600, and have no hard links".into());
    }
  }
  Ok(())
}

pub(crate) fn open_private(path: &Path, create: bool) -> Result<Option<File>, String> {
  open_private_with_validation(path, create, |_| Ok(()))
}

const MAX_READ_REOPENS: usize = 2;

// The boundary callback makes atomic replacement deterministic in unit tests;
// production always uses the no-op above and still validates every opened file.
fn open_private_with_validation(
  path: &Path,
  create: bool,
  mut before_validation: impl FnMut(&File) -> Result<(), String>,
) -> Result<Option<File>, String> {
  for attempt in 0..=MAX_READ_REOPENS {
    if metadata(path)?.is_some_and(|metadata| metadata.file_type().is_symlink()) {
      return Err(format!("Private storage cannot be a symlink: {}", path.display()));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create).truncate(false);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
      Ok(file) => file,
      Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
      Err(error) => return Err(format!("Could not open {}: {error}", path.display())),
    };
    before_validation(&file)?;
    if let Err(error) = validate_file(&file) {
      // An atomic JSON replacement can unlink the old inode after open but
      // before fstat. Discard that descriptor; never relax its link check.
      // Lock creation must not reopen an unlinked lock file.
      if !create && attempt < MAX_READ_REOPENS && replaced_unlinked_file(path, &file)? {
        continue;
      }
      return Err(error);
    }
    return Ok(Some(file));
  }
  unreachable!("the final open either validates or returns its error")
}

fn replaced_unlinked_file(path: &Path, file: &File) -> Result<bool, String> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    // Qualify only the unlink race, never unsafe modes, owners or hard links.
    if !opened.is_file()
      || opened.nlink() != 0
      || opened.mode() & 0o777 != 0o600
      || opened.uid() != unsafe { libc::geteuid() }
    {
      return Ok(false);
    }
    let Some(current) = metadata(path)? else {
      return Ok(false);
    };
    Ok(current.dev() != opened.dev() || current.ino() != opened.ino())
  }
  #[cfg(not(unix))]
  {
    let _ = (path, file);
    Ok(false)
  }
}

pub(crate) fn private_directory(path: &Path) -> Result<(), String> {
  match metadata(path)? {
    Some(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
      return Err(format!("Private storage requires a real directory: {}", path.display()));
    }
    Some(metadata) => {
      #[cfg(unix)]
      {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no memory side effects.
        if metadata.uid() != unsafe { libc::geteuid() } {
          return Err(format!(
            "Private directory is not owned by the current user: {}",
            path.display()
          ));
        }
      }
      let _ = metadata;
    }
    None => {
      if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        // Only create missing parents; do not change unrelated existing ancestor modes.
        if metadata(parent)?.is_none() {
          private_directory(parent)?;
        }
      }
      let mut builder = fs::DirBuilder::new();
      #[cfg(unix)]
      {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
      }
      match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return private_directory(path),
        Err(error) => return Err(format!("Could not create {}: {error}", path.display())),
      }
    }
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())?;
  }
  Ok(())
}

pub(crate) fn acquire_lock(path: &Path) -> Result<File, String> {
  let file = open_private(path, true)?.ok_or("Could not create storage lock")?;
  file
    .try_lock()
    .map_err(|error| format!("Storage is in use; stop the owning process and retry: {error}"))?;
  Ok(file)
}

pub(crate) fn sync_directory(path: &Path) -> Result<(), String> {
  #[cfg(unix)]
  File::open(path)
    .and_then(|directory| directory.sync_all())
    .map_err(|error| format!("Could not sync {}: {error}", path.display()))?;
  Ok(())
}

fn lock_path(path: &Path) -> PathBuf {
  let mut name = path.as_os_str().to_owned();
  name.push(".lock");
  name.into()
}

struct TransactionLock(File);

impl Drop for TransactionLock {
  fn drop(&mut self) {
    // A concurrent process spawn can inherit this open file description until
    // exec. Explicitly unlock so that its descriptor cannot extend a transaction.
    let _ = self.0.unlock();
  }
}

/// Read bounded private JSON without creating files or directories.
pub fn read_private_json<T: DeserializeOwned>(path: &Path, max_bytes: u64) -> Result<Option<T>, String> {
  let Some(file) = open_private(path, false)? else {
    return Ok(None);
  };
  let mut bytes = Vec::new();
  file
    .take(max_bytes.saturating_add(1))
    .read_to_end(&mut bytes)
    .map_err(|error| error.to_string())?;
  if bytes.len() as u64 > max_bytes {
    return Err(format!("Private JSON exceeds its size limit: {}", path.display()));
  }
  serde_json::from_slice(&bytes).map(Some).map_err(|_| {
    format!(
      "Invalid private JSON {}; restore it instead of replacing it",
      path.display()
    )
  })
}

/// Validate and update bounded JSON while holding its cross-process ownership lock.
pub fn update_private_json<T: Serialize + DeserializeOwned>(
  path: &Path,
  max_bytes: u64,
  update: impl FnOnce(Option<T>) -> Result<T, String>,
) -> Result<T, String> {
  let parent = path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .ok_or("Private JSON needs a parent")?;
  private_directory(parent)?;
  let _lock = TransactionLock(acquire_lock(&lock_path(path))?);
  let value = update(read_private_json(path, max_bytes)?)?;
  let bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
  if bytes.len() as u64 > max_bytes {
    return Err("Updated private JSON exceeds its size limit".into());
  }
  let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
  temporary
    .write_all(&bytes)
    .and_then(|_| temporary.as_file().sync_all())
    .map_err(|error| error.to_string())?;
  temporary
    .persist(path)
    .map_err(|error| format!("Could not save private JSON: {}", error.error))?;
  sync_directory(parent)?;
  Ok(value)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[cfg(unix)]
  fn replace_private(path: &Path, bytes: &[u8]) {
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    temporary.write_all(bytes).unwrap();
    temporary.persist(path).unwrap();
  }

  #[cfg(unix)]
  #[test]
  fn atomic_replacement_between_open_and_validation_reopens_only_the_fresh_private_file() {
    use std::os::unix::fs::MetadataExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("preferences.json");
    update_private_json(&path, 128, |_: Option<Vec<u32>>| Ok(vec![1])).unwrap();
    let mut opens = 0;
    let file = open_private_with_validation(&path, false, |opened| {
      opens += 1;
      if opens == 1 {
        replace_private(&path, b"[2]");
        assert_eq!(opened.metadata().unwrap().nlink(), 0);
        assert!(
          validate_file(opened).is_err(),
          "unlinked descriptors must still fail strict validation"
        );
      }
      Ok(())
    })
    .unwrap()
    .unwrap();
    assert_eq!(opens, 2);
    validate_file(&file).unwrap();
    assert_eq!(serde_json::from_reader::<_, Vec<u32>>(file).unwrap(), vec![2]);
  }

  #[cfg(unix)]
  #[test]
  fn replacement_reopens_are_bounded_and_never_reacquire_creation_or_lock_descriptions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("preferences.json");
    for create in [false, true] {
      replace_private(&path, b"[1]");
      let mut opens = 0;
      let error = open_private_with_validation(&path, create, |_| {
        opens += 1;
        replace_private(&path, b"[2]");
        Ok(())
      })
      .unwrap_err();
      assert!(error.contains("mode 0600, and have no hard links"));
      assert_eq!(opens, if create { 1 } else { MAX_READ_REOPENS + 1 });
    }
  }

  #[cfg(unix)]
  #[test]
  fn replacement_cannot_hide_unsafe_original_modes_or_new_modes_links_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for fault in [
      "original_mode",
      "replacement_mode",
      "replacement_hard_link",
      "replacement_symlink",
    ] {
      let directory = tempfile::tempdir().unwrap();
      let path = directory.path().join("preferences.json");
      replace_private(&path, b"[1]");
      if fault == "original_mode" {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
      }
      let mut opens = 0;
      assert!(
        open_private_with_validation(&path, false, |_| {
          opens += 1;
          if opens == 1 {
            replace_private(&path, b"[2]");
            match fault {
              "replacement_mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
              "replacement_hard_link" => fs::hard_link(&path, directory.path().join("linked.json")).unwrap(),
              "replacement_symlink" => {
                let target = directory.path().join("target.json");
                fs::rename(&path, &target).unwrap();
                symlink(target, &path).unwrap();
              }
              _ => {}
            }
          }
          Ok(())
        })
        .is_err(),
        "{fault}"
      );
      if fault == "original_mode" {
        assert_eq!(opens, 1, "unsafe original modes must not qualify for reopening");
      }
    }
  }

  #[test]
  fn transaction_unlocks_while_an_inherited_description_remains_open() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("transaction.lock");
    let transaction = TransactionLock(acquire_lock(&path).unwrap());
    let inherited = transaction.0.try_clone().unwrap();
    assert!(acquire_lock(&path).is_err());
    drop(transaction);
    let next = acquire_lock(&path).unwrap();
    assert!(acquire_lock(&path).is_err());
    drop(next);
    drop(inherited);
  }

  #[test]
  fn an_early_transaction_error_releases_inherited_descriptions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("transaction.lock");
    let mut inherited = None;
    let result = (|| -> Result<(), String> {
      let transaction = TransactionLock(acquire_lock(&path)?);
      inherited = Some(transaction.0.try_clone().map_err(|error| error.to_string())?);
      Err("Update failed".into())
    })();
    assert_eq!(result.unwrap_err(), "Update failed");
    assert!(acquire_lock(&path).is_ok());
    drop(inherited);
  }

  #[test]
  fn missing_read_is_pure_and_updates_preserve_current_valid_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("viewer/preferences.json");
    assert_eq!(read_private_json::<Vec<u32>>(&path, 128).unwrap(), None);
    assert!(!path.parent().unwrap().exists());
    update_private_json(&path, 128, |_: Option<Vec<u32>>| Ok(vec![1])).unwrap();
    let value = update_private_json(&path, 128, |previous: Option<Vec<u32>>| {
      let mut value = previous.unwrap();
      value.push(2);
      Ok(value)
    })
    .unwrap();
    assert_eq!(value, vec![1, 2]);
  }

  #[test]
  fn invalid_or_oversized_json_is_never_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("preferences.json");
    update_private_json(&path, 128, |_: Option<Vec<u32>>| Ok(vec![1])).unwrap();
    fs::write(&path, "broken").unwrap();
    assert!(update_private_json(&path, 128, |_: Option<Vec<u32>>| Ok(vec![2])).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "broken");
    fs::write(&path, "[1,2,3]").unwrap();
    assert!(read_private_json::<Vec<u32>>(&path, 2).is_err());
  }

  #[cfg(unix)]
  #[test]
  fn exposed_linked_or_symlinked_files_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("preferences.json");
    update_private_json(&path, 128, |_: Option<Vec<u32>>| Ok(vec![1])).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_private_json::<Vec<u32>>(&path, 128).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let linked = directory.path().join("linked.json");
    fs::hard_link(&path, &linked).unwrap();
    assert!(read_private_json::<Vec<u32>>(&path, 128).is_err());
    fs::remove_file(linked).unwrap();
    let alias = directory.path().join("alias.json");
    symlink(&path, &alias).unwrap();
    assert!(read_private_json::<Vec<u32>>(&alias, 128).is_err());
  }
}
