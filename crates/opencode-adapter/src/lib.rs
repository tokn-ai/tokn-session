//! Rust control adapter for OpenCode's native V2 server, not its historical V1 store.

mod managed;
mod normalize;
mod transport;

use base64::Engine;
use serde_json::{Value, json};
use std::{
  collections::BTreeMap,
  path::PathBuf,
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  time::Duration,
};
use tokio::{
  process::Child,
  sync::{Mutex, broadcast, watch},
  task::JoinHandle,
};
use tokn_interaction_core::*;
use tokn_opencode_rpc_protocol::{self as wire, Envelope};
pub use transport::Credentials;
use transport::Transport;

/// Endpoint and credentials are host configuration, never frontend command fields.
pub struct ConnectionOptions {
  pub runtime_id: String,
  pub endpoint: String,
  pub directory: String,
  pub workspace_id: Option<String>,
  pub credentials: Option<Credentials>,
}

/// The adapter creates a distinct authenticated, loopback-only server process.
pub struct LaunchOptions {
  pub runtime_id: String,
  pub executable: PathBuf,
  pub directory: String,
  pub port: u16,
  pub environment: BTreeMap<String, String>,
}

struct Inner {
  descriptor: RuntimeDescriptor,
  transport: Transport,
  directory: String,
  workspace_id: Option<String>,
  version: Option<String>,
  events: broadcast::Sender<RuntimeEvent>,
  closed: AtomicBool,
  valid_instance: Arc<AtomicBool>,
  stop: watch::Sender<bool>,
  event_task: Mutex<Option<JoinHandle<()>>>,
  child: Mutex<Option<Child>>,
  output_tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for Inner {
  fn drop(&mut self) {
    let _ = self.stop.send(true);
    if let Some(task) = self.event_task.get_mut().take() {
      task.abort();
    }
    for task in self.output_tasks.get_mut().drain(..) {
      task.abort();
    }
    // Managed children were spawned with kill_on_drop. Attached endpoints have no child.
  }
}

#[derive(Clone)]
pub struct OpenCodeAdapter {
  inner: Arc<Inner>,
}

impl OpenCodeAdapter {
  pub async fn connect(options: ConnectionOptions) -> Result<Self, InteractionError> {
    Self::connect_inner(options, None).await
  }

  pub async fn launch(options: LaunchOptions) -> Result<Self, InteractionError> {
    let password = uuid::Uuid::new_v4().to_string();
    let launch = managed::Launch::start(&options, &password).await?;
    let connection = ConnectionOptions {
      runtime_id: options.runtime_id,
      endpoint: launch.endpoint.clone(),
      directory: options.directory,
      workspace_id: None,
      credentials: Some(Credentials {
        username: "tokn".into(),
        password,
      }),
    };
    Self::connect_inner(connection, Some(launch)).await
  }

  async fn connect_inner(
    options: ConnectionOptions,
    mut launch: Option<managed::Launch>,
  ) -> Result<Self, InteractionError> {
    if options.runtime_id.trim().is_empty() {
      return Err(InteractionError::invalid("OpenCode runtime ID is required"));
    }
    let transport = Transport::new(
      &options.endpoint,
      options.directory.clone(),
      options.workspace_id.clone(),
      options.credentials,
    )?;
    let health: Value = transport.json(&["api", "health"]).await?;
    if health["healthy"] != true {
      return Err(InteractionError::unavailable(
        "OpenCode native V2 endpoint is not healthy",
      ));
    }
    let document: Value = transport.json(&["doc"]).await?;
    let capabilities = capabilities(&document)?;
    let version = transport
      .json::<Value>(&["global", "health"])
      .await
      .ok()
      .and_then(|value| value["version"].as_str().map(str::to_owned));
    let descriptor = RuntimeDescriptor {
      runtime_id: options.runtime_id,
      instance_id: uuid::Uuid::new_v4().to_string(),
      provider: Provider::OpenCode,
      ownership: if launch.is_some() {
        Ownership::Managed
      } else {
        Ownership::Attached
      },
      capabilities,
    };
    let (events, _) = broadcast::channel(256);
    let (stop, receiver) = watch::channel(false);
    let valid_instance = Arc::new(AtomicBool::new(true));
    let event_task = start_events(
      transport.clone(),
      descriptor.clone(),
      options.directory.clone(),
      options.workspace_id.clone(),
      events.clone(),
      receiver,
      valid_instance.clone(),
    );
    Ok(Self {
      inner: Arc::new(Inner {
        descriptor,
        transport,
        directory: options.directory,
        workspace_id: options.workspace_id,
        version,
        events,
        closed: AtomicBool::new(false),
        valid_instance,
        stop,
        event_task: Mutex::new(Some(event_task)),
        child: Mutex::new(launch.as_mut().and_then(|launch| launch.child.take())),
        output_tasks: Mutex::new(
          launch
            .as_mut()
            .map(|launch| std::mem::take(&mut launch.pumps))
            .unwrap_or_default(),
        ),
      }),
    })
  }

  /// Create an explicitly V2 session. Existing historical V1 conversations are never silently converted.
  pub async fn create_session(
    &self,
    agent: Option<String>,
    model: Option<ModelIdentity>,
  ) -> Result<SessionTarget, InteractionError> {
    self.ensure_open()?;
    self.check_child().await?;
    let model = model
      .map(|model| {
        model
          .model_provider
          .map(|provider| json!({"id":model.model_id,"providerID":provider}))
          .ok_or_else(|| InteractionError::invalid("OpenCode models require a provider ID"))
      })
      .transpose()?;
    let mut body = json!({"location":{"directory":self.inner.directory}});
    if let Some(workspace) = &self.inner.workspace_id {
      body["location"]["workspaceID"] = json!(workspace);
    }
    if let Some(agent) = agent {
      body["agent"] = json!(agent);
    }
    if let Some(model) = model {
      body["model"] = model;
    }
    self.ensure_open()?;
    let value = self.inner.transport.post(&["api", "session"], body).await?;
    let session: Envelope<wire::SessionInfo> = serde_json::from_value(value)
      .map_err(|_| InteractionError::uncertain("OpenCode session creation was not confirmed"))?;
    self.check_location(&session.data.location).map_err(|_| {
      InteractionError::uncertain("OpenCode may have created a session, but returned an unexpected location identity")
    })?;
    if session.data.id.is_empty() || session.data.id.len() > 512 || session.data.id.chars().any(char::is_control) {
      return Err(InteractionError::uncertain(
        "OpenCode may have created a session, but returned an invalid session identity",
      ));
    }
    Ok(SessionTarget {
      runtime_id: self.inner.descriptor.runtime_id.clone(),
      instance_id: self.inner.descriptor.instance_id.clone(),
      session_id: Some(session.data.id),
    })
  }

  fn ensure_open(&self) -> Result<(), InteractionError> {
    if self.inner.closed.load(Ordering::Acquire) {
      return Err(InteractionError::unavailable("OpenCode connection is closed"));
    }
    if !self.inner.valid_instance.load(Ordering::Acquire) {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "OpenCode connection lost its runtime identity; attach again before issuing commands",
      ));
    }
    Ok(())
  }

  async fn check_child(&self) -> Result<(), InteractionError> {
    if let Some(child) = self.inner.child.lock().await.as_mut() {
      if child
        .try_wait()
        .map_err(|_| InteractionError::unavailable("Could not inspect the owned OpenCode process"))?
        .is_some()
      {
        self.inner.valid_instance.store(false, Ordering::Release);
        return Err(InteractionError::new(
          ErrorKind::StaleTarget,
          "The owned OpenCode runtime exited",
        ));
      }
    }
    Ok(())
  }

  fn check_location(&self, location: &wire::Location) -> Result<(), InteractionError> {
    if location.directory != self.inner.directory || location.workspace_id != self.inner.workspace_id {
      Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "OpenCode response belongs to another directory or workspace",
      ))
    } else {
      Ok(())
    }
  }

  fn check_envelope<T>(&self, envelope: &Envelope<T>) -> Result<(), InteractionError> {
    let location = envelope
      .location
      .as_ref()
      .ok_or_else(|| InteractionError::unavailable("OpenCode metadata response has no location identity"))?;
    self.check_location(location)
  }

  async fn session(&self, id: &str) -> Result<wire::SessionInfo, InteractionError> {
    let envelope: Envelope<wire::SessionInfo> = self.inner.transport.json(&["api", "session", id]).await?;
    if envelope.data.id != id {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "OpenCode returned another session",
      ));
    }
    self.check_location(&envelope.data.location)?;
    Ok(envelope.data)
  }

  async fn require_v2_session(&self, id: &str) -> Result<(), InteractionError> {
    self.session(id).await?;
    if !self.inner.transport.legacy_messages(id).await?.is_empty() {
      return Err(InteractionError::unsupported(
        "This OpenCode session has legacy V1 history; it cannot be controlled by the native V2 adapter",
      ));
    }
    Ok(())
  }

  async fn metadata(&self, session: Option<&str>) -> Result<Metadata, InteractionError> {
    let mut metadata = Metadata {
      name: Some("OpenCode".into()),
      version: self.inner.version.clone(),
      ..Metadata::default()
    };
    if let Some(session) = session {
      metadata.model = self.session(session).await?.model.as_ref().map(normalize::model);
    }
    for (route, destination) in [
      ("command", &mut metadata.commands),
      ("agent", &mut metadata.agents),
      ("provider", &mut metadata.providers),
    ] {
      let response: Envelope<Vec<Value>> = self.inner.transport.json(&["api", route]).await?;
      self.check_envelope(&response)?;
      *destination = response
        .data
        .iter()
        .filter_map(|value| {
          value
            .get("id")
            .or_else(|| value.get("name"))?
            .as_str()
            .map(str::to_owned)
        })
        .collect();
    }
    Ok(metadata)
  }

  async fn state(&self, session: &str) -> Result<SessionState, InteractionError> {
    let info = self.session(session).await?;
    // Verify compatibility before treating V2's process-local active map as authority.
    if !self.inner.transport.legacy_messages(session).await?.is_empty() {
      return Ok(SessionState {
        session_id: Some(session.into()),
        status: ExecutionState::Unknown,
        model: info.model.as_ref().map(normalize::model),
        pending_inputs: None,
        revision: None,
      });
    }
    let active: Envelope<BTreeMap<String, Value>> = self.inner.transport.json(&["api", "session", "active"]).await?;
    Ok(SessionState {
      session_id: Some(session.into()),
      status: if active.data.contains_key(session) {
        ExecutionState::Running
      } else {
        ExecutionState::Idle
      },
      model: info.model.as_ref().map(normalize::model),
      pending_inputs: None,
      revision: None,
    })
  }

  async fn models(&self) -> Result<Vec<ModelChoice>, InteractionError> {
    let models: Envelope<Vec<wire::ModelInfo>> = self.inner.transport.json(&["api", "model"]).await?;
    self.check_envelope(&models)?;
    Ok(
      models
        .data
        .into_iter()
        .filter(|model| model.enabled)
        .map(|model| ModelChoice {
          model: ModelIdentity {
            model_provider: Some(model.provider_id),
            model_id: model.id,
            display_name: Some(model.name),
          },
          supports_images: model.capabilities.input.iter().any(|input| input == "image"),
        })
        .collect(),
    )
  }

  async fn pending(&self, session: &str) -> Result<Vec<PendingInteraction>, InteractionError> {
    self.session(session).await?;
    let questions: Envelope<Vec<wire::QuestionRequest>> = self
      .inner
      .transport
      .json(&["api", "session", session, "question"])
      .await?;
    let permissions: Envelope<Vec<wire::PermissionRequest>> = self
      .inner
      .transport
      .json(&["api", "session", session, "permission"])
      .await?;
    if questions.data.iter().any(|request| request.session_id != session)
      || permissions.data.iter().any(|request| request.session_id != session)
    {
      return Err(InteractionError::new(
        ErrorKind::StaleTarget,
        "OpenCode pending interaction belongs to another session",
      ));
    }
    Ok(
      questions
        .data
        .into_iter()
        .map(normalize::question)
        .chain(permissions.data.into_iter().map(normalize::permission))
        .collect(),
    )
  }

  async fn message(
    &self,
    session: &str,
    request_id: &str,
    input: MessageInput,
    delivery: DeliveryMode,
  ) -> Result<Reply, InteractionError> {
    if input.model.is_some() || input.expected_revision.is_some() {
      return Err(InteractionError::unsupported(
        "OpenCode V2 cannot atomically apply a model choice only when admission starts a new turn",
      ));
    }
    let delivery = match delivery {
      DeliveryMode::StartOrSteer => wire::Delivery::Steer,
      DeliveryMode::Queue => wire::Delivery::Queue,
      DeliveryMode::Start | DeliveryMode::Steer => {
        return Err(InteractionError::unsupported(
          "OpenCode V2 offers start-or-steer, not fenced idle-only or active-only admission",
        ));
      }
    };
    if request_id.is_empty()
      || request_id.len() > 160
      || !request_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
      return Err(InteractionError::invalid(
        "OpenCode request ID must contain 1–160 ASCII letters, digits, hyphens or underscores",
      ));
    }
    if input.text.len() > 1024 * 1024
      || input.images.len() > 8
      || (input.text.trim().is_empty() && input.images.is_empty())
    {
      return Err(InteractionError::invalid(
        "OpenCode input is empty or exceeds the message limit",
      ));
    }
    let mut files = Vec::new();
    let mut image_bytes = 0_usize;
    for image in input.images {
      image_bytes = image_bytes.saturating_add(image.data.len());
      if !matches!(
        image.mime_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
      ) || image.data.is_empty()
        || image_bytes > 6 * 1024 * 1024
        || base64::engine::general_purpose::STANDARD.decode(&image.data).is_err()
      {
        return Err(InteractionError::invalid(
          "OpenCode image must use a supported image MIME type and bounded base64 data",
        ));
      }
      files.push(wire::FileAttachment {
        uri: format!("data:{};base64,{}", image.mime_type, image.data),
        name: None,
      });
    }
    self.require_v2_session(session).await?;
    let native_id = format!("msg_tokn_{request_id}");
    let prompt = wire::PromptRequest {
      id: native_id.clone(),
      prompt: wire::Prompt {
        text: input.text,
        files,
      },
      delivery,
      resume: true,
    };
    self.ensure_open()?;
    self.check_child().await?;
    let value = self
      .inner
      .transport
      .post(
        &["api", "session", session, "prompt"],
        serde_json::to_value(&prompt).expect("prompt serializes"),
      )
      .await?;
    let response: Envelope<wire::Admission> = serde_json::from_value(value)
      .map_err(|_| InteractionError::uncertain("OpenCode prompt admission response is incompatible"))?;
    if response.data.session_id != session
      || response.data.id != native_id
      || response.data.delivery != delivery
      || response.data.prompt["text"] != prompt.prompt.text
    {
      return Err(InteractionError::uncertain(
        "OpenCode admission does not match the submitted message",
      ));
    }
    Ok(Reply::Admission(InputReceipt {
      disposition: if delivery == wire::Delivery::Queue {
        InputDisposition::Queued
      } else {
        InputDisposition::Accepted
      },
      model: None,
      model_ignored: false,
      native_id: Some(native_id),
    }))
  }

  async fn respond(
    &self,
    session: &str,
    interaction_id: String,
    response: InteractionResponse,
  ) -> Result<Reply, InteractionError> {
    self.require_v2_session(session).await?;
    let pending = self.pending(session).await?;
    let request = pending
      .iter()
      .find(|request| request.interaction_id == interaction_id)
      .ok_or_else(|| InteractionError::new(ErrorKind::StaleTarget, "OpenCode interaction is no longer pending"))?;
    let native_id = request.native["id"]
      .as_str()
      .ok_or_else(|| InteractionError::unavailable("OpenCode interaction has no native ID"))?;
    let (kind, action, body) = match (&request.kind, response) {
      (InteractionKind::Question, InteractionResponse::Answers { answers }) => {
        ("question", "reply", normalize::answer_body(request, answers)?)
      }
      (InteractionKind::Question, InteractionResponse::Reject) => ("question", "reject", json!({})),
      (InteractionKind::Permission, InteractionResponse::Permission { decision }) => (
        "permission",
        "reply",
        json!({"reply": match decision {
          PermissionDecision::AllowOnce => "once", PermissionDecision::AllowAlways => "always", PermissionDecision::Deny => "reject",
        }}),
      ),
      (InteractionKind::Permission, InteractionResponse::Reject) => ("permission", "reply", json!({"reply":"reject"})),
      _ => {
        return Err(InteractionError::invalid(
          "Response type does not match the OpenCode pending interaction",
        ));
      }
    };
    self.ensure_open()?;
    self.check_child().await?;
    self
      .inner
      .transport
      .post(&["api", "session", session, kind, native_id, action], body)
      .await?;
    Ok(Reply::Acknowledged)
  }
}

impl InteractionAdapter for OpenCodeAdapter {
  fn descriptor(&self) -> RuntimeDescriptor {
    self.inner.descriptor.clone()
  }

  fn execute(&self, request: InteractionRequest) -> AdapterFuture<'_, Result<Reply, InteractionError>> {
    Box::pin(async move {
      self.ensure_open()?;
      if request.target.runtime_id != self.inner.descriptor.runtime_id
        || request.target.instance_id != self.inner.descriptor.instance_id
      {
        return Err(InteractionError::new(
          ErrorKind::StaleTarget,
          "OpenCode runtime instance changed",
        ));
      }
      if !self.inner.descriptor.capabilities.supports(request.command.operation()) {
        return Err(InteractionError::unsupported(
          "This OpenCode connection does not support the requested operation",
        ));
      }
      if let Command::Metadata = &request.command {
        return self
          .metadata(request.target.session_id.as_deref())
          .await
          .map(Reply::Metadata);
      }
      if let Command::Models = &request.command {
        return self.models().await.map(Reply::Models);
      }
      let session = request
        .target
        .session_id
        .as_deref()
        .filter(|id| !id.is_empty() && id.len() <= 512)
        .ok_or_else(|| InteractionError::invalid("OpenCode command requires a session ID"))?;
      match request.command {
        Command::State => self.state(session).await.map(Reply::State),
        Command::PendingInteractions => self.pending(session).await.map(Reply::Interactions),
        Command::Message { input, delivery } => self.message(session, &request.request_id, input, delivery).await,
        Command::Respond {
          interaction_id,
          response,
        } => self.respond(session, interaction_id, response).await,
        Command::CancelTurn => {
          self.require_v2_session(session).await?;
          self.ensure_open()?;
          self.check_child().await?;
          self
            .inner
            .transport
            .post(&["api", "session", session, "interrupt"], json!({}))
            .await?;
          Ok(Reply::Acknowledged)
        }
        Command::Metadata | Command::Models => unreachable!(),
      }
    })
  }

  fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.inner.events.subscribe()
  }

  fn close(&self) -> AdapterFuture<'_, Result<(), InteractionError>> {
    Box::pin(async move {
      self.inner.closed.store(true, Ordering::Release);
      let _ = self.inner.stop.send(true);
      if let Some(task) = self.inner.event_task.lock().await.take() {
        task.abort();
        let _ = task.await;
      }
      let child_result = match self.inner.child.lock().await.take() {
        Some(mut child) => managed::stop_child(&mut child).await,
        None => Ok(()),
      };
      let output_tasks = std::mem::take(&mut *self.inner.output_tasks.lock().await);
      for task in output_tasks {
        task.abort();
        let _ = task.await;
      }
      child_result
    })
  }
}

fn capabilities(document: &Value) -> Result<Capabilities, InteractionError> {
  let route = |path: &str, method: &str| document["paths"][path][method].is_object();
  if !route("/api/session/{sessionID}/prompt", "post") || !route("/api/event", "get") {
    return Err(InteractionError::unsupported(
      "OpenCode native V2 prompt admission and event API are required; legacy V1 input is not used",
    ));
  }
  let mut capabilities = Capabilities::default().with(Operation::Message).with(Operation::Queue);
  capabilities.operations.insert(
    Operation::Steer,
    Capability::unavailable(
      "Native V2 delivery steers when busy and starts when idle; active-only admission is not fenced",
    ),
  );
  for (operation, supported) in [
    (Operation::State, route("/api/session/active", "get")),
    (Operation::Models, route("/api/model", "get")),
    (
      Operation::Metadata,
      ["/api/agent", "/api/provider", "/api/command"]
        .iter()
        .all(|path| route(path, "get")),
    ),
    (
      Operation::CancelTurn,
      route("/api/session/{sessionID}/interrupt", "post"),
    ),
    (
      Operation::PendingInteractions,
      [
        "/api/session/{sessionID}/question",
        "/api/session/{sessionID}/permission",
      ]
      .iter()
      .all(|path| route(path, "get")),
    ),
    (
      Operation::Respond,
      [
        "/api/session/{sessionID}/question/{requestID}/reply",
        "/api/session/{sessionID}/question/{requestID}/reject",
        "/api/session/{sessionID}/permission/{requestID}/reply",
      ]
      .iter()
      .all(|path| route(path, "post")),
    ),
  ] {
    capabilities.operations.insert(
      operation,
      if supported {
        Capability::supported()
      } else {
        Capability::unavailable("Required native V2 routes are unavailable on this server")
      },
    );
  }
  Ok(capabilities)
}

fn start_events(
  transport: Transport,
  descriptor: RuntimeDescriptor,
  directory: String,
  workspace_id: Option<String>,
  sender: broadcast::Sender<RuntimeEvent>,
  mut stop: watch::Receiver<bool>,
  valid_instance: Arc<AtomicBool>,
) -> JoinHandle<()> {
  tokio::spawn(async move {
    let sequence = AtomicU64::new(0);
    let emit = |session_id: Option<String>, kind| {
      let _ = sender.send(RuntimeEvent {
        runtime_id: descriptor.runtime_id.clone(),
        instance_id: descriptor.instance_id.clone(),
        session_id,
        sequence: sequence.fetch_add(1, Ordering::Relaxed) + 1,
        kind,
      });
    };
    let mut delay = Duration::from_millis(250);
    loop {
      if *stop.borrow() {
        break;
      }
      let connection = tokio::select! { _ = stop.changed() => break, result = transport.events() => result };
      let result: Result<(), String> = match connection {
        Ok(mut response) => {
          delay = Duration::from_millis(250);
          let mut decoder = wire::SseDecoder::new(1024 * 1024);
          loop {
            let chunk = tokio::select! { _ = stop.changed() => return, chunk = response.chunk() => chunk };
            let (payloads, ended) = match chunk {
              Ok(Some(chunk)) => (decoder.push(&chunk), false),
              Ok(None) => (decoder.finish(), true),
              Err(_) => break Err("OpenCode event stream disconnected".to_owned()),
            };
            let payloads = match payloads {
              Ok(payloads) => payloads,
              Err(error) => break Err(error.to_string()),
            };
            for payload in payloads {
              let value: Value = match serde_json::from_str(&payload) {
                Ok(value) => value,
                Err(_) => {
                  emit(None, RuntimeEventKind::Native(json!({"malformed_event":payload})));
                  continue;
                }
              };
              if value.get("location").is_some_and(|location| {
                location["directory"].as_str() != Some(directory.as_str())
                  || location["workspaceID"].as_str() != workspace_id.as_deref()
              }) {
                continue;
              }
              let session_id = value["data"]["sessionID"].as_str().map(str::to_owned);
              if session_id.is_some() && value.get("location").is_none() {
                continue;
              }
              let native: wire::NativeEvent = match serde_json::from_value(value.clone()) {
                Ok(native) => native,
                Err(_) => {
                  emit(session_id, RuntimeEventKind::Native(value));
                  continue;
                }
              };
              if let Some(kind) = normalize::event_kind(&native) {
                emit(session_id.clone(), kind);
              }
              emit(session_id, RuntimeEventKind::Native(value));
            }
            if ended {
              break Err("OpenCode event stream ended".to_owned());
            }
          }
        }
        Err(error) => Err(error.message),
      };
      if let Err(reason) = result {
        if descriptor.ownership == Ownership::Attached {
          valid_instance.store(false, Ordering::Release);
        }
        emit(
          None,
          RuntimeEventKind::Disconnected {
            reason: format!("{reason}; refresh state and pending interactions after reconnect"),
          },
        );
        if descriptor.ownership == Ownership::Attached {
          break;
        }
      }
      tokio::select! { _ = stop.changed() => break, _ = tokio::time::sleep(delay) => {} }
      delay = delay.saturating_mul(2).min(Duration::from_secs(5));
    }
  })
}

#[cfg(test)]
mod tests;
