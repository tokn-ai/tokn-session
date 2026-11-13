//! Lazy native artifact reads; history and display projections retain opaque URIs.
use std::{
  ffi::OsStr,
  fs::File,
  io::Read,
  path::{Path, PathBuf},
};

const MAX_ENTRIES: usize = 4096;
const MAX_ENCODED_BYTES: usize = super::MAX_IMAGE_BYTES.div_ceil(3) * 4 + 64;

#[cfg(unix)]
type Directory = File;
#[cfg(not(unix))]
struct Directory;

pub(crate) fn matches_reference(reference: &str, session_id: &str) -> bool {
  artifact_id(reference, session_id).is_some()
}

fn artifact_id<'a>(reference: &'a str, session_id: &str) -> Option<&'a str> {
  let (owner, artifact) = reference.strip_prefix("zcode-artifact://")?.split_once('/')?;
  let valid = |value: &str| {
    !value.is_empty()
      && value.len() <= 120
      && value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
  };
  // No decoding, case folding or native path sanitization: ownership must be
  // exact, and both URI components must already be safe native identifiers.
  (owner == session_id && valid(owner) && valid(artifact)).then_some(artifact)
}

pub(crate) fn read_artifact(database: &Path, session_id: &str, reference: &str) -> Result<Vec<u8>, String> {
  let artifact = artifact_id(reference, session_id).ok_or("ZCode image does not match its session owner")?;
  let cli = native_cli_directory(database)?;
  let artifacts = cli.join("artifacts");
  let session = artifacts.join(session_id);
  for directory in [&artifacts, &session] {
    let metadata = std::fs::symlink_metadata(directory).map_err(|_| "ZCode image storage is unavailable")?;
    if !metadata.is_dir()
      || metadata.file_type().is_symlink()
      || directory.canonicalize().ok().as_ref() != Some(directory)
    {
      return Err("ZCode image storage is outside its native directory".into());
    }
  }
  // Hold directory handles before scanning. Unix opens remain relative to the
  // verified owner even if another process moves a directory during the read.
  let directory = open_session_directory(&cli, session_id, &session)?;
  let suffix = format!("-{artifact}.txt");
  let mut matching = None;
  for (index, entry) in std::fs::read_dir(&session)
    .map_err(|_| "Could not inspect ZCode image storage")?
    .enumerate()
  {
    if index >= MAX_ENTRIES {
      return Err("ZCode image storage exceeds the directory limit".into());
    }
    let name = entry.map_err(|_| "Could not inspect ZCode image storage")?.file_name();
    if name.as_encoded_bytes().ends_with(suffix.as_bytes()) && matching.replace(name).is_some() {
      return Err("ZCode image artifact identity is ambiguous".into());
    }
  }
  let name = matching.ok_or("ZCode image artifact is unavailable")?;
  let file = open_artifact(&directory, &session, &name)?;
  let metadata = file.metadata().map_err(|_| "Could not inspect ZCode image artifact")?;
  if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_ENCODED_BYTES as u64 {
    return Err("ZCode image artifact exceeds the size limit or is unavailable".into());
  }
  let mut bytes = Vec::new();
  file
    .take((MAX_ENCODED_BYTES + 1) as u64)
    .read_to_end(&mut bytes)
    .map_err(|_| "Could not read ZCode image artifact")?;
  if bytes.len() > MAX_ENCODED_BYTES {
    return Err("ZCode image artifact exceeds the size limit".into());
  }
  let data = std::str::from_utf8(&bytes).map_err(|_| "ZCode image artifact is not a data URL")?;
  super::read_data_url(data)
}

fn native_cli_directory(database: &Path) -> Result<PathBuf, String> {
  let db = database.parent().ok_or("ZCode image database layout is unavailable")?;
  let cli = db.parent().ok_or("ZCode image database layout is unavailable")?;
  if !database.is_absolute()
    || database.file_name() != Some(OsStr::new("db.sqlite"))
    || db.file_name() != Some(OsStr::new("db"))
    || cli.file_name() != Some(OsStr::new("cli"))
  {
    return Err("ZCode image database layout is unavailable".into());
  }
  let cli = cli
    .canonicalize()
    .map_err(|_| "ZCode image database layout is unavailable")?;
  if database.canonicalize().ok().as_ref() != Some(&cli.join("db/db.sqlite")) {
    // Custom database overrides do not determine native artifact storage.
    return Err("ZCode image database layout is unavailable".into());
  }
  Ok(cli)
}

#[cfg(unix)]
fn open_session_directory(cli: &Path, session_id: &str, _: &Path) -> Result<Directory, String> {
  use std::os::unix::fs::OpenOptionsExt;
  let cli = std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
    .open(cli)
    .map_err(|_| "Could not open ZCode image storage")?;
  let artifacts = open_child(&cli, OsStr::new("artifacts"), true)?;
  open_child(&artifacts, OsStr::new(session_id), true)
}

#[cfg(unix)]
fn open_child(directory: &File, name: &OsStr, is_directory: bool) -> Result<File, String> {
  use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::ffi::OsStrExt,
  };
  let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| "Invalid ZCode image artifact name")?;
  let flags = libc::O_RDONLY
    | libc::O_CLOEXEC
    | libc::O_NOFOLLOW
    | libc::O_NONBLOCK
    | if is_directory { libc::O_DIRECTORY } else { 0 };
  // Both native directory components and the scanned filename are single
  // components. O_NOFOLLOW refuses symlinks at every step below the root.
  let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
  if descriptor < 0 {
    return Err("Could not open ZCode image artifact".into());
  }
  Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn open_artifact(directory: &Directory, _: &Path, name: &OsStr) -> Result<File, String> {
  open_child(directory, name, false)
}

#[cfg(not(unix))]
fn open_session_directory(_: &Path, _: &str, _: &Path) -> Result<Directory, String> {
  Ok(Directory)
}

#[cfg(not(unix))]
fn open_artifact(_: &Directory, session: &Path, name: &OsStr) -> Result<File, String> {
  let path = session.join(name);
  let metadata = std::fs::symlink_metadata(&path).map_err(|_| "Could not inspect ZCode image artifact")?;
  if !metadata.is_file()
    || metadata.file_type().is_symlink()
    || path.canonicalize().ok().as_deref().and_then(Path::parent) != Some(session)
  {
    return Err("ZCode image artifact is outside its native directory".into());
  }
  File::open(path).map_err(|_| "Could not open ZCode image artifact".into())
}

#[cfg(test)]
mod tests;
