//! Compatibility transport for the private Codex Desktop IPC router.
//!
//! Sending is deliberately limited to the owning Desktop process. In
//! particular, an unavailable router never starts another Codex process.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use uuid::Uuid;

use super::{Admission, DeliveryError, InputTarget};
use crate::images::ResolvedImage;
use crate::models::{ModelIdentity, SessionModelState, SessionModelStatus, valid_identity};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
// The router can spend ten seconds discovering an owner before forwarding.
// Keep this below the browser's 30-second request deadline, including the
// separate five-second connection/initialization budget.
const SUBMISSION_TIMEOUT: Duration = Duration::from_secs(20);
const START_TURN_METHOD: &str = "thread-follower-start-turn";

trait IpcStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IpcStream for T {}

struct DesktopClient {
  stream: Box<dyn IpcStream>,
  client_id: String,
  request_timeout: Duration,
}

pub(super) async fn status(_target: &InputTarget) -> Result<String, DeliveryError> {
  DesktopClient::connect(&desktop_endpoint()?, REQUEST_TIMEOUT).await?;
  Ok("Codex Desktop is available. Open this session in Codex Desktop to receive messages.".to_owned())
}

pub(crate) async fn model_status(target: &InputTarget) -> Result<SessionModelStatus, DeliveryError> {
  let endpoint = desktop_endpoint().map_err(|_| model_unavailable())?;
  let mut client = DesktopClient::connect(&endpoint, REQUEST_TIMEOUT)
    .await
    .map_err(|_| model_unavailable())?;
  client.read_model_status(&target.session_id).await
}

pub(crate) async fn submit_images(
  target: &InputTarget,
  request_id: &str,
  text: &str,
  images: &[ResolvedImage],
) -> Result<Admission, DeliveryError> {
  let mut client = DesktopClient::connect(&desktop_endpoint()?, REQUEST_TIMEOUT).await?;
  client.request_timeout = SUBMISSION_TIMEOUT;
  client.submit_images(&target.session_id, request_id, text, images).await
}

pub(crate) async fn submit_question(
  target: &InputTarget,
  request_id: &str,
  text: &str,
) -> Result<Admission, DeliveryError> {
  let mut client = DesktopClient::connect(&desktop_endpoint()?, REQUEST_TIMEOUT).await?;
  client.request_timeout = SUBMISSION_TIMEOUT;
  client.submit_question(&target.session_id, request_id, text).await
}

fn desktop_endpoint() -> Result<PathBuf, DeliveryError> {
  #[cfg(windows)]
  {
    Ok(PathBuf::from(r"\\.\pipe\codex-ipc"))
  }
  #[cfg(not(windows))]
  {
    let codex_home = std::env::var_os("CODEX_HOME")
      .filter(|value| !value.is_empty())
      .map(PathBuf::from)
      .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
      .ok_or_else(|| DeliveryError::not_sent("Cannot locate the Codex Desktop IPC endpoint."))?;
    Ok(codex_home.join("ipc").join("ipc.sock"))
  }
}

fn model_unavailable() -> DeliveryError {
  DeliveryError::not_sent(
    "Live model settings are unavailable for this session in Codex Desktop. Open it there and retry; recorded model information is shown when available.",
  )
}

impl DesktopClient {
  async fn read_model_status(&mut self, session_id: &str) -> Result<SessionModelStatus, DeliveryError> {
    // Optional display metadata has one bounded budget. Router-wide negative
    // discovery can take ten seconds, so a deadline means unavailable context,
    // not a failed message. Never resume a thread or retry through another owner.
    match timeout(self.request_timeout, self.read_owned_model_status(session_id)).await {
      Ok(Ok(status)) => Ok(status),
      _ => Err(model_unavailable()),
    }
  }

  async fn read_owned_model_status(&mut self, session_id: &str) -> Result<SessionModelStatus, DeliveryError> {
    let id = Uuid::new_v4().to_string();
    // Desktop's explicit discovery uses cached ownership or a short role probe.
    // Ordinary follower discovery can wait on unrelated peers for much longer.
    let discovery = self
      .request(
        json!({
          "type":"request", "requestId":id, "sourceClientId":self.client_id,
          "version":1, "method":"thread-owner-discovery",
          "params":{"hostId":"local", "conversationId":session_id},
          "timeoutMs":self.request_timeout.as_millis() as u64
        }),
        &id,
        "thread-owner-discovery",
        false,
      )
      .await?;
    let owner = discovery["handledByClientId"]
      .as_str()
      .filter(|value| valid_identity(value))
      .ok_or_else(model_unavailable)?
      .to_owned();
    let id = Uuid::new_v4().to_string();
    let response = self
      .request(
        json!({
          "type":"request", "requestId":id, "sourceClientId":self.client_id,
          "version":1, "method":"thread-follower-read-model-settings", "targetClientId":owner,
          "params":{"conversationId":session_id}, "timeoutMs":self.request_timeout.as_millis() as u64
        }),
        &id,
        "thread-follower-read-model-settings",
        false,
      )
      .await?;
    if response["handledByClientId"].as_str() != Some(owner.as_str()) {
      return Err(model_unavailable());
    }
    let settings = &response["result"]["settings"];
    let model_id = settings["model"]
      .as_str()
      .filter(|value| valid_identity(value))
      .ok_or_else(|| DeliveryError::not_sent("Codex Desktop has no live model state for this session"))?;
    let generation = format!("{session_id}:{owner}");
    Ok(SessionModelStatus {
      state: SessionModelState::live(Some(ModelIdentity {
        model_provider: settings["modelProvider"].as_str().filter(|value| valid_identity(value)).map(str::to_owned),
        model_id: model_id.to_owned(), display_name: None,
      }), &generation),
      can_select: false, can_list: false,
      reason: Some("Codex Desktop does not expose a verified model catalog through its input connection. Change models in Codex Desktop.".into()),
      runtime_generation: Some(generation), updates_default: false, supports_model_input: false,
    })
  }

  async fn connect(endpoint: &std::path::Path, request_timeout: Duration) -> Result<Self, DeliveryError> {
    timeout(request_timeout, Self::connect_and_initialize(endpoint, request_timeout))
      .await
      .map_err(|_| DeliveryError::not_sent("Connecting to Codex Desktop timed out."))?
  }

  async fn connect_and_initialize(
    endpoint: &std::path::Path,
    request_timeout: Duration,
  ) -> Result<Self, DeliveryError> {
    let stream = connect_stream(endpoint)
      .await
      .map_err(|error| DeliveryError::not_sent(format!("Codex Desktop is unavailable: {error}")))?;
    let mut client = Self {
      stream,
      client_id: "initializing-client".to_owned(),
      request_timeout,
    };
    let request_id = Uuid::new_v4().to_string();
    let response = client
      .request(
        json!({
          "type": "request",
          "requestId": request_id,
          "sourceClientId": client.client_id,
          "version": 0,
          "method": "initialize",
          "params": { "clientType": "tokn-session-viewer" }
        }),
        &request_id,
        "initialize",
        false,
      )
      .await?;
    client.client_id = response
      .get("result")
      .and_then(|result| result.get("clientId"))
      .and_then(nonempty_string)
      .ok_or_else(|| DeliveryError::not_sent("Codex Desktop returned an invalid initialization response."))?
      .to_owned();
    Ok(client)
  }

  #[cfg(test)]
  async fn submit(&mut self, session_id: &str, request_id: &str, text: &str) -> Result<Admission, DeliveryError> {
    self.submit_images(session_id, request_id, text, &[]).await
  }

  async fn submit_images(
    &mut self,
    session_id: &str,
    request_id: &str,
    text: &str,
    images: &[ResolvedImage],
  ) -> Result<Admission, DeliveryError> {
    if session_id.trim().is_empty() || request_id.trim().is_empty() || (text.trim().is_empty() && images.is_empty()) {
      return Err(DeliveryError::not_sent(
        "A session, request ID, and message are required.",
      ));
    }
    let mut input = Vec::new();
    if !text.trim().is_empty() {
      input.push(json!({ "type": "text", "text": text, "text_elements": [] }));
    }
    for image in images {
      input.push(json!({ "type": "localImage", "path": image.path }));
    }
    // The wire request is unique for this connection. The user-message ID is
    // stable across the viewer's delivery bookkeeping, not a promise that the
    // private Desktop protocol can safely retry an ambiguous submission.
    let wire_request_id = Uuid::new_v4().to_string();
    self
      .request(
        json!({
          "type": "request",
          "requestId": wire_request_id,
          "sourceClientId": self.client_id,
          // Version 2 observed in Codex Desktop 26.901.41123. Both the
          // operation envelope and the nested thread ID are required.
          "version": 2,
          "method": START_TURN_METHOD,
          "params": {
            "conversationId": session_id,
            "turnStart": {
              "request": {
                "threadId": session_id,
                // Desktop renders this input before the app server normalizes
                // it. Its text renderer requires the annotation array even
                // for plain text, or the owning conversation fails to render.
                "input": input,
                "clientUserMessageId": request_id,
                "additionalContext": null
              },
              "context": { "inheritThreadSettings": true }
            }
          },
          "timeoutMs": self.request_timeout.as_millis() as u64
        }),
        &wire_request_id,
        START_TURN_METHOD,
        true,
      )
      .await?;
    Ok(Admission::Accepted)
  }

  async fn submit_question(
    &mut self,
    session_id: &str,
    request_id: &str,
    text: &str,
  ) -> Result<Admission, DeliveryError> {
    let wire_id = Uuid::new_v4().to_string();
    // Observed in ChatGPT/Codex Desktop's follower router: steering adds a
    // structured answer to the active turn without starting another turn.
    self
      .request(
        json!({
          "type": "request", "requestId": wire_id, "sourceClientId": self.client_id,
          "version": 1, "method": "thread-follower-steer-turn",
          "params": { "conversationId": session_id,
            "input": [{ "type": "text", "text": text, "text_elements": [] }],
            "clientUserMessageId": request_id, "restoreMessage": {
              "cwd": null, "context": { "workspaceRoots": [], "commentAttachments": [], "collaborationMode": null },
              "responsesapiClientMetadata": null, "serverQueuedMessageId": null
            },
            "serviceTier": null, "attachments": [], "additionalContext": null, "toolOutput": null },
          "timeoutMs": self.request_timeout.as_millis() as u64
        }),
        &wire_id,
        "thread-follower-steer-turn",
        true,
      )
      .await?;
    Ok(Admission::Accepted)
  }

  async fn request(
    &mut self,
    request: Value,
    request_id: &str,
    method: &str,
    may_deliver_message: bool,
  ) -> Result<Value, DeliveryError> {
    // Serialization and the size check happen before any write. Once a write
    // begins, even an I/O error cannot prove the message was not delivered.
    let frame = encode_frame(&request).map_err(DeliveryError::not_sent)?;
    let result = timeout(self.request_timeout, async {
      self.stream.write_all(&frame).await.map_err(|error| error.to_string())?;
      loop {
        let response = read_frame(&mut self.stream).await?;
        let object = response
          .as_object()
          .ok_or("Codex Desktop returned a non-object IPC frame.")?;
        match object.get("type").and_then(Value::as_str) {
          Some("client-discovery-request") => {
            let discovery_id = object
              .get("requestId")
              .and_then(nonempty_string)
              .ok_or("Codex Desktop sent an invalid client-discovery request.")?;
            let discovery = encode_frame(&json!({
              "type": "client-discovery-response",
              "requestId": discovery_id,
              "response": { "canHandle": false }
            }))?;
            self
              .stream
              .write_all(&discovery)
              .await
              .map_err(|error| error.to_string())?;
          }
          Some("response") => {
            let response_id = object
              .get("requestId")
              .and_then(nonempty_string)
              .ok_or("Codex Desktop returned a response without a request ID.")?;
            if response_id == request_id {
              return Ok::<Value, String>(response);
            }
          }
          _ => {}
        }
      }
    })
    .await;
    let response = match result {
      Ok(Ok(response)) => response,
      Ok(Err(error)) => return Err(delivery_failure(may_deliver_message, error)),
      Err(_) => {
        return Err(delivery_failure(
          may_deliver_message,
          format!("Codex Desktop {method} timed out."),
        ));
      }
    };
    match response.get("resultType").and_then(Value::as_str) {
      Some("success")
        if response.get("method").and_then(Value::as_str) == Some(method)
          && response.get("handledByClientId").and_then(nonempty_string).is_some() =>
      {
        Ok(response)
      }
      Some("error") => {
        let Some(error) = response.get("error").and_then(nonempty_string) else {
          return Err(delivery_failure(
            may_deliver_message,
            "Codex Desktop returned an invalid error response.",
          ));
        };
        if error == "no-client-found" || error.starts_with("no-client-found:") {
          return Err(DeliveryError::not_sent(
            "No compatible Codex Desktop owner found. Open this session in a current Codex Desktop build, then send again.",
          ));
        }
        if matches!(error, "request-version-mismatch" | "no-handler-for-request") {
          return Err(DeliveryError::not_sent(format!(
            "Codex Desktop does not support this message request ({error}). Update Codex Desktop before trying again."
          )));
        }
        // Other owner errors can occur after beginning work. They cannot
        // establish non-delivery, so never retry or change transports here.
        Err(delivery_failure(
          may_deliver_message,
          format!("Codex Desktop rejected the request: {error}"),
        ))
      }
      _ => Err(delivery_failure(
        may_deliver_message,
        "Codex Desktop returned an incompatible IPC response.",
      )),
    }
  }
}

fn delivery_failure(may_have_been_sent: bool, message: impl Into<String>) -> DeliveryError {
  if may_have_been_sent {
    DeliveryError::uncertain(message)
  } else {
    DeliveryError::not_sent(message)
  }
}

fn nonempty_string(value: &Value) -> Option<&str> {
  value.as_str().filter(|text| !text.trim().is_empty())
}

fn encode_frame(message: &Value) -> Result<Vec<u8>, String> {
  let payload = serde_json::to_vec(message).map_err(|error| format!("Cannot encode Codex Desktop request: {error}"))?;
  if payload.len() > MAX_FRAME_BYTES {
    return Err("Codex Desktop IPC frame exceeds the size limit.".to_owned());
  }
  let mut frame = Vec::with_capacity(4 + payload.len());
  frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  frame.extend_from_slice(&payload);
  Ok(frame)
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<Value, String> {
  let length = stream
    .read_u32_le()
    .await
    .map_err(|error| format!("Codex Desktop IPC read failed: {error}"))? as usize;
  if length == 0 || length > MAX_FRAME_BYTES {
    return Err("Codex Desktop returned an invalid IPC frame size.".to_owned());
  }
  let mut payload = vec![0; length];
  stream
    .read_exact(&mut payload)
    .await
    .map_err(|error| format!("Codex Desktop IPC read failed: {error}"))?;
  serde_json::from_slice(&payload).map_err(|error| format!("Codex Desktop returned invalid IPC JSON: {error}"))
}

async fn connect_stream(endpoint: &std::path::Path) -> std::io::Result<Box<dyn IpcStream>> {
  #[cfg(unix)]
  {
    Ok(Box::new(tokio::net::UnixStream::connect(endpoint).await?))
  }
  #[cfg(windows)]
  {
    use tokio::net::windows::named_pipe::ClientOptions;
    loop {
      match ClientOptions::new().open(endpoint) {
        Ok(stream) => return Ok(Box::new(stream)),
        Err(error) if error.raw_os_error() == Some(231) => {
          // All pipe instances are busy. The caller's connection timeout
          // bounds retries before a user-message write has been attempted.
          tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(error) => return Err(error),
      }
    }
  }
  #[cfg(not(any(unix, windows)))]
  {
    let _ = endpoint;
    Err(std::io::Error::new(
      std::io::ErrorKind::Unsupported,
      "Codex Desktop IPC is unavailable on this platform",
    ))
  }
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
