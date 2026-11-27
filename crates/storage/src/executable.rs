use std::{
  ffi::OsStr,
  path::{Path, PathBuf},
};

/// Locate an executable by absolute path or on absolute entries in a supplied
/// search path. This checks file metadata without launching a process.
pub fn find_executable(command: &Path, search_path: Option<&OsStr>) -> Option<PathBuf> {
  if command.is_absolute() {
    executable_candidates(command).find(|candidate| executable_ready(candidate))
  } else {
    let search_path = search_path?;
    std::env::split_paths(search_path)
      .filter(|directory| directory.is_absolute())
      .flat_map(|directory| executable_candidates(&directory.join(command)))
      .find(|candidate| executable_ready(candidate))
  }
}

fn executable_candidates(command: &Path) -> std::vec::IntoIter<PathBuf> {
  let mut candidates = vec![command.to_path_buf()];
  // Windows package managers can expose shims without an explicit extension
  // in configuration. Keep an explicitly supplied extension exact.
  if cfg!(windows) && command.extension().is_none() {
    candidates.extend(["exe", "com", "cmd", "bat"].map(|extension| command.with_extension(extension)));
  }
  candidates.into_iter()
}

fn executable_ready(path: &Path) -> bool {
  let Ok(metadata) = path.metadata() else { return false };
  if !metadata.is_file() {
    return false;
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
      return false;
    }
  }
  true
}
