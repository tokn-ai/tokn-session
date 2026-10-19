//! Local project registration and viewer-owned session contracts.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub struct RegisteredProject {
  pub project_id: String,
  pub group_key: String,
  pub folder_path: String,
  pub name: String,
  pub created_at_ms: i64,
  pub available: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProjectWorkspaceSnapshot {
  pub available: bool,
  pub unavailable_reason: Option<String>,
  pub projects: Vec<RegisteredProject>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RegisterProjectRequest {
  pub folder_path: String,
  pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CreateProjectSessionRequest {
  pub request_id: String,
  pub project_id: String,
  pub prompt: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionCreationError {
  pub message: String,
  pub may_have_started: bool,
}

impl SessionCreationError {
  pub(crate) fn not_started(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      may_have_started: false,
    }
  }

  pub(crate) fn uncertain(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      may_have_started: true,
    }
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSessionStatus {
  Starting,
  Running,
  Idle,
  Stopping,
  Stopped,
  Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagedSessionDialog {
  pub dialog_id: String,
  /// Pi RPC dialog method: confirm, select, input, or editor.
  pub kind: String,
  pub title: String,
  pub message: Option<String>,
  pub options: Vec<String>,
  pub initial_value: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagedSessionSnapshot {
  pub revision: u64,
  pub request_id: String,
  pub project_id: String,
  pub provider: String,
  pub status: ManagedSessionStatus,
  pub session_id: Option<String>,
  pub session_key: Option<String>,
  /// Present only after validating and indexing Pi's durable history file.
  pub session: Option<crate::model::SessionSummary>,
  pub message: Option<String>,
  /// Latest settled provider-turn failure; the owned runtime can remain ready.
  pub last_turn_error: Option<String>,
  pub pending_dialog: Option<ManagedSessionDialog>,
}

/// Exactly one identity must be supplied. Session keys are accepted only for
/// persisted sessions that this viewer explicitly created.
#[derive(Clone, Debug, Deserialize)]
pub struct ManagedSessionRequest {
  pub request_id: Option<String>,
  pub session_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ManagedSessionDialogResponse {
  pub request_id: String,
  pub dialog_id: String,
  pub value: Option<String>,
  pub confirmed: Option<bool>,
  #[serde(default)]
  pub cancelled: bool,
}
