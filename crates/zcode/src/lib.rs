use std::path::PathBuf;

use tokn_session_core::{LoadedSession, SessionHeader, SessionHeaderCatalog, SessionRef};
use tokn_session_opencode::OpenCodeSessionSource;

/// Read-only access to ZCode's OpenCode-compatible SQLite session store.
///
/// ZCode extends the persisted message envelopes with its own semantics and
/// runtime metadata. The shared reader retains those fields while normalizing
/// every event with the distinct `zcode` provider identity.
pub struct ZCodeSessionSource {
  inner: OpenCodeSessionSource,
}

impl ZCodeSessionSource {
  pub fn new(session_dir: Option<PathBuf>) -> Self {
    Self {
      inner: OpenCodeSessionSource::for_zcode(session_dir),
    }
  }

  pub fn list_sessions(&self) -> Result<Vec<SessionRef>, String> {
    self.inner.list_sessions()
  }

  pub fn list_session_relations(&self) -> Result<Vec<SessionRef>, String> {
    self.inner.list_session_relations()
  }

  pub fn list_session_headers(&self) -> Result<Vec<SessionHeader>, String> {
    self.inner.list_session_headers()
  }

  pub fn list_session_header_catalog(&self) -> Result<SessionHeaderCatalog, String> {
    self.inner.list_session_header_catalog()
  }

  pub fn hydrate_session_header(&self, header: SessionHeader) -> Result<SessionHeader, String> {
    self.inner.hydrate_session_header(header)
  }

  pub fn load_session(&self, session_id: &str) -> Result<LoadedSession, String> {
    self.inner.load_session(session_id)
  }

  pub fn load_session_exact(&self, session_id: &str) -> Result<LoadedSession, String> {
    self.inner.load_session_exact(session_id)
  }

  pub fn database_path(&self) -> Result<PathBuf, String> {
    self.inner.database_path()
  }
}
