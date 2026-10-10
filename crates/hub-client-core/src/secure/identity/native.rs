//! Native private-file persistence, excluded from browser/WASM builds.
use crate::protocol::{decode, encode};
use serde::{Deserialize, Serialize};
use std::{
  fs::{self, File, OpenOptions},
  io::{Read, Write},
  path::Path,
};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretFile {
  version: u8,
  kind: String,
  secret_key: String,
}

impl Drop for SecretFile {
  fn drop(&mut self) {
    use zeroize::Zeroize;
    self.secret_key.zeroize();
  }
}

pub(super) fn load_or_create_secret(
  path: &Path,
  kind: &str,
  generate: impl FnOnce() -> Result<[u8; 32], String>,
) -> Result<Zeroizing<[u8; 32]>, String> {
  if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
    fs::create_dir_all(parent).map_err(|e| format!("Could not create identity directory: {e}"))?;
  }
  // create_new is exclusive and rejects a pre-existing symlink. On reads,
  // O_NOFOLLOW closes the final-component symlink inspection/open race.
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
  }
  match options.open(path) {
    Ok(mut file) => {
      let secret = Zeroizing::new(generate()?);
      let encoded = Zeroizing::new(
        serde_json::to_vec(&SecretFile {
          version: 1,
          kind: kind.into(),
          secret_key: encode(secret.as_ref()),
        })
        .map_err(|e| e.to_string())?,
      );
      file
        .write_all(&encoded)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("Could not save private identity: {e}"))?;
      Ok(secret)
    }
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_secret(path, kind),
    Err(error) => Err(format!("Could not create private identity: {error}")),
  }
}

pub(super) fn read_secret(path: &Path, kind: &str) -> Result<Zeroizing<[u8; 32]>, String> {
  let metadata = fs::symlink_metadata(path).map_err(|e| format!("Could not inspect private identity: {e}"))?;
  if !metadata.is_file() {
    return Err("Private identity must be a regular file, not a symlink".into());
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let mut file = options
    .open(path)
    .map_err(|e| format!("Could not open private identity: {e}"))?;
  validate_private_file(&file)?;
  let mut bytes = Zeroizing::new(Vec::new());
  (&mut file)
    .take(4097)
    .read_to_end(&mut bytes)
    .map_err(|e| format!("Could not read private identity: {e}"))?;
  if bytes.len() > 4096 {
    return Err("Private identity file is too large".into());
  }
  let stored: SecretFile = serde_json::from_slice(&bytes).map_err(|_| "Invalid private identity file")?;
  if stored.version != 1 || stored.kind != kind {
    return Err("Private identity has the wrong version or key kind".into());
  }
  let secret = Zeroizing::new(decode(&stored.secret_key, 32)?);
  Ok(Zeroizing::new(
    secret
      .as_slice()
      .try_into()
      .map_err(|_| "Invalid private identity key length")?,
  ))
}

fn validate_private_file(file: &File) -> Result<(), String> {
  let metadata = file
    .metadata()
    .map_err(|e| format!("Could not inspect opened private identity: {e}"))?;
  if !metadata.is_file() || metadata.len() > 4096 {
    return Err("Private identity must be a small regular file".into());
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if metadata.mode() & 0o777 != 0o600 || metadata.nlink() != 1 {
      return Err("Private identity must have mode 0600 and no hard links".into());
    }
    // SAFETY: geteuid has no arguments, allocation, or memory side effects.
    if metadata.uid() != unsafe { libc::geteuid() } {
      return Err("Private identity must be owned by the current user".into());
    }
  }
  Ok(())
}
