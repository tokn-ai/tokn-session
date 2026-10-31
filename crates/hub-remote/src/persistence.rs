//! Bounded private JSON persistence shared by native client and host state.
//!
//! Callers hold the matching ownership lock while reading and replacing state.
use serde::{Serialize, de::DeserializeOwned};
use std::{
  fs::{self, File, OpenOptions},
  io::{Read, Write},
  path::{Path, PathBuf},
};
use uuid::Uuid;
use zeroize::Zeroizing;

const MAX_STATE: u64 = 256 * 1024;

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

pub fn read_required<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
  read_optional(path)?.ok_or_else(|| format!("Required private state is missing: {}", path.display()))
}

pub(crate) fn nofollow(options: &mut OpenOptions) {
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
}

pub(crate) fn reject_symlink(path: &Path) -> Result<(), String> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_symlink() => Err("Private state cannot be a symlink".into()),
    Ok(_) => Ok(()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(format!("Could not inspect private state: {error}")),
  }
}

pub(crate) fn validate_private(file: &File) -> Result<(), String> {
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

/// Inspect an existing ownership lock without creating state or conflating I/O failures with contention.
pub fn lock_held(path: &Path) -> Result<bool, String> {
  let mut name = path.as_os_str().to_owned();
  name.push(".lock");
  let lock_path = PathBuf::from(name);
  reject_symlink(&lock_path)?;
  let mut options = OpenOptions::new();
  options.read(true).write(true);
  nofollow(&mut options);
  let file = match options.open(&lock_path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
    Err(error) => return Err(format!("Could not inspect host ownership: {error}")),
  };
  validate_private(&file)?;
  match file.try_lock() {
    Ok(()) => Ok(false),
    Err(std::fs::TryLockError::WouldBlock) => Ok(true),
    Err(std::fs::TryLockError::Error(error)) => Err(format!("Could not inspect host ownership: {error}")),
  }
}

pub fn lock(path: &Path) -> Result<File, String> {
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

pub fn write_atomic(path: &Path, value: &impl Serialize) -> Result<(), String> {
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
