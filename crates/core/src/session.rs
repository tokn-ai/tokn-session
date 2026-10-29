use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::agent_event::AgentEvent;

/// Metadata used to discover a session without reading its conversation body.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
  pub id: String,
  pub parent_session_id: Option<String>,
  pub agent_path: Option<String>,
  pub agent_nickname: Option<String>,
  pub agent_role: Option<String>,
  /// Provider-native user-facing title, when the provider persists one.
  pub title: Option<String>,
  /// Best available first-message preview. This remains separate from
  /// `title` so clients can choose an explicit title before prompt text.
  pub preview: Option<String>,
  pub path: PathBuf,
  pub cwd: Option<String>,
  /// Provider-native session creation time, retained without reinterpretation.
  pub timestamp: Option<String>,
  /// Last-update time. File-backed providers expose file mtime as Unix
  /// milliseconds; catalog-backed providers retain their native update value.
  pub updated_at: Option<String>,
  /// Canonical Unix milliseconds used for cross-provider ordering.
  pub updated_at_ms: Option<i64>,
}

/// A metadata-only observation of a provider's session inventory.
///
/// Incomplete observations still contain useful discoveries, but omitted
/// sessions must not be interpreted as removed from the provider. Completeness
/// proves membership only in the observed storage scope; it does not certify
/// that every conversation body or project directory is readable.
#[derive(Clone, Debug, Default)]
pub struct SessionHeaderCatalog {
  pub headers: Vec<SessionHeader>,
  pub complete: bool,
}

/// File membership without reading session bodies or unchanged headers.
#[derive(Clone, Debug, Default)]
pub struct FileSessionCatalog {
  pub paths: Vec<PathBuf>,
  pub complete: bool,
}

/// Provider-native header relations before file metadata is attached.
#[derive(Clone, Debug, Default)]
pub struct SessionRelationCatalog {
  pub references: Vec<SessionRef>,
  pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRef {
  /// Model immediately before a retained history window; absent for a full
  /// transcript. This checkpoint is state, never a synthetic history record.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model_baseline: Option<crate::ModelObservation>,
  pub id: String,
  pub parent_session_id: Option<String>,
  pub agent_path: Option<String>,
  pub agent_nickname: Option<String>,
  pub agent_role: Option<String>,
  pub title: Option<String>,
  pub preview: Option<String>,
  pub path: PathBuf,
  pub cwd: Option<String>,
  pub timestamp: Option<String>,
  pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSession {
  pub reference: SessionRef,
  pub events: Vec<AgentEvent>,
  pub history_status: SessionHistoryStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSessionTree {
  pub session: LoadedSession,
  pub children: Vec<LoadedSessionTree>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionHistoryStatus {
  Complete,
  FilteredSubagent,
  SubagentBodyUnavailable,
}
