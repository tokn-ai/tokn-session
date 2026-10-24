//! Local project registration and viewer-owned session contracts.
use serde::{Deserialize, Serialize};
pub use tokn_interaction_core::managed_sessions::{
  CreateProjectSessionRequest, ManagedSessionDialog, ManagedSessionDialogResponse, ManagedSessionRequest,
  ManagedSessionStatus,
};

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

impl From<tokn_interaction_core::managed_sessions::ManagedSessionSnapshot> for ManagedSessionSnapshot {
  fn from(snapshot: tokn_interaction_core::managed_sessions::ManagedSessionSnapshot) -> Self {
    Self {
      revision: snapshot.revision,
      request_id: snapshot.request_id,
      project_id: snapshot.project_id,
      provider: snapshot.provider,
      status: snapshot.status,
      session_id: snapshot.session_id,
      session_key: snapshot.session_key,
      session: None,
      message: snapshot.message,
      last_turn_error: snapshot.last_turn_error,
      pending_dialog: snapshot.pending_dialog,
    }
  }
}
