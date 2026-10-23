//! Native Pi RPC transport and provider-independent runtime interaction adapter.
pub mod framing;
mod mapping;
mod supervised;
mod transport;

pub use supervised::SupervisedSession;

use serde_json::{Value, json};
use std::{
  collections::{BTreeMap, HashMap},
  ffi::{OsStr, OsString},
  path::PathBuf,
  sync::{Arc, Mutex},
  time::Duration,
};
use tokio::{
  io::{AsyncRead, AsyncWrite},
  process::Child,
  sync::{Mutex as AsyncMutex, broadcast},
  time::Instant,
};
use tokn_interaction_core::*;
use tokn_pi_rpc_protocol as wire;

const MAX_INTERACTIONS: usize = 128;
const MAX_RECEIPTS: usize = 1024;
const MAX_DIALOG_AGE: Duration = Duration::from_secs(120);

/// Identifies one particular transport instance. Reusing an old instance ID
/// across process replacement would invalidate stale-target protection.
#[derive(Clone, Debug)]
pub struct EndpointOptions {
  pub runtime_id: String,
  pub instance_id: String,
  pub expected_session_id: Option<String>,
  pub request_timeout: Duration,
  pub prompt_timeout: Duration,
}

impl EndpointOptions {
  pub fn new(runtime_id: impl Into<String>) -> Self {
    Self {
      runtime_id: runtime_id.into(),
      instance_id: uuid::Uuid::new_v4().to_string(),
      expected_session_id: None,
      request_timeout: Duration::from_secs(15),
      prompt_timeout: Duration::from_secs(300),
    }
  }
}

/// All launch settings are host-owned. No shell is involved, and callers must
/// explicitly choose whether a saved native history should be resumed.
#[derive(Clone, Debug)]
pub struct LaunchOptions {
  pub executable: PathBuf,
  pub working_directory: PathBuf,
  pub arguments: Vec<OsString>,
  pub environment: BTreeMap<OsString, OsString>,
  pub session_file: Option<PathBuf>,
}

#[derive(Clone)]
pub struct PiAdapter {
  inner: Arc<transport::Inner>,
}

#[derive(Default)]
struct Snapshot {
  native_state: Option<wire::State>,
  execution: Option<ExecutionState>,
  revision: u64,
  interactions: BTreeMap<String, DialogRecord>,
  receipts: HashMap<String, Receipt>,
}

#[derive(Clone)]
struct DialogRecord {
  dialog: wire::ExtensionDialog,
  interaction: PendingInteraction,
  expires: Instant,
}

struct Receipt {
  fingerprint: String,
  result: Option<Result<Reply, InteractionError>>,
}

impl PiAdapter {
  /// Accept transport handles intentionally supplied by their owner. This does
  /// not find, steal, or retroactively attach to a running process's pipes.
  pub fn attach<R, W>(options: EndpointOptions, reader: R, writer: W) -> Result<Self, InteractionError>
  where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
  {
    Self::connect(options, Ownership::Attached, reader, writer, None)
  }

  pub async fn spawn(options: EndpointOptions, launch: LaunchOptions) -> Result<Self, InteractionError> {
    Self::spawn_with_supervision(options, launch, None).await
  }

  async fn spawn_with_supervision(
    options: EndpointOptions,
    mut launch: LaunchOptions,
    supervision: Option<tokio::sync::mpsc::Sender<Value>>,
  ) -> Result<Self, InteractionError> {
    validate_options(&options)?;
    if launch.executable.as_os_str().is_empty()
      || !launch.working_directory.is_absolute()
      || launch.session_file.as_ref().is_some_and(|path| !path.is_absolute())
    {
      return Err(InteractionError::invalid(
        "Pi needs an executable, an absolute working directory, and an absolute resume path",
      ));
    }
    let inherited_node = std::env::var_os("NODE_EXTRA_CA_CERTS");
    let inherited_ssl = std::env::var_os("SSL_CERT_FILE");
    let node = launch
      .environment
      .get(OsStr::new("NODE_EXTRA_CA_CERTS"))
      .or(inherited_node.as_ref());
    let ssl = launch
      .environment
      .get(OsStr::new("SSL_CERT_FILE"))
      .or(inherited_ssl.as_ref());
    if let Some(path) = inherited_node_ca_fallback(node.map(OsString::as_os_str), ssl.map(OsString::as_os_str)) {
      launch.environment.insert("NODE_EXTRA_CA_CERTS".into(), path);
    }
    let mut command = tokio::process::Command::new(&launch.executable);
    command
      .args(["--mode", "rpc"])
      .args(launch.arguments)
      .current_dir(launch.working_directory)
      .envs(launch.environment)
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped())
      .kill_on_drop(true);
    if let Some(path) = launch.session_file {
      command.arg("--session").arg(path);
    }
    let mut child = command
      .spawn()
      .map_err(|error| InteractionError::unavailable(format!("Could not start Pi RPC: {error}")))?;
    let reader = child
      .stdout
      .take()
      .ok_or_else(|| InteractionError::unavailable("Pi stdout is unavailable"))?;
    let writer = child
      .stdin
      .take()
      .ok_or_else(|| InteractionError::unavailable("Pi stdin is unavailable"))?;
    Self::connect_with_supervision(options, Ownership::Managed, reader, writer, Some(child), supervision)
  }

  fn connect<R, W>(
    options: EndpointOptions,
    ownership: Ownership,
    reader: R,
    writer: W,
    child: Option<Child>,
  ) -> Result<Self, InteractionError>
  where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
  {
    Self::connect_with_supervision(options, ownership, reader, writer, child, None)
  }

  fn connect_with_supervision<R, W>(
    options: EndpointOptions,
    ownership: Ownership,
    reader: R,
    writer: W,
    child: Option<Child>,
    supervision: Option<tokio::sync::mpsc::Sender<Value>>,
  ) -> Result<Self, InteractionError>
  where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
  {
    validate_options(&options)?;
    let inner = transport::Inner::new(options, ownership, writer, child, supervision);
    transport::start_reader(&inner, reader);
    Ok(Self { inner })
  }

  pub fn stderr_tail(&self) -> String {
    self
      .inner
      .stderr
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .text()
  }

  async fn perform(&self, request: &InteractionRequest) -> Result<Reply, InteractionError> {
    self.inner.check_target(&request.target)?;
    match &request.command {
      Command::Message { input, .. } if input.model.is_some() || input.expected_revision.is_some() => {
        return Err(InteractionError::unsupported(
          "Pi RPC cannot atomically apply model intent with prompt admission",
        ));
      }
      Command::Message {
        delivery: DeliveryMode::Steer,
        ..
      } => {
        return Err(InteractionError::unsupported(
          "Pi RPC cannot atomically reject strict steering after a turn settles; use start_or_steer",
        ));
      }
      _ => {}
    }
    // Responses must remain available while a prompt preflight is waiting for
    // extension UI, so only message admissions share this lock.
    let _admission = if matches!(request.command, Command::Message { .. }) {
      Some(self.inner.admission.lock().await)
    } else {
      None
    };
    let native = self.inner.fresh_state(&request.target).await?;
    match &request.command {
      Command::State => Ok(Reply::State(self.inner.session_state())),
      Command::Metadata => {
        let value = self.inner.rpc(wire::Command::GetCommands, false).await?;
        self.inner.check_session(&request.target)?;
        let commands = mapping::commands(&value)?;
        Ok(Reply::Metadata(Metadata {
          name: native.session_name,
          model: native.model.as_ref().and_then(mapping::model),
          commands,
          ..Metadata::default()
        }))
      }
      Command::Models => {
        let value = self.inner.rpc(wire::Command::GetAvailableModels, false).await?;
        self.inner.check_session(&request.target)?;
        Ok(Reply::Models(mapping::models(&value)?))
      }
      Command::Message { input, delivery } => {
        mapping::validate_input(input)?;
        if !input.images.is_empty() && !native.images_available {
          return Err(InteractionError::new(
            ErrorKind::Rejected,
            "The active Pi model does not support image input",
          ));
        }
        let busy = native.is_streaming || native.is_compacting || native.pending_message_count > 0;
        if *delivery == DeliveryMode::Start && busy {
          return Err(InteractionError::new(
            ErrorKind::Conflict,
            "Pi is already working; use start_or_steer",
          ));
        }
        let images = input
          .images
          .iter()
          .map(|image| wire::ImageContent {
            kind: "image".into(),
            data: image.data.clone(),
            mime_type: image.mime_type.clone(),
          })
          .collect();
        let command = match delivery {
          DeliveryMode::Queue => wire::Command::FollowUp {
            message: input.text.clone(),
            images,
          },
          DeliveryMode::Start => wire::Command::Prompt {
            message: input.text.clone(),
            images,
            streaming_behavior: None,
          },
          DeliveryMode::StartOrSteer => wire::Command::Prompt {
            message: input.text.clone(),
            images,
            streaming_behavior: Some(wire::StreamingBehavior::Steer),
          },
          DeliveryMode::Steer => unreachable!("unsupported strict steering was rejected"),
        };
        self.inner.rpc(command, true).await?;
        // Pi acknowledges acceptance without reporting which side of an idle
        // race admitted the prompt. Do not invent a started/steered guarantee.
        let disposition = match delivery {
          DeliveryMode::Start => InputDisposition::Started,
          DeliveryMode::Queue => InputDisposition::Queued,
          _ => InputDisposition::Accepted,
        };
        Ok(Reply::Admission(InputReceipt {
          disposition,
          model: native.model.as_ref().and_then(mapping::model),
          model_ignored: false,
          native_id: None,
        }))
      }
      Command::PendingInteractions => {
        let snapshot = self.inner.snapshot.lock().unwrap_or_else(|error| error.into_inner());
        Ok(Reply::Interactions(
          snapshot
            .interactions
            .values()
            .map(|record| record.interaction.clone())
            .collect(),
        ))
      }
      Command::Respond {
        interaction_id,
        response,
      } => {
        self.respond(&request.target, interaction_id, response).await?;
        Ok(Reply::Acknowledged)
      }
      Command::CancelTurn => {
        self.inner.cancel_interactions().await?;
        self.inner.rpc(wire::Command::Abort, true).await?;
        Ok(Reply::Acknowledged)
      }
    }
  }

  async fn respond(
    &self,
    target: &SessionTarget,
    id: &str,
    response: &InteractionResponse,
  ) -> Result<(), InteractionError> {
    let (value, session_id) = {
      let mut snapshot = self.inner.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      let record = snapshot
        .interactions
        .get(id)
        .ok_or_else(|| InteractionError::new(ErrorKind::StaleTarget, "This Pi interaction is no longer pending"))?;
      if record.expires <= Instant::now()
        || target
          .session_id
          .as_ref()
          .is_some_and(|session| record.interaction.session_id.as_ref() != Some(session))
      {
        return Err(InteractionError::new(
          ErrorKind::StaleTarget,
          "This Pi interaction expired or belongs to another session",
        ));
      }
      let value = mapping::dialog_response(&record.dialog, response)?;
      let session_id = record.interaction.session_id.clone();
      // Claim before writing, so concurrent replies or expiration cannot send
      // two answers. An uncertain write must never reopen this native request.
      snapshot.interactions.remove(id);
      (value, session_id)
    };
    self.inner.publish_for_session(
      session_id,
      RuntimeEventKind::InteractionResolved {
        interaction_id: id.to_owned(),
      },
    );
    self.inner.write_value(value, true).await?;
    Ok(())
  }

  async fn execute_request(&self, request: InteractionRequest) -> Result<Reply, InteractionError> {
    self.inner.check_identity(&request.target)?;
    if !valid_identity(&request.request_id) {
      return Err(InteractionError::invalid("Invalid interaction request ID"));
    }
    if !request.command.is_mutation() {
      return self.perform(&request).await;
    }
    let fingerprint = serde_json::to_string(&request).map_err(|error| InteractionError::invalid(error.to_string()))?;
    {
      let mut snapshot = self.inner.snapshot.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(receipt) = snapshot.receipts.get(&request.request_id) {
        if receipt.fingerprint != fingerprint {
          return Err(InteractionError::new(
            ErrorKind::Conflict,
            "The request ID was already used for different input",
          ));
        }
        return receipt.result.clone().unwrap_or(Ok(Reply::Pending));
      }
      self.inner.check_available()?;
      if snapshot.receipts.len() >= MAX_RECEIPTS {
        return Err(InteractionError::new(
          ErrorKind::Conflict,
          "Pi runtime receipt limit reached; reconnect with a new instance before admitting more input",
        ));
      }
      snapshot.receipts.insert(
        request.request_id.clone(),
        Receipt {
          fingerprint,
          result: None,
        },
      );
    }
    // Admission outlives a disconnected API caller. Repeated request IDs only
    // observe the receipt and never write the native command twice.
    let adapter = self.clone();
    let task = tokio::spawn(async move {
      let result = adapter.perform(&request).await;
      if let Some(receipt) = adapter
        .inner
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .receipts
        .get_mut(&request.request_id)
      {
        receipt.result = Some(result.clone());
      }
      result
    });
    task
      .await
      .map_err(|_| InteractionError::uncertain("Pi command admission was interrupted"))?
  }
}

impl InteractionAdapter for PiAdapter {
  fn descriptor(&self) -> RuntimeDescriptor {
    self.inner.descriptor.clone()
  }
  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Result<Reply, InteractionError>> {
    Box::pin(self.execute_request(request))
  }
  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.inner.events.subscribe()
  }
  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>> {
    Box::pin(self.inner.close())
  }
}

fn valid_identity(value: &str) -> bool {
  !value.is_empty() && value.len() <= 512 && value.trim() == value && !value.chars().any(char::is_control)
}

/// Node does not automatically use SSL_CERT_FILE. An explicit Node setting,
/// including an empty one, always takes priority over the host CA fallback.
fn inherited_node_ca_fallback(node_extra_ca_certs: Option<&OsStr>, ssl_cert_file: Option<&OsStr>) -> Option<OsString> {
  if node_extra_ca_certs.is_some() {
    return None;
  }
  ssl_cert_file.filter(|path| !path.is_empty()).map(OsStr::to_os_string)
}
fn validate_options(options: &EndpointOptions) -> Result<(), InteractionError> {
  if !valid_identity(&options.runtime_id)
    || !valid_identity(&options.instance_id)
    || options
      .expected_session_id
      .as_ref()
      .is_some_and(|id| !valid_identity(id))
    || options.request_timeout.is_zero()
    || options.prompt_timeout.is_zero()
  {
    return Err(InteractionError::invalid(
      "Invalid Pi RPC endpoint identity or deadline",
    ));
  }
  Ok(())
}

fn capabilities() -> Capabilities {
  let mut capabilities = Capabilities::default()
    .with(Operation::Metadata)
    .with(Operation::State)
    .with(Operation::Models)
    .with(Operation::Message)
    .with(Operation::Queue)
    .with(Operation::CancelTurn)
    .with(Operation::Respond)
    .with(Operation::PendingInteractions);
  capabilities.operations.insert(
    Operation::Steer,
    Capability::unavailable("Pi RPC has no atomic active-only steering precondition; use start_or_steer"),
  );
  capabilities
}

#[cfg(test)]
mod supervised_tests;

#[cfg(test)]
mod certificate_tests {
  use super::*;

  #[test]
  fn only_an_absent_node_setting_inherits_a_nonempty_host_bundle() {
    let bundle = OsStr::new("/configured CA bundle.pem");
    assert_eq!(
      inherited_node_ca_fallback(None, Some(bundle)),
      Some(bundle.to_os_string())
    );
    for explicit in [OsStr::new(""), OsStr::new("/node-specific.pem")] {
      assert_eq!(inherited_node_ca_fallback(Some(explicit), Some(bundle)), None);
    }
    assert_eq!(inherited_node_ca_fallback(None, None), None);
    assert_eq!(inherited_node_ca_fallback(None, Some(OsStr::new(""))), None);
  }
}
#[cfg(test)]
mod tests;
