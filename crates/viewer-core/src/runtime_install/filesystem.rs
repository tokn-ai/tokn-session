use super::*;
use std::{
  fs::{self, File, OpenOptions},
  io::{Read, Write},
  path::Component,
};

const RECORD: &str = "tokn-install.json";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallRecord {
  pub provider: Provider,
  pub version: String,
  pub generation: String,
  /// Relative native executable or JavaScript entrypoint within this install.
  pub entrypoint: PathBuf,
  pub node: bool,
  pub digest: String,
}
#[derive(Clone, Debug)]
pub struct RuntimeLaunchSpec {
  pub executable: PathBuf,
  pub arguments: Vec<String>,
  pub generation: String,
}
/// Shared, process-wide software use lease. Keep alive until its child exits.
#[derive(Debug)]
pub struct RuntimeUseGuard {
  _file: File,
}

pub(super) fn provider_name(provider: Provider) -> &'static str {
  match provider {
    Provider::Pi => "pi",
    Provider::Codex => "codex",
    Provider::OpenCode => "opencode",
    Provider::Dsh => "dsh",
  }
}
fn real_directory(path: &Path) -> Result<(), String> {
  if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink() || !m.is_dir()) {
    return Err("Agent storage must be a real directory".into());
  }
  fs::create_dir_all(path).map_err(|_| "Could not create agent storage")?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| "Could not protect agent storage")?;
  }
  Ok(())
}
fn lock_file(root: &Path, provider: Provider) -> Result<File, String> {
  real_directory(root)?;
  let locks = root.join(".locks");
  real_directory(&locks)?;
  let path = locks.join(format!("{}.lock", provider_name(provider)));
  if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink() || !m.is_file()) {
    return Err("Invalid agent installation lock".into());
  }
  let mut options = OpenOptions::new();
  options.create(true).read(true).write(true).truncate(false);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
  }
  options
    .open(path)
    .map_err(|_| "Could not open the agent installation lock".into())
}
pub(super) fn exclusive_lock(root: &Path, provider: Provider) -> Result<File, String> {
  let file = lock_file(root, provider)?;
  file
    .try_lock()
    .map_err(|_| "This agent installation is in use or another installation is running; stop its runtimes and retry")?;
  Ok(file)
}
pub(super) fn in_use(root: &Path, provider: Provider) -> Result<bool, String> {
  let file = lock_file(root, provider)?;
  match file.try_lock() {
    Ok(()) => Ok(false),
    Err(std::fs::TryLockError::WouldBlock) => Ok(true),
    Err(std::fs::TryLockError::Error(_)) => Err("Could not inspect the agent installation lock".into()),
  }
}
pub fn acquire_use(root: &Path, provider: Provider, generation: &str) -> Result<RuntimeUseGuard, String> {
  let file = lock_file(root, provider)?;
  file
    .try_lock_shared()
    .map_err(|_| "This agent is being updated; retry after the installation finishes")?;
  let current = read_record(root, provider)?.ok_or("The managed agent was removed; refresh the runtime selection")?;
  if current.generation != generation {
    return Err("The managed agent changed; refresh the runtime selection".into());
  }
  Ok(RuntimeUseGuard { _file: file })
}
pub fn launch_spec(root: &Path, provider: Provider) -> Result<Option<RuntimeLaunchSpec>, String> {
  match fs::symlink_metadata(root) {
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
    _ => return Err("Agent storage must be a real directory".into()),
  }
  if fs::symlink_metadata(root.join(provider_name(provider))).is_err() && previous(root, provider).exists() {
    let _lease = exclusive_lock(root, provider)?;
    recover(root, provider)?;
  }
  // An update briefly has no active directory between renames. A shared read
  // lease prevents that gap from being mistaken for an absent installation.
  let read_lease = lock_file(root, provider)?;
  read_lease
    .try_lock_shared()
    .map_err(|_| "This agent is being updated; retry after the installation finishes")?;
  let Some(record) = read_record(root, provider)? else {
    return Ok(None);
  };
  let directory = root.join(provider_name(provider));
  validate_staged(&directory, &record)?;
  let entrypoint = directory.join(&record.entrypoint);
  let (executable, arguments) = if record.node {
    let tools = super::package::node_tools()?;
    (
      tools.node,
      vec![
        entrypoint
          .to_str()
          .ok_or("Agent entrypoint must have a Unicode path")?
          .to_owned(),
      ],
    )
  } else {
    (entrypoint, Vec::new())
  };
  Ok(Some(RuntimeLaunchSpec {
    executable,
    arguments,
    generation: record.generation,
  }))
}
pub(super) fn read_record(root: &Path, provider: Provider) -> Result<Option<InstallRecord>, String> {
  let directory = root.join(provider_name(provider));
  let metadata = match fs::symlink_metadata(&directory) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(_) => return Err("Could not inspect agent installation".into()),
  };
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err("Invalid managed agent installation directory".into());
  }
  let path = directory.join(RECORD);
  if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
    return Err("Invalid managed agent installation record".into());
  }
  let mut data = Vec::new();
  File::open(path)
    .map_err(|_| "Agent installation metadata is missing; use Update to repair it")?
    .take(64 * 1024 + 1)
    .read_to_end(&mut data)
    .map_err(|_| "Could not read agent installation metadata")?;
  if data.len() > 64 * 1024 {
    return Err("Agent installation metadata is too large".into());
  }
  let record: InstallRecord =
    serde_json::from_slice(&data).map_err(|_| "Agent installation metadata is invalid; use Update to repair it")?;
  if record.provider != provider
    || uuid::Uuid::parse_str(&record.generation).is_err()
    || record.version.is_empty()
    || record.version.len() > 128
  {
    return Err("Agent installation identity is invalid".into());
  }
  Ok(Some(record))
}
pub(super) fn write_record(directory: &Path, record: &InstallRecord) -> Result<(), String> {
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let mut file = options
    .open(directory.join(RECORD))
    .map_err(|_| "Could not create installation metadata")?;
  file
    .write_all(&serde_json::to_vec(record).map_err(|_| "Could not encode installation metadata")?)
    .map_err(|_| "Could not save installation metadata")?;
  file
    .sync_all()
    .map_err(|_| "Could not persist installation metadata".to_owned())
}
pub(super) fn relative_path(path: &Path) -> bool {
  !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}
pub(super) fn validate_staged(directory: &Path, record: &InstallRecord) -> Result<(), String> {
  if !relative_path(&record.entrypoint) {
    return Err("Agent entrypoint escapes its installation".into());
  }
  let root = directory
    .canonicalize()
    .map_err(|_| "Agent installation directory is unavailable")?;
  let path = directory
    .join(&record.entrypoint)
    .canonicalize()
    .map_err(|_| "Agent entrypoint was not installed")?;
  if !path.starts_with(root) || !path.is_file() {
    return Err("Invalid agent entrypoint".into());
  }
  if !record.node {
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      if path
        .metadata()
        .map_err(|_| "Could not inspect agent entrypoint")?
        .permissions()
        .mode()
        & 0o111
        == 0
      {
        return Err("Agent entrypoint is not executable".into());
      }
    }
  }
  Ok(())
}
fn previous(root: &Path, provider: Provider) -> PathBuf {
  root.join(format!(".{}-previous", provider_name(provider)))
}
/// A crash between the two renames restores the previous complete installation.
/// All callers hold the exclusive provider lock, including recovery after restart.
pub(super) fn recover(root: &Path, provider: Provider) -> Result<(), String> {
  let backup = previous(root, provider);
  let active = root.join(provider_name(provider));
  if fs::symlink_metadata(&backup).is_ok_and(|m| m.file_type().is_symlink() || !m.is_dir()) {
    return Err("Invalid agent update recovery directory".into());
  }
  if backup.exists() {
    if !active.exists() {
      fs::rename(&backup, &active).map_err(|_| "Could not recover the previous agent installation")?;
    } else {
      fs::remove_dir_all(backup).map_err(|_| "Could not finish previous agent update cleanup")?;
    }
  }
  Ok(())
}
pub(super) fn replace(root: &Path, provider: Provider, staged: &Path) -> Result<(), String> {
  recover(root, provider)?;
  let active = root.join(provider_name(provider));
  let backup = previous(root, provider);
  if fs::symlink_metadata(&active).is_ok_and(|m| m.file_type().is_symlink() || !m.is_dir()) {
    return Err("Invalid agent installation directory".into());
  }
  let existed = active.exists();
  if existed {
    fs::rename(&active, &backup).map_err(|_| "Could not stage replacement of the current agent installation")?;
  }
  if fs::rename(staged, &active).is_err() {
    if existed {
      fs::rename(&backup, &active)
        .map_err(|_| "Agent update failed; previous installation awaits recovery on retry")?;
    }
    return Err("Could not activate the new agent installation; previous installation was retained".into());
  }
  // A leftover backup is harmless and recovered under the same lock next time.
  if existed {
    let _ = fs::remove_dir_all(backup);
  }
  Ok(())
}
