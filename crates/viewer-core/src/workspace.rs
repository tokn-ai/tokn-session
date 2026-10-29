//! Local project registration and viewer-owned session contracts.
use serde::{Deserialize, Serialize};
pub use tokn_interaction_core::managed_sessions::{
  ManagedSessionDialog, ManagedSessionDialogResponse, ManagedSessionRequest, ManagedSessionStatus,
};

/// An omitted provider preserves the original Pi creation command.
#[derive(Clone, Debug, Deserialize)]
pub struct CreateProjectSessionRequest {
  pub request_id: String,
  pub project_id: String,
  pub prompt: String,
  #[serde(default)]
  pub image_ids: Vec<String>,
  #[serde(default = "default_creation_provider")]
  pub provider: tokn_interaction_core::Provider,
  pub runtime_profile_id: Option<String>,
  pub runtime_revision: Option<u64>,
}

fn default_creation_provider() -> tokn_interaction_core::Provider {
  tokn_interaction_core::Provider::Pi
}

impl CreateProjectSessionRequest {
  pub(crate) fn pi_request(&self) -> tokn_interaction_core::managed_sessions::CreateProjectSessionRequest {
    tokn_interaction_core::managed_sessions::CreateProjectSessionRequest {
      request_id: self.request_id.clone(),
      project_id: self.project_id.clone(),
      prompt: self.prompt.clone(),
      image_ids: self.image_ids.clone(),
    }
  }
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionCreationOptionsRequest {
  pub project_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionCreationRuntime {
  pub profile_id: String,
  pub revision: u64,
  pub name: String,
  pub available: bool,
  pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionCreationProvider {
  pub provider: tokn_interaction_core::Provider,
  pub available: bool,
  pub reason: Option<String>,
  pub supports_images: bool,
  pub default_profile_id: Option<String>,
  pub runtimes: Vec<SessionCreationRuntime>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionCreationOptions {
  pub project_id: String,
  pub providers: Vec<SessionCreationProvider>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectOrigin {
  Manual,
  Discovered,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectDiscoveryState {
  Present,
  Absent,
  Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RegisteredProject {
  pub project_id: String,
  pub group_key: String,
  pub folder_path: String,
  pub name: String,
  pub created_at_ms: i64,
  pub available: bool,
  pub origin: ProjectOrigin,
  pub discovery_state: ProjectDiscoveryState,
  pub unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
  /// Present only after validating and indexing the provider's native history.
  pub session: Option<crate::model::SessionSummary>,
  pub message: Option<String>,
  /// Latest settled provider-turn failure; the owned runtime can remain ready.
  pub last_turn_error: Option<String>,
  pub pending_dialog: Option<ManagedSessionDialog>,
  pub creation_input_state: Option<tokn_interaction_core::creation::FirstInputState>,
  pub creation_phase: Option<tokn_interaction_core::creation::CreationPhase>,
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
      creation_phase: None,
      creation_input_state: snapshot.creation_input_state,
    }
  }
}
