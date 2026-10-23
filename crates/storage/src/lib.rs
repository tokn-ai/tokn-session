//! Shared native paths and conservative migrations. Path discovery never migrates data.
use std::path::{Path, PathBuf};

mod migration;
pub use migration::{
  directory_for_inspection, migrate_directory_with_alias, migrate_directory_with_alias_and_locks,
  migrate_directory_with_alias_and_locks_checked, resolve_directory,
};
mod private;
pub use private::{read_private_json, update_private_json};

/// Hold a private cross-process lease until the returned file is dropped.
pub fn acquire_private_lock(path: &Path) -> Result<std::fs::File, String> {
  let parent = path.parent().ok_or("Private lock needs a parent directory")?;
  private::private_directory(parent)?;
  private::acquire_lock(path)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPaths {
  home: PathBuf,
}

impl SessionPaths {
  pub fn from_home(home: impl AsRef<Path>) -> Self {
    Self {
      home: home.as_ref().to_owned(),
    }
  }

  pub fn discover() -> Result<Self, String> {
    dirs::home_dir()
      .map(Self::from_home)
      .ok_or_else(|| "Cannot resolve home directory; specify an explicit storage path".into())
  }

  pub fn root(&self) -> PathBuf {
    self.home.join(".tokn/sessions")
  }

  pub fn index_path(&self) -> PathBuf {
    self.root().join("index.sqlite")
  }

  pub fn hub_dir(&self) -> PathBuf {
    self.root().join("hub")
  }

  pub fn viewer_dir(&self) -> PathBuf {
    self.root().join("viewer")
  }

  pub fn desktop_client_dir(&self) -> PathBuf {
    self.hub_dir().join("desktop-client")
  }

  pub fn legacy_hub_dir(&self) -> PathBuf {
    self.home.join(".tokn/hub")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn all_paths_share_the_native_root_without_creating_it() {
    let home = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(home.path());
    assert_eq!(paths.root(), home.path().join(".tokn/sessions"));
    assert_eq!(paths.index_path(), paths.root().join("index.sqlite"));
    assert_eq!(paths.hub_dir(), paths.root().join("hub"));
    assert_eq!(paths.viewer_dir(), paths.root().join("viewer"));
    assert_eq!(paths.desktop_client_dir(), paths.hub_dir().join("desktop-client"));
    assert!(!paths.root().exists());
  }
}
