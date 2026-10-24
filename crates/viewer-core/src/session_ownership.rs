//! Host-scoped native session ownership shared by every control route.
use sha2::{Digest, Sha256};
use std::{fs::File, path::PathBuf, sync::Arc};
use tokn_interaction_core::Provider;

#[derive(Clone, Debug)]
pub(crate) struct SessionOwnership {
  scope: Arc<Scope>,
}

#[derive(Debug)]
struct Scope {
  directory: PathBuf,
  _temporary: Option<tempfile::TempDir>,
}

/// Clones share one ownership lifetime, not independently unlocked descriptors.
/// The final holder explicitly unlocks before closing the file, including when
/// a concurrent fork temporarily retains the same open file description.
#[derive(Clone, Debug)]
pub(crate) struct SessionLease {
  _inner: Arc<Lease>,
}

#[derive(Debug)]
struct Lease {
  file: File,
  _scope: Arc<Scope>,
}

impl Drop for Lease {
  fn drop(&mut self) {
    let _ = self.file.unlock();
  }
}

impl SessionOwnership {
  /// Resolving a scope never creates persistent directories or takes ownership.
  pub(crate) fn new(directory: Option<PathBuf>) -> Result<Self, String> {
    let (directory, temporary) = match directory {
      Some(directory) => {
        if !directory.is_absolute() {
          return Err("Session ownership requires an absolute host storage directory".into());
        }
        (directory, None)
      }
      None => {
        let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
        (temporary.path().to_path_buf(), Some(temporary))
      }
    };
    Ok(Self {
      scope: Arc::new(Scope {
        directory,
        _temporary: temporary,
      }),
    })
  }

  /// Blocking filesystem admission. Call from a blocking task before any native
  /// resume or mutation, then retain the lease until cleanup is confirmed.
  pub(crate) fn acquire(&self, provider: Provider, native_id: &str) -> Result<SessionLease, String> {
    if native_id.is_empty()
      || native_id.len() > 1024
      || native_id.trim() != native_id
      || native_id.chars().any(char::is_control)
    {
      return Err("Invalid native session ownership identity".into());
    }
    let identity = serde_json::to_vec(&(provider, native_id)).map_err(|_| "Invalid session ownership identity")?;
    let key = format!("{:x}", Sha256::digest(identity));
    let file = tokn_session_storage::acquire_private_lock(&self.scope.directory.join(format!("{key}.lease")))
      .map_err(|error| format!("This native session already has an owner, or ownership cannot be verified: {error}"))?;
    Ok(SessionLease {
      _inner: Arc::new(Lease {
        file,
        _scope: self.scope.clone(),
      }),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ownership_is_shared_across_independent_registries_and_last_clone_releases() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session-leases");
    let first = SessionOwnership::new(Some(path.clone())).unwrap();
    let second = SessionOwnership::new(Some(path.clone())).unwrap();
    assert!(!path.exists(), "Constructing ownership must remain read-only");
    let lease = first.acquire(Provider::Pi, "native-session").unwrap();
    let clone = lease.clone();
    assert!(second.acquire(Provider::Pi, "native-session").is_err());
    drop(lease);
    assert!(second.acquire(Provider::Pi, "native-session").is_err());
    drop(clone);
    assert!(second.acquire(Provider::Pi, "native-session").is_ok());
  }

  #[test]
  fn native_identity_is_provider_scoped_and_never_used_as_a_path() {
    let directory = tempfile::tempdir().unwrap();
    let ownership = SessionOwnership::new(Some(directory.path().to_path_buf())).unwrap();
    let _pi = ownership.acquire(Provider::Pi, "../same/native-id").unwrap();
    let _codex = ownership.acquire(Provider::Codex, "../same/native-id").unwrap();
    let _other = ownership.acquire(Provider::Pi, "../same/other-id").unwrap();
    assert!(ownership.acquire(Provider::Pi, "../same/native-id").is_err());
    let files = std::fs::read_dir(directory.path())
      .unwrap()
      .collect::<Result<Vec<_>, _>>()
      .unwrap();
    assert_eq!(files.len(), 3);
    assert!(files.iter().all(|file| file.file_name().to_string_lossy().len() == 70));
    for id in ["", " leading", "trailing ", "line\nbreak", &"x".repeat(1025)] {
      assert!(ownership.acquire(Provider::Pi, id).is_err());
    }
  }

  #[test]
  fn lease_retains_ephemeral_scope_after_registry_is_dropped() {
    let ownership = SessionOwnership::new(None).unwrap();
    let path = ownership.scope.directory.clone();
    let lease = ownership.acquire(Provider::Pi, "temporary").unwrap();
    drop(ownership);
    assert!(path.exists());
    drop(lease);
    assert!(!path.exists());
  }

  #[cfg(unix)]
  #[test]
  fn final_owner_unlocks_a_fork_shared_file_description() {
    let ownership = SessionOwnership::new(None).unwrap();
    let lease = ownership.acquire(Provider::Pi, "native").unwrap();
    let inherited = lease._inner.file.try_clone().unwrap();
    let other_holder = lease.clone();
    drop(lease);
    assert!(ownership.acquire(Provider::Pi, "native").is_err());
    drop(other_holder);
    let next = ownership.acquire(Provider::Pi, "native").unwrap();
    drop(inherited);
    assert!(ownership.acquire(Provider::Pi, "native").is_err());
    drop(next);
    assert!(ownership.acquire(Provider::Pi, "native").is_ok());
  }
}
