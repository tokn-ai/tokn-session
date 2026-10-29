//! Host-independent managed-session values and commands.
use super::model_contracts::*;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub const MAX_INPUT_LENGTH: usize = 16 * 1024;
pub const MAX_IMAGES: usize = 4;

/// Opaque host project identity and canonical working folder.
#[derive(Clone, Debug)]
pub struct RegisteredProject {
  pub project_id: String,
  pub folder_path: String,
  pub available: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CreateProjectSessionRequest {
  pub request_id: String,
  pub project_id: String,
  pub prompt: String,
  #[serde(default)]
  pub image_ids: Vec<String>,
  #[serde(default)]
  pub model: Option<crate::ModelIdentity>,
}

/// Host-validated immutable image bytes. Filesystem upload paths stay in the host.
#[derive(Clone, Debug)]
pub struct ResolvedImage {
  pub image_id: String,
  pub mime_type: String,
  pub sha256: String,
  pub bytes: Arc<Vec<u8>>,
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
  /// Provider dialog method: confirm, select, input, or editor.
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
  pub message: Option<String>,
  /// Latest settled provider-turn failure; the owned runtime can remain ready.
  pub last_turn_error: Option<String>,
  /// Initial input acknowledgement, independent of native history readiness.
  /// Legacy receipts without this evidence remain unknown.
  pub creation_input_state: Option<crate::creation::FirstInputState>,
  pub pending_dialog: Option<ManagedSessionDialog>,
}

/// Exactly one identity must be supplied. Session keys are accepted only for
/// persisted sessions that this host explicitly created.
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

#[derive(Clone, Debug)]
pub struct DeliveryError {
  pub message: String,
  pub may_have_been_sent: bool,
  pub model_result: Option<MessageModelResult>,
}

impl DeliveryError {
  pub fn not_sent(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      may_have_been_sent: false,
      model_result: None,
    }
  }

  pub fn uncertain(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      may_have_been_sent: true,
      model_result: None,
    }
  }

  pub fn with_model_result(mut self, result: MessageModelResult) -> Self {
    self.model_result = Some(result);
    self
  }
}

#[derive(Clone, Debug)]
pub enum Admission {
  Accepted,
  Started,
  QueuedFollowUp,
  Steered,
  WithModel {
    disposition: ModelInputDisposition,
    result: MessageModelResult,
  },
}

#[derive(Clone, Copy, Debug)]
pub enum ModelInputDisposition {
  Started,
  Steered,
}

#[derive(Clone, Debug)]
pub enum WorkerState {
  Starting,
  Ready,
  Closing(Option<String>),
  Stopped(Option<String>),
}

pub enum ModelOperation {
  Status,
  List,
}
pub enum ModelResult {
  Status(SessionModelStatus),
  List(ListSessionModelsResponse),
}

/// Semantic requests accepted by an already admitted worker. Native encoding
/// and acknowledgement belong to its provider driver.
pub enum Command {
  Model(ModelOperation, oneshot::Sender<Result<ModelResult, DeliveryError>>),
  Prompt {
    request_id: String,
    text: String,
    images: Vec<ResolvedImage>,
    selection: Option<MessageModelSelection>,
    reply: oneshot::Sender<Result<Admission, DeliveryError>>,
  },
  Abort(oneshot::Sender<Result<(), String>>),
  Dialog(ManagedSessionDialogResponse, oneshot::Sender<Result<(), String>>),
}

pub struct ManagedWorker {
  pub host: super::ManagedHostContext,
  pub lease: super::SessionLease,
  pub native_lease: Option<crate::session_ownership::SessionLease>,
  pub receipt: super::Receipt,
  pub prompt: Option<(String, Vec<ResolvedImage>)>,
  pub resume: bool,
  pub commands: mpsc::Receiver<Command>,
  pub worker_state: watch::Sender<WorkerState>,
  pub termination: CancellationToken,
}

#[derive(Clone, Debug)]
pub struct ManagedHistoryIdentity {
  pub provider: crate::Provider,
  pub session_id: String,
  pub session_file: PathBuf,
}

/// The host supplies its own opaque presentation key. The core never decodes it.
pub trait ManagedSessionHost: Send + Sync {
  fn session_key(&self, identity: &ManagedHistoryIdentity) -> Result<String, String>;
  fn keys_equal(&self, left: &str, right: &str) -> bool {
    left == right
  }
  fn validate_identity(&self, identity: &ManagedHistoryIdentity, key: &str) -> Result<(), String> {
    let expected = self.session_key(identity)?;
    if self.keys_equal(&expected, key) {
      Ok(())
    } else {
      Err("Managed session ownership identity is inconsistent".into())
    }
  }
}

pub trait ManagedSessionDriver: Send + Sync {
  fn provider(&self) -> crate::Provider;
  fn label(&self) -> &str;
  fn model_capabilities(&self) -> ManagedModelCapabilities {
    ManagedModelCapabilities::default()
  }
  /// Discover creation choices without creating a durable native session or
  /// sending a prompt. Implementations own temporary-runtime cleanup.
  fn creation_models(
    &self,
    _working_directory: PathBuf,
  ) -> crate::AdapterFuture<'static, Result<Vec<crate::ModelChoice>, String>> {
    Box::pin(async { Err("This agent does not support model discovery before session creation".into()) })
  }
  fn spawn(&self, worker: ManagedWorker) -> crate::AdapterFuture<'static, ()>;
  /// Bounded provider-native history identity verification; called on a blocking task.
  fn validate_history(
    &self,
    path: &std::path::Path,
    session_id: &str,
    folder: &std::path::Path,
  ) -> Result<Option<ManagedHistoryIdentity>, String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ManagedModelCapabilities {
  pub updates_default: bool,
  pub supports_model_input: bool,
}

/// Shared settled-worker idle deadline; providers supply native eligibility and
/// perform one final native state probe before requesting gated retirement.
pub struct ManagedIdlePolicy {
  timeout: std::time::Duration,
  since: Option<tokio::time::Instant>,
}
impl ManagedIdlePolicy {
  pub fn new(timeout: std::time::Duration) -> Self {
    Self { timeout, since: None }
  }
  pub fn observe(&mut self, eligible: bool) -> bool {
    if !eligible {
      self.reset();
      return false;
    }
    self.since.get_or_insert_with(tokio::time::Instant::now).elapsed() >= self.timeout
  }
  pub fn reset(&mut self) {
    self.since = None;
  }
}

#[derive(Clone, Debug)]
pub struct ManagedSessionConfiguration {
  pub state_path: Option<PathBuf>,
  pub idle_timeout: std::time::Duration,
  /// Version-one receipt catalogs predate provider tagging. Migration must name
  /// their known owner explicitly, never guess from their presentation key.
  pub legacy_provider: Option<crate::Provider>,
  /// Keep the prior lease namespace when adopting an existing host catalog.
  pub lease_prefix: String,
}

#[derive(Clone, Debug)]
pub struct ManagedRuntimeInfo {
  pub runtime_id: String,
  pub instance_id: String,
  pub provider: crate::Provider,
  pub status: String,
  pub pid: Option<u32>,
  pub started_at_ms: Option<i64>,
  pub session_key: Option<String>,
  pub session_id: Option<String>,
  pub project_id: String,
  pub working_directory: String,
  pub has_worker: bool,
  pub externally_owned: bool,
  pub last_error: Option<String>,
  pub can_restart: bool,
  pub restart_disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRestartRequest {
  pub request_id: String,
  pub runtime_id: String,
  pub expected_instance_id: String,
}
