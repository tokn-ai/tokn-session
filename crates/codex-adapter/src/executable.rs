//! Locate the native Codex launcher for CLI and app-server callers.
use std::{
  ffi::OsStr,
  path::{Path, PathBuf},
};
use tokn_session_storage::find_executable;

pub const EXECUTABLE_OVERRIDE: &str = "TOKN_SESSION_CODEX_EXECUTABLE";

pub fn resolve_executable() -> Result<PathBuf, String> {
  resolve_with_environment(
    std::env::var_os(EXECUTABLE_OVERRIDE).as_deref(),
    std::env::var_os("PATH").as_deref(),
    &bundled_executables(),
  )
}

pub fn resolve_with_environment(
  configured: Option<&OsStr>,
  search_path: Option<&OsStr>,
  bundled: &[PathBuf],
) -> Result<PathBuf, String> {
  let path = if let Some(configured) = configured {
    find_executable(Path::new(configured), search_path)
      .ok_or_else(|| format!("The configured Codex executable is unavailable. Check {EXECUTABLE_OVERRIDE}."))?
  } else {
    find_executable(Path::new("codex"), search_path)
      .or_else(|| bundled.iter().find_map(|candidate| find_executable(candidate, None)))
      .ok_or_else(|| format!("Codex is unavailable. Install it or set {EXECUTABLE_OVERRIDE}."))?
  };
  path
    .canonicalize()
    .map_err(|_| "Could not resolve the Codex executable".into())
}

/// Standard app install locations for the current host. Does not inspect them.
pub fn bundled_executables() -> Vec<PathBuf> {
  #[cfg(target_os = "macos")]
  {
    // A native app launched from Finder may have a much shorter PATH than a
    // terminal launched from ChatGPT. Search the app bundle on that machine.
    const LAUNCHER: &str = "ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex";
    let mut candidates = vec![Path::new("/Applications").join(LAUNCHER)];
    if let Some(home) = dirs::home_dir() {
      candidates.push(home.join("Applications").join(LAUNCHER));
    }
    candidates
  }
  #[cfg(not(target_os = "macos"))]
  Vec::new()
}

#[cfg(all(unix, test))]
mod tests {
  use super::*;
  use std::{fs, os::unix::fs::PermissionsExt};

  fn executable(path: &Path) -> PathBuf {
    fs::write(path, b"fixture").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    path.canonicalize().unwrap()
  }

  #[test]
  fn prefers_configured_then_path_then_bundled_codex() {
    let directory = tempfile::tempdir().unwrap();
    let from_path = executable(&directory.path().join("codex"));
    let configured = executable(&directory.path().join("Codex with spaces"));
    let bundled = executable(&directory.path().join("bundled codex"));
    let path = Some(directory.path().as_os_str());
    let fallback = std::slice::from_ref(&bundled);

    assert_eq!(
      resolve_with_environment(Some(configured.as_os_str()), path, fallback).unwrap(),
      configured
    );
    assert_eq!(resolve_with_environment(None, path, fallback).unwrap(), from_path);
    assert_eq!(resolve_with_environment(None, None, fallback).unwrap(), bundled);
    assert!(
      resolve_with_environment(Some(OsStr::new("missing")), path, fallback)
        .unwrap_err()
        .contains(EXECUTABLE_OVERRIDE)
    );
  }

  #[test]
  fn ignores_incomplete_and_non_executable_bundle_paths() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let not_executable = directory.path().join("not-executable");
    fs::write(&not_executable, b"fixture").unwrap();
    fs::set_permissions(&not_executable, fs::Permissions::from_mode(0o600)).unwrap();
    let bundled = executable(&directory.path().join("codex"));
    assert_eq!(
      resolve_with_environment(None, None, &[missing, not_executable, bundled.clone()]).unwrap(),
      bundled
    );
  }
}
