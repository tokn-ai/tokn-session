//! Native transport for a host that owns the session workflow state machine.
use super::*;
use tokio::sync::mpsc;

const FRAME_CAPACITY: usize = 8;
const MAX_TURN_ERROR_BYTES: usize = 2048;

/// An exclusive, ordered native session transport. The adapter owns IO and
/// process cleanup; the host owns admission receipts, workflow deadlines,
/// single-dialog policy, and its persisted lifecycle. This wrapper deliberately
/// does not expose the common adapter API, preventing competing request owners.
pub struct SupervisedSession {
  adapter: PiAdapter,
  frames: mpsc::Receiver<Value>,
}

impl PiAdapter {
  /// Install the bounded native observer before starting the process reader.
  pub async fn spawn_supervised(
    options: EndpointOptions,
    launch: LaunchOptions,
  ) -> Result<SupervisedSession, InteractionError> {
    let (send, frames) = mpsc::channel(FRAME_CAPACITY);
    let adapter = Self::spawn_with_supervision(options, launch, Some(send)).await?;
    Ok(SupervisedSession { adapter, frames })
  }

  /// Attach only handles explicitly supplied by their owner. Closing this
  /// session detaches its transport without terminating an external process.
  pub fn attach_supervised<R, W>(
    options: EndpointOptions,
    reader: R,
    writer: W,
  ) -> Result<SupervisedSession, InteractionError>
  where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
  {
    let (send, frames) = mpsc::channel(FRAME_CAPACITY);
    let adapter = Self::connect_with_supervision(options, Ownership::Attached, reader, writer, None, Some(send))?;
    Ok(SupervisedSession { adapter, frames })
  }
}

impl SupervisedSession {
  pub fn pid(&self) -> Option<u32> {
    self.adapter.inner.pid
  }

  pub fn stderr_tail(&self) -> String {
    self.adapter.stderr_tail()
  }

  /// True only when the terminal event confirmed the owned process exited
  /// successfully. Transport or protocol failures never become clean exits
  /// merely because subsequent cleanup stops the child successfully.
  pub fn exited_successfully(&self) -> bool {
    self.adapter.inner.clean_exit.load(std::sync::atomic::Ordering::Acquire)
  }

  /// Reserve correlation before writing. Completion is delivered by `recv` in
  /// native order; a successful send only confirms the complete pipe write.
  /// The host must retain its workflow receipt and never replay uncertain IO.
  pub async fn send(&self, id: String, command: wire::Command) -> Result<(), InteractionError> {
    let adapter = self.adapter.clone();
    tokio::spawn(async move { adapter.inner.supervised_send(id, command).await })
      .await
      .map_err(|_| InteractionError::uncertain("Pi RPC write admission was interrupted"))?
  }

  /// Write a host-validated dialog response without issuing a state query that
  /// could itself be blocked by that same native extension dialog.
  pub async fn respond(&self, response: wire::ExtensionUiResponse) -> Result<(), InteractionError> {
    let value = serde_json::to_value(response).map_err(|error| InteractionError::invalid(error.to_string()))?;
    let adapter = self.adapter.clone();
    tokio::spawn(async move { adapter.inner.write_value(value, true).await })
      .await
      .map_err(|_| InteractionError::uncertain("Pi dialog write admission was interrupted"))?
  }

  /// Deliver EOF while leaving stdout/stderr available for orderly teardown.
  pub async fn close_input(&self) {
    self.adapter.inner.close_input().await;
  }

  /// Cleanup continues if the caller disappears. Await this before releasing
  /// host ownership: attached sessions detach; managed sessions stop and reap.
  pub async fn close(&self) -> Result<(), InteractionError> {
    let adapter = self.adapter.clone();
    tokio::spawn(async move { adapter.inner.close().await })
      .await
      .map_err(|_| InteractionError::unavailable("Pi process cleanup was interrupted"))?
  }

  /// Drain previously received metadata before reporting the terminal failure.
  /// Token and transcript bodies are filtered before entering this queue.
  pub async fn recv(&mut self) -> Option<Result<Value, String>> {
    match self.frames.recv().await {
      Some(frame) => Some(Ok(frame)),
      None => self
        .adapter
        .inner
        .terminal
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .map(Err),
    }
  }

  pub fn is_empty(&self) -> bool {
    self.frames.is_empty()
      && self
        .adapter
        .inner
        .terminal
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_none()
  }
}

/// Keep coarse lifecycle metadata, never token events or complete message/tool
/// bodies. Their source of truth remains the provider's persisted history.
pub(super) fn project_frame(value: Value) -> Option<Value> {
  let kind = value.get("type").and_then(Value::as_str)?;
  match kind {
    "extension_ui_request" => Some(value),
    "message_end" => {
      let message = value.get("message")?;
      if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
      }
      Some(json!({
        "type":"message_end",
        "stop_reason":message.get("stopReason").and_then(Value::as_str)
          .map(|reason| reason.chars().take(128).collect::<String>()),
        "error_message":turn_error(message.get("errorMessage"))
      }))
    }
    "auto_retry_end" => Some(json!({
      "type":"auto_retry_end",
      "success":value.get("success").and_then(Value::as_bool),
      "final_error":turn_error(value.get("finalError"))
    })),
    "compaction_end" | "auto_compaction_end" => Some(json!({
      "type":kind,
      "aborted":value.get("aborted").and_then(Value::as_bool),
      "error_message":turn_error(value.get("errorMessage"))
    })),
    "agent_start"
    | "agent_end"
    | "agent_settled"
    | "compaction_start"
    | "auto_compaction_start"
    | "auto_retry_start" => Some(json!({"type":kind})),
    _ => None,
  }
}

fn turn_error(value: Option<&Value>) -> Option<String> {
  let text = value.and_then(Value::as_str)?.trim();
  if text.is_empty() {
    return None;
  }
  let mut end = text.len().min(MAX_TURN_ERROR_BYTES);
  while !text.is_char_boundary(end) {
    end -= 1;
  }
  Some(text[..end].to_owned())
}
