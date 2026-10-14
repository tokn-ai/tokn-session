pub(crate) mod activity;
use activity::{Activity, ActivityReaders};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokn_session_client::{AgentClient, SessionHeader};
use tokn_session_core::LoadedSession;

use crate::model::{SessionLocator, ViewerProvider};

const JSONL_BODY_BASE_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);
const JSONL_BODY_MAX_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const JSONL_BODY_BYTES_PER_QUIET_SECOND: u64 = 32 * 1024;
const JSONL_EAGER_BODY_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionBodyIndexing {
  Ready,
  Deferred,
  CatalogOnly,
}

fn jsonl_body_quiet_period(len: u64) -> std::time::Duration {
  let size_seconds = len.div_ceil(JSONL_BODY_BYTES_PER_QUIET_SECOND);
  JSONL_BODY_BASE_QUIET_PERIOD
    .saturating_add(std::time::Duration::from_secs(size_seconds))
    .min(JSONL_BODY_MAX_QUIET_PERIOD)
}

fn jsonl_body_indexing(len: u64, age: std::time::Duration) -> SessionBodyIndexing {
  if len > JSONL_EAGER_BODY_MAX_BYTES {
    SessionBodyIndexing::CatalogOnly
  } else if age >= jsonl_body_quiet_period(len) {
    SessionBodyIndexing::Ready
  } else {
    SessionBodyIndexing::Deferred
  }
}

pub(crate) trait ViewerRepository: Send + Sync {
  fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String>;

  /// Resolves file roots for providers that can be incrementally cataloged
  /// from individual filesystem changes. The default keeps existing test and
  /// alternate repositories source-compatible until they opt in.
  fn file_session_roots(&self, provider: ViewerProvider) -> Result<Vec<PathBuf>, String> {
    Err(format!(
      "{} does not support path-targeted session cataloging",
      provider.as_str()
    ))
  }

  /// Enumerates file-backed sessions without parsing their headers.
  fn file_session_paths(&self, provider: ViewerProvider) -> Result<Vec<PathBuf>, String> {
    Err(format!(
      "{} does not support path-only session cataloging",
      provider.as_str()
    ))
  }

  /// Applies provider metadata stored outside individual session files.
  fn apply_catalog_metadata(&self, _provider: ViewerProvider, _headers: &mut [SessionHeader]) -> Result<(), String> {
    Ok(())
  }

  /// Reads one metadata-only session header from a known changed file. The
  /// default makes unsupported repository implementations fail closed, so a
  /// caller can fall back to a complete provider catalog instead.
  fn session_header_at_path(&self, provider: ViewerProvider, path: &Path) -> Result<SessionHeader, String> {
    Err(format!(
      "{} does not support path-targeted session cataloging for {}",
      provider.as_str(),
      path.display()
    ))
  }

  /// Avoids parsing a JSONL file while an agent is actively appending to it.
  /// Alternate repositories are stable by construction unless they opt in.
  fn session_body_indexing(&self, _locator: &SessionLocator) -> Result<SessionBodyIndexing, String> {
    Ok(SessionBodyIndexing::Ready)
  }

  fn load_activity(&self, locator: &SessionLocator) -> Result<Activity, String> {
    Activity::from_loaded(self.load_session(locator)?, &locator.session_id)
  }

  fn load_session(&self, locator: &SessionLocator) -> Result<LoadedSession, String>;
}

#[derive(Default)]
pub(crate) struct NativeRepository {
  activity: Mutex<ActivityReaders>,
}

impl ViewerRepository for NativeRepository {
  fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
    AgentClient::list_session_headers(provider.source(), None)
  }

  fn file_session_roots(&self, provider: ViewerProvider) -> Result<Vec<PathBuf>, String> {
    AgentClient::file_session_roots(provider.source(), None)
  }

  fn file_session_paths(&self, provider: ViewerProvider) -> Result<Vec<PathBuf>, String> {
    AgentClient::file_session_paths(provider.source(), None)
  }

  fn apply_catalog_metadata(&self, provider: ViewerProvider, headers: &mut [SessionHeader]) -> Result<(), String> {
    AgentClient::apply_catalog_metadata(provider.source(), None, headers)
  }

  fn session_header_at_path(&self, provider: ViewerProvider, path: &Path) -> Result<SessionHeader, String> {
    AgentClient::session_header_at_path(provider.source(), None, path)
  }

  fn session_body_indexing(&self, locator: &SessionLocator) -> Result<SessionBodyIndexing, String> {
    if !matches!(locator.provider, ViewerProvider::Codex | ViewerProvider::Pi) {
      return Ok(SessionBodyIndexing::Ready);
    }
    let source_path = if locator.provider == ViewerProvider::Codex {
      tokn_session_codex::resolve_rollout_path(&locator.source_path)
    } else {
      locator.source_path.clone()
    };
    let metadata = std::fs::metadata(&source_path)
      .map_err(|error| format!("failed to inspect {}: {error}", locator.source_path.display()))?;
    let modified = metadata
      .modified()
      .map_err(|error| format!("failed to inspect {}: {error}", locator.source_path.display()))?;
    let age = modified.elapsed().unwrap_or_default();
    if age <= std::time::Duration::from_secs(30) || self.activity.lock().map_err(|e| e.to_string())?.contains(locator) {
      return Ok(SessionBodyIndexing::Ready);
    }
    Ok(jsonl_body_indexing(metadata.len(), age))
  }

  fn load_activity(&self, locator: &SessionLocator) -> Result<Activity, String> {
    if matches!(locator.provider, ViewerProvider::Codex | ViewerProvider::Pi) {
      return self.activity.lock().map_err(|e| e.to_string())?.read(locator);
    }
    Activity::from_loaded(self.load_session(locator)?, &locator.session_id)
  }

  fn load_session(&self, locator: &SessionLocator) -> Result<LoadedSession, String> {
    match locator.provider {
      ViewerProvider::OpenCode | ViewerProvider::ZCode => AgentClient::load_session(
        locator.provider.source(),
        Some(locator.source_path.clone()),
        &locator.session_id,
      ),
      ViewerProvider::Codex | ViewerProvider::Pi | ViewerProvider::WorkBuddy | ViewerProvider::Dsh => {
        let path = locator
          .source_path
          .to_str()
          .ok_or_else(|| "session path is not valid UTF-8".to_string())?;
        AgentClient::load_session(locator.provider.source(), None, path)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn native_repository_reads_active_jsonl_activity_without_the_cold_body_delay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("active.jsonl");
    std::fs::write(&path, "active").unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "active".into(),
      source_path: path,
    };
    assert_eq!(
      NativeRepository::default().session_body_indexing(&locator).unwrap(),
      SessionBodyIndexing::Ready
    );
  }

  #[test]
  fn jsonl_body_quiet_period_scales_with_parse_cost() {
    assert_eq!(jsonl_body_quiet_period(0), std::time::Duration::from_secs(2));
    assert_eq!(jsonl_body_quiet_period(32 * 1024), std::time::Duration::from_secs(3));
    assert_eq!(
      jsonl_body_quiet_period(10 * 1024 * 1024),
      std::time::Duration::from_secs(5 * 60)
    );
  }

  #[test]
  fn oversized_jsonl_bodies_stay_on_demand() {
    assert_eq!(
      jsonl_body_indexing(JSONL_EAGER_BODY_MAX_BYTES, std::time::Duration::from_secs(5 * 60)),
      SessionBodyIndexing::Ready
    );
    assert_eq!(
      jsonl_body_indexing(JSONL_EAGER_BODY_MAX_BYTES + 1, std::time::Duration::from_secs(5 * 60)),
      SessionBodyIndexing::CatalogOnly
    );
  }
}
