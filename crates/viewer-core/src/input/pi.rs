use super::{Admission, DeliveryError, InputTarget, ModelInputDisposition};
use crate::{images::ResolvedImage, input::InputCapability};

#[cfg(unix)]
mod unix {
  use std::fs::OpenOptions;
  use std::io::Read;
  use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
  use std::path::{Component, Path, PathBuf};
  use std::time::Duration;

  use serde::Deserialize;
  use serde_json::{Value, json};
  use sha2::{Digest, Sha256};
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokio::net::UnixStream;

  use super::{Admission, DeliveryError, InputCapability, InputTarget, ModelInputDisposition, ResolvedImage};
  use crate::model::SubmitSessionInputRequest;
  use crate::model::ViewerProvider;
  use crate::models::*;
  use base64::Engine;

  const PROTOCOL: u64 = 1;
  const MAX_DESCRIPTOR_BYTES: usize = 32 * 1024;
  // 24 MiB of validated images expands to 32 MiB of base64, plus text/envelope.
  const MAX_FRAME_BYTES: usize = 34 * 1024 * 1024;
  const MAX_RESPONSE_BYTES: usize = 32 * 1024;
  const BRIDGE_TIMEOUT: Duration = Duration::from_secs(5);

  #[derive(Debug, Deserialize)]
  struct Descriptor {
    protocol: u64,
    provider: String,
    transport: String,
    session_id: String,
    session_file: String,
    instance_id: String,
    socket_path: PathBuf,
    pid: u64,
    token: String,
  }

  pub(super) async fn status(target: &InputTarget) -> Result<InputCapability, DeliveryError> {
    let descriptor = load_descriptor(target).await?;
    query_capability(&descriptor, BRIDGE_TIMEOUT).await
  }

  pub(super) async fn model_status(target: &InputTarget) -> Result<SessionModelStatus, DeliveryError> {
    let descriptor = load_descriptor(target).await?;
    let (status, _) = query_model_status(&descriptor).await?;
    Ok(status)
  }

  async fn query_model_status(descriptor: &Descriptor) -> Result<(SessionModelStatus, String), DeliveryError> {
    let id = uuid::Uuid::new_v4().to_string();
    let response = exchange(descriptor, request(descriptor, "status", &id), false, BRIDGE_TIMEOUT).await?;
    validate_response(&response, &id, false)?;
    if response["type"] != "ready" || response["session_file"] != descriptor.session_file {
      return Err(DeliveryError::not_sent("Pi returned an invalid model status"));
    }
    parse_model_status(descriptor, &response)
  }

  fn parse_model_status(
    descriptor: &Descriptor,
    response: &Value,
  ) -> Result<(SessionModelStatus, String), DeliveryError> {
    if response["session_id"] != descriptor.session_id || response["instance_id"] != descriptor.instance_id {
      return Err(DeliveryError::not_sent(
        "Pi model status targets another session or process",
      ));
    }
    if response["model_controls_version"] != 1 {
      return Ok((
        SessionModelStatus::unavailable("Update the Pi input bridge to enable model discovery and selection"),
        String::new(),
      ));
    }
    let model: Option<ModelIdentity> = serde_json::from_value(response["model"].clone())
      .map_err(|_| DeliveryError::not_sent("Pi returned an invalid model identity"))?;
    if model.as_ref().is_some_and(|model| {
      !valid_identity(&model.model_id)
        || model
          .model_provider
          .as_deref()
          .is_none_or(|provider| !valid_identity(provider))
        || model.display_name.as_deref().is_some_and(|name| !valid_identity(name))
    }) {
      return Err(DeliveryError::not_sent("Pi returned an invalid model identity"));
    }
    let token = response["model_revision"]
      .as_str()
      .filter(|token| valid_identity(token))
      .ok_or_else(|| DeliveryError::not_sent("Pi returned an invalid model revision"))?
      .to_owned();
    let idle = response["state"] == "idle";
    let can_list = response["can_list_models"] == true;
    let can_select = idle && response["can_select_model"] == true && can_list;
    Ok((
      SessionModelStatus {
        state: SessionModelState::live(model, &format!("{}:{token}", descriptor.instance_id)),
        can_list,
        can_select,
        runtime_generation: Some(descriptor.instance_id.clone()),
        updates_pi_default: true,
        supports_model_input: response["model_input_version"] == 1,
        reason: (!can_select).then(|| {
          if !idle {
            "Wait for Pi and queued messages to finish before changing models".into()
          } else {
            "This Pi extension cannot select models".into()
          }
        }),
      },
      token,
    ))
  }

  pub(super) async fn list_models(target: &InputTarget) -> Result<ListSessionModelsResponse, DeliveryError> {
    let descriptor = load_descriptor(target).await?;
    let (status, _) = query_model_status(&descriptor).await?;
    if !status.can_list {
      return Ok(ListSessionModelsResponse {
        status,
        models: Vec::new(),
      });
    }
    let id = uuid::Uuid::new_v4().to_string();
    let response = exchange(&descriptor, request(&descriptor, "models", &id), false, BRIDGE_TIMEOUT).await?;
    validate_response(&response, &id, false)?;
    if response["type"] != "models" {
      return Err(DeliveryError::not_sent("Pi returned an invalid model catalog"));
    }
    let (status, _) = parse_model_status(&descriptor, &response)?;
    let models: Vec<ModelChoice> = serde_json::from_value(response["models"].clone())
      .map_err(|_| DeliveryError::not_sent("Pi returned an invalid model catalog"))?;
    let mut seen = std::collections::HashSet::new();
    if models.len() > 512
      || models.iter().any(|choice| {
        !valid_identity(&choice.model.model_id)
          || choice
            .model
            .model_provider
            .as_deref()
            .is_none_or(|provider| !valid_identity(provider))
          || choice
            .model
            .display_name
            .as_deref()
            .is_some_and(|name| !valid_identity(name))
          || !seen.insert((choice.model.model_provider.clone(), choice.model.model_id.clone()))
      })
    {
      return Err(DeliveryError::not_sent(
        "Pi returned an invalid or oversized model catalog",
      ));
    }
    Ok(ListSessionModelsResponse { status, models })
  }

  pub(super) async fn submit_model_images(
    target: &InputTarget,
    request: &SubmitSessionInputRequest,
    images: &[ResolvedImage],
  ) -> Result<Admission, DeliveryError> {
    let descriptor = load_descriptor(target).await?;
    let selection = if let Some(selection) = request.model_selection.as_ref() {
      let (status, token) = query_model_status(&descriptor).await?;
      if !status.supports_model_input {
        return Err(DeliveryError::not_sent(
          "Update the Pi input bridge to apply the draft model when sending",
        ));
      }
      // Only the bridge may decide idle versus steering. A stale intent is
      // deliberately carried through: busy admission ignores it, whereas idle
      // admission must reject it without applying a model or sending input.
      let token = if status.state.revision == selection.expected_revision
        && status.runtime_generation == selection.runtime_generation
      {
        token
      } else {
        "stale".to_string()
      };
      Some((selection, token))
    } else {
      None
    };
    submit_message_images(
      &descriptor,
      &request.request_id,
      &request.text,
      images,
      selection
        .as_ref()
        .map(|(selection, token)| (*selection, token.as_str())),
      BRIDGE_TIMEOUT,
    )
    .await
  }

  // Node's path.resolve normalizes lexically without resolving symlinks. Using
  // canonicalize here would hash a different path on systems where /var or /tmp
  // is a symlink, and would no longer match the extension's capability record.
  fn normalized_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
      return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
      match component {
        Component::ParentDir => {
          normalized.pop();
        }
        Component::CurDir => {}
        other => normalized.push(other.as_os_str()),
      }
    }
    Some(normalized)
  }

  fn runtime_directory() -> PathBuf {
    if let Some(configured) = std::env::var_os("XDG_RUNTIME_DIR") {
      if let Some(root) = normalized_absolute(Path::new(&configured)) {
        return root.join("tokn-session/input");
      }
    }
    // SAFETY: getuid has no preconditions or pointer arguments.
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("tokn-session-input-{uid}"))
  }

  fn descriptor_path(session_file: &str, runtime: &Path) -> PathBuf {
    let digest = Sha256::digest(session_file.as_bytes());
    runtime.join("pi/sessions").join(format!("{digest:x}.json"))
  }

  async fn load_descriptor(target: &InputTarget) -> Result<Descriptor, DeliveryError> {
    if target.provider != ViewerProvider::Pi {
      return Err(DeliveryError::not_sent("Input requires a Pi session."));
    }
    let session_file = normalized_absolute(&target.session_file)
      .and_then(|path| path.into_os_string().into_string().ok())
      .ok_or_else(|| DeliveryError::not_sent("The Pi session file must have an absolute UTF-8 path."))?;
    let path = descriptor_path(&session_file, &runtime_directory());
    let session_id = target.session_id.clone();
    tokio::task::spawn_blocking(move || read_descriptor(&path, &session_id, &session_file))
      .await
      .map_err(|_| DeliveryError::not_sent("The Pi input bridge descriptor could not be read."))?
  }

  fn read_descriptor(path: &Path, session_id: &str, session_file: &str) -> Result<Descriptor, DeliveryError> {
    if !std::fs::metadata(session_file).is_ok_and(|metadata| metadata.is_file()) {
      return Err(DeliveryError::not_sent("The observed Pi session file is unavailable."));
    }
    // Inspect the opened file, not a prior path stat. NOFOLLOW rejects symlinks;
    // NONBLOCK prevents an invalid FIFO descriptor from blocking a worker.
    let file = OpenOptions::new()
      .read(true)
      .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
      .open(path)
      .map_err(|_| unavailable())?;
    let metadata = file.metadata().map_err(|_| unavailable())?;
    // SAFETY: getuid has no preconditions or pointer arguments.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_file()
      || metadata.len() > MAX_DESCRIPTOR_BYTES as u64
      || metadata.mode() & 0o077 != 0
      || metadata.uid() != uid
    {
      return Err(DeliveryError::not_sent(
        "The Pi input bridge descriptor is not a private, owned regular file.",
      ));
    }
    let mut bytes = Vec::new();
    file
      .take((MAX_DESCRIPTOR_BYTES + 1) as u64)
      .read_to_end(&mut bytes)
      .map_err(|_| unavailable())?;
    if bytes.len() > MAX_DESCRIPTOR_BYTES {
      return Err(DeliveryError::not_sent("The Pi input bridge descriptor is too large."));
    }
    let descriptor: Descriptor = serde_json::from_slice(&bytes)
      .map_err(|_| DeliveryError::not_sent("The Pi input bridge descriptor is invalid."))?;
    if descriptor.protocol != PROTOCOL
      || descriptor.provider != "pi"
      || descriptor.transport != "unix"
      || descriptor.session_id != session_id
      || descriptor.session_file != session_file
      || descriptor.instance_id.is_empty()
      || !descriptor.socket_path.is_absolute()
      || descriptor.pid == 0
      || descriptor.token.is_empty()
    {
      return Err(DeliveryError::not_sent(
        "The Pi input bridge descriptor does not match this session.",
      ));
    }
    Ok(descriptor)
  }

  fn unavailable() -> DeliveryError {
    DeliveryError::not_sent("Pi input is unavailable. Load the input bridge extension in the active Pi process.")
  }

  fn request(descriptor: &Descriptor, kind: &str, request_id: &str) -> Value {
    json!({
      "protocol": PROTOCOL,
      "type": kind,
      "request_id": request_id,
      "token": descriptor.token,
      "session_id": descriptor.session_id,
      "session_file": descriptor.session_file,
      "instance_id": descriptor.instance_id,
    })
  }

  #[cfg(test)]
  async fn query_status(descriptor: &Descriptor, timeout: Duration) -> Result<String, DeliveryError> {
    query_capability(descriptor, timeout)
      .await
      .map(|capability| capability.message)
  }

  async fn query_capability(descriptor: &Descriptor, timeout: Duration) -> Result<InputCapability, DeliveryError> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let response = exchange(descriptor, request(descriptor, "status", &request_id), false, timeout).await?;
    validate_response(&response, &request_id, false)?;
    if response["type"] != "ready"
      || response["session_id"] != descriptor.session_id
      || response["session_file"] != descriptor.session_file
      || response["instance_id"] != descriptor.instance_id
    {
      return Err(DeliveryError::not_sent(
        "The Pi input bridge returned a mismatched status.",
      ));
    }
    let message = match response["state"].as_str() {
      Some("idle") => "Pi is ready for a new message.",
      Some("busy") => "Pi is busy; your message will steer its current turn.",
      _ => {
        return Err(DeliveryError::not_sent(
          "The Pi input bridge returned an invalid status.",
        ));
      }
    };
    Ok(InputCapability {
      message: message.into(),
      images_available: response["images_available"].as_bool().unwrap_or(false),
    })
  }

  #[cfg(test)]
  async fn submit_message(
    descriptor: &Descriptor,
    request_id: &str,
    text: &str,
    timeout: Duration,
  ) -> Result<Admission, DeliveryError> {
    submit_message_images(descriptor, request_id, text, &[], None, timeout).await
  }

  async fn submit_message_images(
    descriptor: &Descriptor,
    request_id: &str,
    text: &str,
    images: &[ResolvedImage],
    selection: Option<(&MessageModelSelection, &str)>,
    timeout: Duration,
  ) -> Result<Admission, DeliveryError> {
    let mut request = request(descriptor, "submit", request_id);
    request["delivery"] = json!("steer");
    if let Some((selection, token)) = selection {
      request["model_selection"] = json!({
        "model_provider": selection.model_provider,
        "model_id": selection.model_id,
        "expected_revision": selection.expected_revision,
        "expected_model_revision": token,
        "runtime_generation": selection.runtime_generation,
      });
    }
    let mut content = Vec::new();
    if !text.trim().is_empty() {
      content.push(json!({ "type": "text", "text": text }));
    }
    for image in images {
      content.push(json!({ "type": "image", "data": base64::engine::general_purpose::STANDARD.encode(image.bytes.as_slice()), "mimeType": image.mime_type }));
    }
    request["content"] = json!(content);
    let response = exchange(descriptor, request, true, timeout).await?;
    if response["protocol"] != PROTOCOL || response["request_id"] != request_id {
      return Err(DeliveryError::uncertain(
        "The Pi input bridge returned an invalid or mismatched response.",
      ));
    }
    let model_result =
      parse_message_model_result(descriptor, &response).map_err(|error| DeliveryError::uncertain(error.message))?;
    validate_response(&response, request_id, true).map_err(|mut error| {
      error.model_result = model_result.clone();
      error
    })?;
    if response["type"] != "admitted"
      || response["session_id"] != descriptor.session_id
      || response["instance_id"] != descriptor.instance_id
    {
      return Err(DeliveryError::uncertain(
        "The Pi input bridge returned a mismatched admission. Check the session before retrying.",
      ));
    }
    if let Some((selection, _)) = selection {
      let valid_result =
        model_result
          .as_ref()
          .is_some_and(|result| match (response["disposition"].as_str(), result.outcome) {
            (Some("queued_steer"), MessageModelOutcome::IgnoredWhileRunning) => true,
            (Some("started"), MessageModelOutcome::Applied | MessageModelOutcome::Unchanged) => {
              result.state.model.as_ref().is_some_and(|model| {
                model.model_provider == selection.model_provider && model.model_id == selection.model_id
              })
            }
            _ => false,
          });
      if !valid_result {
        return Err(DeliveryError::uncertain(
          "Pi did not confirm how the draft model was handled; check the session before retrying",
        ));
      }
    }
    match (response["disposition"].as_str(), model_result) {
      (Some("started"), Some(result)) => Ok(Admission::WithModel {
        disposition: ModelInputDisposition::Started,
        result,
      }),
      (Some("queued_steer"), Some(result)) => Ok(Admission::WithModel {
        disposition: ModelInputDisposition::Steered,
        result,
      }),
      (Some("started"), None) => Ok(Admission::Started),
      (Some("queued_steer"), None) => Ok(Admission::Steered),
      (Some("queued_follow_up"), None) => Ok(Admission::QueuedFollowUp),
      _ => Err(DeliveryError::uncertain(
        "The Pi input bridge returned an unexpected admission. Check the session before retrying.",
      )),
    }
  }

  fn parse_message_model_result(
    descriptor: &Descriptor,
    response: &Value,
  ) -> Result<Option<MessageModelResult>, DeliveryError> {
    let Some(value) = response.get("model_result") else {
      return Ok(None);
    };
    let outcome: MessageModelOutcome = serde_json::from_value(value["outcome"].clone())
      .map_err(|_| DeliveryError::uncertain("Pi returned an invalid message model outcome"))?;
    let model: Option<ModelIdentity> = serde_json::from_value(value["model"].clone())
      .map_err(|_| DeliveryError::uncertain("Pi returned an invalid message model identity"))?;
    if model.as_ref().is_some_and(|model| {
      !valid_identity(&model.model_id)
        || model
          .model_provider
          .as_deref()
          .is_none_or(|provider| !valid_identity(provider))
        || model.display_name.as_deref().is_some_and(|name| !valid_identity(name))
    }) {
      return Err(DeliveryError::uncertain(
        "Pi returned an invalid message model identity",
      ));
    }
    let revision = value["model_revision"]
      .as_str()
      .filter(|revision| valid_identity(revision))
      .ok_or_else(|| DeliveryError::uncertain("Pi returned an invalid message model revision"))?;
    Ok(Some(MessageModelResult {
      outcome,
      state: SessionModelState::live(model, &format!("{}:{revision}", descriptor.instance_id)),
    }))
  }

  fn delivery_error(message: impl Into<String>, may_have_been_sent: bool) -> DeliveryError {
    if may_have_been_sent {
      DeliveryError::uncertain(message)
    } else {
      DeliveryError::not_sent(message)
    }
  }

  fn validate_response(response: &Value, request_id: &str, submission: bool) -> Result<(), DeliveryError> {
    if response["protocol"] != PROTOCOL || response["request_id"] != request_id {
      return Err(delivery_error(
        "The Pi input bridge returned an invalid or mismatched response.",
        submission,
      ));
    }
    if response["type"] != "error" {
      return Ok(());
    }
    // These errors precede sendUserMessage. bridge_unavailable can instead be a
    // throw from that call, so it cannot establish that input was never sent.
    let rejected_before_send = response["may_have_been_sent"] != true
      && matches!(
        response["code"].as_str(),
        Some(
          "instance_mismatch"
            | "invalid_request"
            | "message_invalid"
            | "request_conflict"
            | "session_mismatch"
            | "unauthorized"
            | "unsupported"
        )
      );
    let message = response["message"]
      .as_str()
      .filter(|text| !text.is_empty())
      .unwrap_or("input was rejected");
    let message = if response["code"] == "message_invalid" {
      format!("Pi input bridge: {message}. For multiline messages, update the input bridge extension in Pi.")
    } else {
      format!("Pi input bridge: {message}")
    };
    Err(delivery_error(message, submission && !rejected_before_send))
  }

  async fn exchange(
    descriptor: &Descriptor,
    request: Value,
    submission: bool,
    timeout: Duration,
  ) -> Result<Value, DeliveryError> {
    let mut frame = serde_json::to_vec(&request)
      .map_err(|_| DeliveryError::not_sent("The Pi input request could not be encoded."))?;
    frame.push(b'\n');
    let limit = if request["content"]
      .as_array()
      .is_some_and(|items| items.iter().any(|item| item["type"] == "image"))
    {
      MAX_FRAME_BYTES
    } else {
      MAX_DESCRIPTOR_BYTES
    };
    if frame.len() > limit {
      return Err(DeliveryError::not_sent(
        "This message exceeds the Pi input bridge's encoded request size limit.",
      ));
    }
    let mut sending = false;
    let response = tokio::time::timeout(timeout, async {
      let mut stream = UnixStream::connect(&descriptor.socket_path)
        .await
        .map_err(|_| unavailable())?;
      sending = true;
      stream.write_all(&frame).await.map_err(|_| {
        delivery_error(
          "The connection to Pi failed while sending. Check the session before retrying.",
          submission,
        )
      })?;
      let mut response = Vec::new();
      let mut buffer = [0_u8; 4096];
      loop {
        let count = stream.read(&mut buffer).await.map_err(|_| {
          delivery_error(
            "The Pi input bridge response could not be read. Check the session before retrying.",
            submission,
          )
        })?;
        if count == 0 {
          return Err(delivery_error(
            "Pi closed the connection without confirming delivery. Check the session before retrying.",
            submission,
          ));
        }
        let end = buffer[..count].iter().position(|byte| *byte == b'\n');
        let line_bytes = end.unwrap_or(count);
        let response_limit = if matches!(request["type"].as_str(), Some("models")) {
          1024 * 1024
        } else {
          MAX_RESPONSE_BYTES
        };
        if response.len() + line_bytes > response_limit {
          return Err(delivery_error("The Pi input bridge response is too large.", submission));
        }
        response.extend_from_slice(&buffer[..line_bytes]);
        if end.is_some() {
          return serde_json::from_slice(&response)
            .map_err(|_| delivery_error("The Pi input bridge returned invalid JSON.", submission));
        }
      }
    })
    .await;
    match response {
      Ok(response) => response,
      Err(_) => Err(delivery_error(
        "The Pi input bridge timed out. Check the session before retrying.",
        submission && sending,
      )),
    }
  }

  #[cfg(test)]
  mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;

    use super::*;

    struct Fixture {
      root: TempDir,
      value: Value,
    }

    impl Fixture {
      fn new() -> Self {
        // A short path also fits macOS's Unix socket pathname bound.
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let session_file = root.path().join("session.jsonl");
        std::fs::write(&session_file, "{}\n").unwrap();
        let value = json!({
          "protocol": 1,
          "provider": "pi",
          "transport": "unix",
          "session_id": "session-1",
          "session_file": session_file,
          "instance_id": "instance-1",
          "socket_path": root.path().join("bridge.sock"),
          "pid": 1234,
          "token": "test-token",
        });
        let fixture = Self { root, value };
        fixture.write_descriptor(&fixture.value);
        fixture
      }

      fn path(&self) -> PathBuf {
        self.root.path().join("descriptor.json")
      }

      fn write_descriptor(&self, value: &Value) {
        std::fs::write(self.path(), serde_json::to_vec(value).unwrap()).unwrap();
        std::fs::set_permissions(self.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
      }

      fn descriptor(&self) -> Result<Descriptor, DeliveryError> {
        read_descriptor(&self.path(), "session-1", self.value["session_file"].as_str().unwrap())
      }

      fn serve(&self, respond: impl FnOnce(&Value) -> Vec<u8> + Send + 'static) -> JoinHandle<Value> {
        let listener = UnixListener::bind(self.value["socket_path"].as_str().unwrap()).unwrap();
        tokio::spawn(async move {
          let (stream, _) = listener.accept().await.unwrap();
          let mut reader = BufReader::new(stream);
          let mut line = String::new();
          reader.read_line(&mut line).await.unwrap();
          let request: Value = serde_json::from_str(&line).unwrap();
          reader.get_mut().write_all(&respond(&request)).await.unwrap();
          request
        })
      }
    }

    fn frame(value: Value) -> Vec<u8> {
      let mut bytes = serde_json::to_vec(&value).unwrap();
      bytes.push(b'\n');
      bytes
    }

    fn admitted(request: &Value, disposition: &str) -> Value {
      json!({
        "protocol": 1,
        "type": "admitted",
        "request_id": request["request_id"],
        "session_id": request["session_id"],
        "instance_id": request["instance_id"],
        "disposition": disposition,
      })
    }

    #[test]
    fn descriptor_hash_matches_node_resolve_without_resolving_symlinks() {
      let path = normalized_absolute(Path::new("/sessions/./other/../session.jsonl")).unwrap();
      assert_eq!(path, Path::new("/sessions/session.jsonl"));
      assert_eq!(
        normalized_absolute(Path::new("/../../session.jsonl")).unwrap(),
        Path::new("/session.jsonl")
      );
      assert!(normalized_absolute(Path::new("session.jsonl")).is_none());
      assert_eq!(
        descriptor_path(path.to_str().unwrap(), Path::new("/runtime")),
        Path::new("/runtime/pi/sessions/1901b3dd699c54c5834719e831b1338a21f29a5ad9fa6a61aeefb0a297f6d0f8.json"),
      );
    }

    #[test]
    fn descriptor_requires_matching_protocol_and_session_identity() {
      let fixture = Fixture::new();
      assert!(fixture.descriptor().is_ok());
      for (field, value) in [
        ("protocol", json!(2)),
        ("provider", json!("codex")),
        ("transport", json!("tcp")),
        ("session_id", json!("other-session")),
        ("session_file", json!("/other/session.jsonl")),
        ("instance_id", json!("")),
        ("socket_path", json!("relative.sock")),
        ("pid", json!(0)),
        ("token", json!("")),
      ] {
        let mut descriptor = fixture.value.clone();
        descriptor[field] = value;
        fixture.write_descriptor(&descriptor);
        assert!(!fixture.descriptor().unwrap_err().may_have_been_sent, "{field}");
      }
    }

    #[test]
    fn descriptor_rejects_public_files_symlinks_and_oversized_content() {
      let fixture = Fixture::new();
      std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
      assert!(fixture.descriptor().unwrap_err().message.contains("private"));
      fixture.write_descriptor(&fixture.value);
      let alternate = fixture.root.path().join("alternate.json");
      std::fs::rename(fixture.path(), &alternate).unwrap();
      symlink(&alternate, fixture.path()).unwrap();
      assert!(fixture.descriptor().is_err());
      std::fs::remove_file(fixture.path()).unwrap();
      std::fs::rename(alternate, fixture.path()).unwrap();
      std::fs::write(fixture.path(), vec![b' '; MAX_DESCRIPTOR_BYTES + 1]).unwrap();
      assert!(fixture.descriptor().is_err());
    }

    #[test]
    fn descriptor_requires_an_existing_session_file() {
      let fixture = Fixture::new();
      std::fs::remove_file(fixture.value["session_file"].as_str().unwrap()).unwrap();
      assert!(
        fixture
          .descriptor()
          .unwrap_err()
          .message
          .contains("session file is unavailable")
      );
    }

    #[tokio::test]
    async fn status_authenticates_and_reports_idle_or_busy() {
      for state in ["idle", "busy"] {
        let fixture = Fixture::new();
        let server = fixture.serve(move |request| {
          frame(json!({
            "protocol": 1,
            "type": "ready",
            "request_id": request["request_id"],
            "session_id": request["session_id"],
            "session_file": request["session_file"],
            "instance_id": request["instance_id"],
            "state": state,
          }))
        });
        let status = query_status(&fixture.descriptor().unwrap(), BRIDGE_TIMEOUT)
          .await
          .unwrap();
        assert!(status.contains(if state == "idle" { "ready" } else { "steer" }));
        let request = server.await.unwrap();
        assert_eq!(request["type"], "status");
        assert_eq!(request["token"], "test-token");
      }
    }

    #[tokio::test]
    async fn status_rejects_a_different_process_or_session() {
      for field in [
        "protocol",
        "request_id",
        "session_id",
        "session_file",
        "instance_id",
        "state",
      ] {
        let fixture = Fixture::new();
        let server = fixture.serve(move |request| {
          let mut response = json!({
            "protocol": 1,
            "type": "ready",
            "request_id": request["request_id"],
            "session_id": request["session_id"],
            "session_file": request["session_file"],
            "instance_id": request["instance_id"],
            "state": "idle",
          });
          response[field] = json!("mismatch");
          frame(response)
        });
        let error = query_status(&fixture.descriptor().unwrap(), BRIDGE_TIMEOUT)
          .await
          .unwrap_err();
        assert!(!error.may_have_been_sent);
        server.await.unwrap();
      }
    }

    #[tokio::test]
    async fn legacy_bridge_status_does_not_advertise_images() {
      for advertised in [None, Some(false), Some(true)] {
        let fixture = Fixture::new();
        let server = fixture.serve(move |request| {
          let mut response = json!({"protocol":1,"type":"ready","request_id":request["request_id"],"session_id":request["session_id"],"session_file":request["session_file"],"instance_id":request["instance_id"],"state":"idle"});
          if let Some(available) = advertised { response["images_available"] = json!(available); }
          frame(response)
        });
        assert_eq!(
          query_capability(&fixture.descriptor().unwrap(), BRIDGE_TIMEOUT)
            .await
            .unwrap()
            .images_available,
          advertised.unwrap_or(false)
        );
        server.await.unwrap();
      }
    }

    #[test]
    fn model_status_requires_capability_and_binds_revision_to_process_and_native_model() {
      let fixture = Fixture::new();
      let descriptor = fixture.descriptor().unwrap();
      let mut response = json!({"protocol":1,"type":"ready","session_id":descriptor.session_id,"session_file":descriptor.session_file,
        "instance_id":descriptor.instance_id,"state":"idle","model_controls_version":1,"can_list_models":true,"can_select_model":true,
        "model":{"model_provider":"synthetic","model_id":"text","display_name":"Text"},"model_revision":"revision-one"});
      let first = parse_model_status(&descriptor, &response).unwrap().0;
      assert!(first.can_list && first.can_select && first.updates_pi_default);
      assert!(!first.supports_model_input);
      response["model_input_version"] = json!(1);
      assert!(
        parse_model_status(&descriptor, &response)
          .unwrap()
          .0
          .supports_model_input
      );
      response["model_revision"] = json!("revision-two");
      assert_ne!(
        parse_model_status(&descriptor, &response).unwrap().0.state.revision,
        first.state.revision
      );
      response["state"] = json!("busy");
      assert!(!parse_model_status(&descriptor, &response).unwrap().0.can_select);
      response.as_object_mut().unwrap().remove("model_controls_version");
      let legacy = parse_model_status(&descriptor, &response).unwrap().0;
      assert!(!legacy.can_select && !legacy.can_list);
      assert!(legacy.state.model.is_none());
    }

    #[tokio::test]
    async fn image_submission_keeps_bytes_separate_from_paths_and_omits_empty_text() {
      let fixture = Fixture::new();
      let server = fixture.serve(move |request| frame(admitted(request, "started")));
      let images = [ResolvedImage {
        image_id: "image".into(),
        mime_type: "image/png".into(),
        sha256: "hash".into(),
        path: "/host/private.png".into(),
        bytes: std::sync::Arc::new(vec![1, 2, 3]),
      }];
      submit_message_images(
        &fixture.descriptor().unwrap(),
        "image-request",
        "",
        &images,
        None,
        BRIDGE_TIMEOUT,
      )
      .await
      .unwrap();
      let request = server.await.unwrap();
      assert_eq!(
        request["content"],
        json!([{"type":"image", "data":"AQID", "mimeType":"image/png"}])
      );
      assert!(!request.to_string().contains("/host/private.png"));
    }

    #[tokio::test]
    async fn submission_preserves_multiline_text_and_uses_steer_delivery() {
      for disposition in ["started", "queued_steer"] {
        let fixture = Fixture::new();
        let server = fixture.serve(move |request| frame(admitted(request, disposition)));
        let text = "Explain this:\n\tfirst line\r\n第二行";
        let result = submit_message(&fixture.descriptor().unwrap(), "request-1", text, BRIDGE_TIMEOUT)
          .await
          .unwrap();
        assert!(matches!(
          (disposition, result),
          ("started", Admission::Started) | ("queued_steer", Admission::Steered)
        ));
        let request = server.await.unwrap();
        assert_eq!(request["protocol"], 1);
        assert_eq!(request["type"], "submit");
        assert_eq!(request["token"], "test-token");
        assert_eq!(request["instance_id"], "instance-1");
        assert_eq!(request["session_id"], "session-1");
        assert_eq!(request["session_file"], fixture.value["session_file"]);
        assert_eq!(request["request_id"], "request-1");
        assert_eq!(request["delivery"], "steer");
        assert_eq!(request["content"], json!([{ "type": "text", "text": text }]));
      }
    }

    fn model_result(outcome: &str) -> Value {
      json!({
        "outcome": outcome,
        "model": {"model_provider":"test","model_id":"active","display_name":"Active"},
        "model_revision":"native-revision",
      })
    }

    #[tokio::test]
    async fn busy_admission_returns_actual_model_and_carries_stale_intent_without_applying_it() {
      let fixture = Fixture::new();
      let server = fixture.serve(move |request| {
        let mut response = admitted(request, "queued_steer");
        response["model_result"] = model_result("ignored_while_running");
        frame(response)
      });
      let selection = MessageModelSelection {
        model_provider: Some("other".into()),
        model_id: "draft".into(),
        expected_revision: "viewer-revision".into(),
        runtime_generation: Some("old-process".into()),
      };
      let response = submit_message_images(
        &fixture.descriptor().unwrap(),
        "busy",
        "steer",
        &[],
        Some((&selection, "stale")),
        BRIDGE_TIMEOUT,
      )
      .await
      .unwrap();
      let Admission::WithModel {
        disposition: ModelInputDisposition::Steered,
        result,
      } = response
      else {
        panic!("expected model-aware steering admission")
      };
      assert_eq!(result.outcome, MessageModelOutcome::IgnoredWhileRunning);
      assert_eq!(result.state.model.as_ref().unwrap().model_id, "active");
      assert_eq!(
        result.state,
        SessionModelState::live(result.state.model.clone(), "instance-1:native-revision",)
      );
      let request = server.await.unwrap();
      assert_eq!(request["delivery"], "steer");
      assert_eq!(
        request["model_selection"],
        json!({
          "model_provider":"other", "model_id":"draft", "expected_revision":"viewer-revision",
          "expected_model_revision":"stale", "runtime_generation":"old-process",
        })
      );
    }

    #[tokio::test]
    async fn draft_model_admission_requires_a_confirmed_matching_model_result() {
      for result in [
        Value::Null,
        model_result("applied"),
        model_result("ignored_while_running"),
      ] {
        let fixture = Fixture::new();
        let server = fixture.serve(move |request| {
          let mut response = admitted(request, "started");
          if !result.is_null() {
            response["model_result"] = result;
          }
          frame(response)
        });
        let selection = MessageModelSelection {
          model_provider: Some("test".into()),
          model_id: "other".into(),
          expected_revision: "revision".into(),
          runtime_generation: Some("instance-1".into()),
        };
        assert!(
          submit_message_images(
            &fixture.descriptor().unwrap(),
            "draft",
            "prompt",
            &[],
            Some((&selection, "native")),
            BRIDGE_TIMEOUT,
          )
          .await
          .unwrap_err()
          .may_have_been_sent
        );
        server.await.unwrap();
      }
    }

    #[tokio::test]
    async fn model_result_survives_an_uncertain_prompt_error() {
      let fixture = Fixture::new();
      let server = fixture.serve(move |request| {
        frame(json!({
          "protocol":1, "type":"error", "request_id":request["request_id"], "code":"unsupported",
          "message":"model hook failed after mutation", "may_have_been_sent":true,
          "model_result":model_result("applied"),
        }))
      });
      let error = submit_message(&fixture.descriptor().unwrap(), "partial", "prompt", BRIDGE_TIMEOUT)
        .await
        .unwrap_err();
      assert!(error.may_have_been_sent);
      let result = error.model_result.unwrap();
      assert_eq!(result.outcome, MessageModelOutcome::Applied);
      assert_eq!(result.state.model.unwrap().model_id, "active");
      server.await.unwrap();
    }

    #[tokio::test]
    async fn submission_distinguishes_rejection_from_uncertain_admission() {
      for (code, uncertain) in [
        ("unauthorized", false),
        ("message_invalid", false),
        ("bridge_unavailable", true),
        ("future_error", true),
      ] {
        let fixture = Fixture::new();
        let server = fixture.serve(move |request| {
          frame(json!({
            "protocol": 1,
            "type": "error",
            "request_id": request["request_id"],
            "code": code,
            "message": "Rejected",
          }))
        });
        let error = submit_message(&fixture.descriptor().unwrap(), "request-1", "Hello", BRIDGE_TIMEOUT)
          .await
          .unwrap_err();
        assert_eq!(error.may_have_been_sent, uncertain, "{code}");
        if code == "message_invalid" {
          assert!(error.message.contains("update the input bridge"));
        }
        server.await.unwrap();
      }
    }

    #[tokio::test]
    async fn mismatched_admissions_are_uncertain() {
      for field in [
        "protocol",
        "type",
        "request_id",
        "session_id",
        "instance_id",
        "disposition",
      ] {
        let fixture = Fixture::new();
        let server = fixture.serve(move |request| {
          let mut response = admitted(request, "started");
          response[field] = json!("mismatch");
          frame(response)
        });
        assert!(
          submit_message(&fixture.descriptor().unwrap(), "request-1", "Hello", BRIDGE_TIMEOUT)
            .await
            .unwrap_err()
            .may_have_been_sent
        );
        server.await.unwrap();
      }
    }

    #[tokio::test]
    async fn invalid_or_missing_responses_are_uncertain_after_submission() {
      for response in [b"invalid\n".to_vec(), Vec::new(), vec![b'a'; MAX_RESPONSE_BYTES + 1]] {
        let fixture = Fixture::new();
        let server = fixture.serve(move |_| response);
        let error = submit_message(&fixture.descriptor().unwrap(), "request-1", "Hello", BRIDGE_TIMEOUT)
          .await
          .unwrap_err();
        assert!(error.may_have_been_sent);
        server.await.unwrap();
      }
    }

    #[tokio::test]
    async fn timeout_after_submission_is_uncertain_but_connect_failure_is_not() {
      let fixture = Fixture::new();
      let descriptor = fixture.descriptor().unwrap();
      let error = submit_message(&descriptor, "request-1", "Hello", BRIDGE_TIMEOUT)
        .await
        .unwrap_err();
      assert!(!error.may_have_been_sent);
      let listener = UnixListener::bind(&descriptor.socket_path).unwrap();
      let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
      });
      let error = submit_message(&descriptor, "request-1", "Hello", Duration::from_millis(50))
        .await
        .unwrap_err();
      assert!(error.may_have_been_sent);
      assert!(error.message.contains("timed out"));
      server.abort();
    }

    #[tokio::test]
    async fn oversized_encoded_request_is_rejected_before_connecting() {
      let fixture = Fixture::new();
      let error = submit_message(
        &fixture.descriptor().unwrap(),
        "request-1",
        &"中".repeat(16 * 1024),
        BRIDGE_TIMEOUT,
      )
      .await
      .unwrap_err();
      assert!(!error.may_have_been_sent);
      assert!(error.message.contains("encoded request size limit"));
    }
  }
}

pub(super) async fn status(target: &InputTarget) -> Result<InputCapability, DeliveryError> {
  #[cfg(unix)]
  return unix::status(target).await;
  #[cfg(not(unix))]
  {
    let _ = target;
    Err(DeliveryError::not_sent(
      "Pi input requires a Unix system with the Pi input bridge extension loaded.",
    ))
  }
}

pub(crate) async fn model_status(target: &InputTarget) -> Result<crate::models::SessionModelStatus, DeliveryError> {
  #[cfg(unix)]
  return unix::model_status(target).await;
  #[cfg(not(unix))]
  {
    let _ = target;
    Err(DeliveryError::not_sent(
      "Pi model controls require the Unix input bridge",
    ))
  }
}

pub(crate) async fn list_models(
  target: &InputTarget,
) -> Result<crate::models::ListSessionModelsResponse, DeliveryError> {
  #[cfg(unix)]
  return unix::list_models(target).await;
  #[cfg(not(unix))]
  {
    let _ = target;
    Err(DeliveryError::not_sent(
      "Pi model controls require the Unix input bridge",
    ))
  }
}

pub(super) async fn submit_model_images(
  target: &InputTarget,
  request: &crate::model::SubmitSessionInputRequest,
  images: &[ResolvedImage],
) -> Result<Admission, DeliveryError> {
  #[cfg(unix)]
  return unix::submit_model_images(target, request, images).await;
  #[cfg(not(unix))]
  {
    let _ = (target, request, images);
    Err(DeliveryError::not_sent(
      "Pi input requires a Unix system with the Pi input bridge extension loaded.",
    ))
  }
}
