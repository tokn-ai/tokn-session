use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, future::Future, pin::Pin};
use tokio::sync::broadcast;

pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
  Pi,
  OpenCode,
  Dsh,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
  Managed,
  Attached,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
  Metadata,
  State,
  Models,
  Message,
  Steer,
  Queue,
  CancelTurn,
  Respond,
  PendingInteractions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capability {
  pub supported: bool,
  pub reason: Option<String>,
}
impl Capability {
  pub fn supported() -> Self {
    Self {
      supported: true,
      reason: None,
    }
  }
  pub fn unavailable(reason: impl Into<String>) -> Self {
    Self {
      supported: false,
      reason: Some(reason.into()),
    }
  }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Capabilities {
  pub operations: BTreeMap<Operation, Capability>,
  /// True only if the runtime can atomically apply model intent on a new turn
  /// while ignoring it when the same admission steers an existing turn.
  pub atomic_model_on_send: bool,
}
impl Capabilities {
  pub fn supports(&self, operation: Operation) -> bool {
    self
      .operations
      .get(&operation)
      .is_some_and(|capability| capability.supported)
  }
  pub fn with(mut self, operation: Operation) -> Self {
    self.operations.insert(operation, Capability::supported());
    self
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeDescriptor {
  pub runtime_id: String,
  pub instance_id: String,
  pub provider: Provider,
  pub ownership: Ownership,
  pub capabilities: Capabilities,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionTarget {
  pub runtime_id: String,
  pub instance_id: String,
  pub session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelIdentity {
  pub model_provider: Option<String>,
  pub model_id: String,
  pub display_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelChoice {
  pub model: ModelIdentity,
  pub supports_images: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Metadata {
  pub name: Option<String>,
  pub version: Option<String>,
  pub model: Option<ModelIdentity>,
  pub commands: Vec<String>,
  #[serde(default)]
  pub agents: Vec<String>,
  #[serde(default)]
  pub providers: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
  Unknown,
  Idle,
  Running,
  Stopping,
  Stopped,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionState {
  pub session_id: Option<String>,
  pub status: ExecutionState,
  pub model: Option<ModelIdentity>,
  pub pending_inputs: Option<u64>,
  pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImageInput {
  pub mime_type: String,
  pub data: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MessageInput {
  pub text: String,
  #[serde(default)]
  pub images: Vec<ImageInput>,
  pub model: Option<ModelIdentity>,
  pub expected_revision: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
  /// Start only if native admission can guarantee idle state; otherwise reject.
  Start,
  /// Steer only an active turn; never silently start or queue on an idle runtime.
  Steer,
  /// Native admission starts if idle, steers if busy, and reports its outcome.
  StartOrSteer,
  /// Native follow-up delivery; execution may begin immediately if already idle.
  Queue,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionOption {
  pub label: String,
  pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionItem {
  pub question_id: String,
  pub prompt: String,
  pub options: Vec<QuestionOption>,
  pub allows_free_text: bool,
  pub multiple: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
  Question,
  Permission,
  Confirm,
  Select,
  Input,
  Editor,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingInteraction {
  pub interaction_id: String,
  pub session_id: Option<String>,
  pub kind: InteractionKind,
  pub title: String,
  pub questions: Vec<QuestionItem>,
  /// Native correlation metadata stays host-side; UI projections must allowlist it.
  pub native: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionAnswer {
  pub question_id: String,
  pub answers: Vec<String>,
  #[serde(default)]
  pub custom: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
  AllowOnce,
  AllowAlways,
  Deny,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionResponse {
  Answers {
    answers: Vec<QuestionAnswer>,
  },
  Permission {
    decision: PermissionDecision,
  },
  Dialog {
    value: Option<String>,
    confirmed: Option<bool>,
    cancelled: bool,
  },
  Reject,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
  Metadata,
  State,
  Models,
  Message {
    input: MessageInput,
    delivery: DeliveryMode,
  },
  PendingInteractions,
  Respond {
    interaction_id: String,
    response: InteractionResponse,
  },
  CancelTurn,
}
impl Command {
  pub fn operation(&self) -> Operation {
    match self {
      Self::Metadata => Operation::Metadata,
      Self::State => Operation::State,
      Self::Models => Operation::Models,
      Self::Message {
        delivery: DeliveryMode::Steer,
        ..
      } => Operation::Steer,
      Self::Message {
        delivery: DeliveryMode::Queue,
        ..
      } => Operation::Queue,
      Self::Message { .. } => Operation::Message,
      Self::PendingInteractions => Operation::PendingInteractions,
      Self::Respond { .. } => Operation::Respond,
      Self::CancelTurn => Operation::CancelTurn,
    }
  }
  pub fn is_mutation(&self) -> bool {
    matches!(self, Self::Message { .. } | Self::Respond { .. } | Self::CancelTurn)
  }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InteractionRequest {
  pub request_id: String,
  pub target: SessionTarget,
  pub command: Command,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDisposition {
  Started,
  Steered,
  Queued,
  Accepted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InputReceipt {
  pub disposition: InputDisposition,
  pub model: Option<ModelIdentity>,
  pub model_ignored: bool,
  pub native_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Reply {
  Metadata(Metadata),
  State(SessionState),
  Models(Vec<ModelChoice>),
  Interactions(Vec<PendingInteraction>),
  Admission(InputReceipt),
  Acknowledged,
  Pending,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
  Unsupported,
  Unavailable,
  InvalidRequest,
  StaleTarget,
  Rejected,
  Conflict,
  Uncertain,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InteractionError {
  pub kind: ErrorKind,
  pub message: String,
}
impl InteractionError {
  pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
    Self {
      kind,
      message: message.into(),
    }
  }
  pub fn unsupported(message: impl Into<String>) -> Self {
    Self::new(ErrorKind::Unsupported, message)
  }
  pub fn unavailable(message: impl Into<String>) -> Self {
    Self::new(ErrorKind::Unavailable, message)
  }
  pub fn invalid(message: impl Into<String>) -> Self {
    Self::new(ErrorKind::InvalidRequest, message)
  }
  pub fn uncertain(message: impl Into<String>) -> Self {
    Self::new(ErrorKind::Uncertain, message)
  }
}
impl std::fmt::Display for InteractionError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    self.message.fmt(formatter)
  }
}
impl std::error::Error for InteractionError {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeEvent {
  pub runtime_id: String,
  pub instance_id: String,
  pub session_id: Option<String>,
  pub sequence: u64,
  pub kind: RuntimeEventKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RuntimeEventKind {
  State(SessionState),
  InteractionRequested(PendingInteraction),
  InteractionResolved { interaction_id: String },
  Native(Value),
  Disconnected { reason: String },
}

/// Native admission remains authoritative. A host lock alone cannot fence
/// other clients of an attached runtime, so adapters must report real guarantees.
pub trait InteractionAdapter: Send + Sync + 'static {
  fn descriptor(&self) -> RuntimeDescriptor;
  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Result<Reply, InteractionError>>;
  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent>;
  /// Detach attached endpoints; terminate only children this adapter owns.
  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>>;
}
