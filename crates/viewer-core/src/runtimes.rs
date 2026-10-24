//! Public, credential-free inventory of runtimes known to the desktop viewer.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
  Pi,
  Reader,
  Hosting,
  HubConnection,
  BackgroundApi,
  BackgroundConnector,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeExecutionKind {
  Subprocess,
  Embedded,
  External,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOwnership {
  Viewer,
  External,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeInfo {
  pub runtime_id: String,
  pub instance_id: String,
  pub name: String,
  pub kind: RuntimeKind,
  pub execution_kind: RuntimeExecutionKind,
  pub ownership: RuntimeOwnership,
  pub status: String,
  pub pid: Option<u32>,
  pub started_at_ms: Option<i64>,
  pub session_key: Option<String>,
  pub session_id: Option<String>,
  pub project_id: Option<String>,
  pub working_directory: Option<String>,
  pub endpoint: Option<String>,
  pub detail: Option<String>,
  pub last_error: Option<String>,
  pub can_restart: bool,
  pub restart_disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeInventory {
  pub version: u8,
  pub runtimes: Vec<RuntimeInfo>,
}

pub use tokn_interaction_core::managed_sessions::RuntimeRestartRequest;

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeRestartResponse {
  pub request_id: String,
  pub runtime_id: String,
  pub message: String,
}

pub(crate) fn now_ms() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis()
    .try_into()
    .unwrap_or(i64::MAX)
}
