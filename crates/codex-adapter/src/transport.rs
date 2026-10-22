use crate::framing::{self, RpcReader, StderrTail};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use serde_json::{Value, json};
use std::{
  collections::HashMap,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  time::Duration,
};
use tokio::{
  io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
  process::Child,
  sync::{Mutex as AsyncMutex, mpsc, oneshot, watch},
  task::JoinHandle,
};
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream,
  tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokn_codex_rpc_protocol::{self as wire, RpcId};
use tokn_interaction_core::{ErrorKind, InteractionError};

const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PENDING: usize = 128;
const MAX_INCOMING: usize = 1024;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type CleanupResult = Option<Result<(), InteractionError>>;

pub(super) enum Incoming {
  Notification {
    method: String,
    params: Value,
    native: Value,
  },
  Request {
    id: RpcId,
    method: String,
    params: Value,
    native: Value,
  },
  Response {
    method: String,
    params: Value,
    context: Option<u64>,
    mutation: bool,
    result: Result<Value, InteractionError>,
    reply: oneshot::Sender<Result<Value, InteractionError>>,
  },
  Native(Value),
  Disconnected {
    reason: String,
  },
}

enum Writer {
  Stdio(Box<dyn AsyncWrite + Unpin + Send>),
  WebSocket(SplitSink<Socket, Message>),
}

struct Pending {
  method: String,
  params: Value,
  context: Option<u64>,
  mutation: bool,
  reply: oneshot::Sender<Result<Value, InteractionError>>,
}

pub(super) struct Transport {
  writer: Arc<AsyncMutex<Option<Writer>>>,
  pending: Mutex<HashMap<RpcId, Pending>>,
  incoming: mpsc::Sender<Incoming>,
  child: Mutex<Option<Child>>,
  tasks: Mutex<Vec<JoinHandle<()>>>,
  stderr: Mutex<StderrTail>,
  closed: AtomicBool,
  cleanup: watch::Sender<CleanupResult>,
  sequence: AtomicU64,
  request_timeout: Duration,
}

impl Transport {
  pub(super) fn stdio<R, W>(
    reader: R,
    writer: W,
    child: Option<Child>,
    request_timeout: Duration,
  ) -> (Arc<Self>, mpsc::Receiver<Incoming>)
  where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
  {
    let (transport, incoming) = Self::new(Writer::Stdio(Box::new(writer)), child, request_timeout);
    let weak = Arc::downgrade(&transport);
    let task = tokio::spawn(async move {
      let mut reader = RpcReader::new(reader);
      loop {
        let frame = reader.read_frame().await;
        let Some(transport) = weak.upgrade() else { return };
        match frame {
          Ok(Some(value)) => {
            if let Err(reason) = transport.receive(value) {
              transport.disconnected(reason);
              return;
            }
          }
          Ok(None) => {
            transport.disconnected("Codex closed its RPC output".into());
            return;
          }
          Err(reason) => {
            transport.disconnected(reason);
            return;
          }
        }
      }
    });
    transport.track(task);
    (transport, incoming)
  }

  pub(super) async fn websocket(
    endpoint: &str,
    bearer: Option<&str>,
    request_timeout: Duration,
  ) -> Result<(Arc<Self>, mpsc::Receiver<Incoming>), InteractionError> {
    let mut request = endpoint
      .into_client_request()
      .map_err(|_| InteractionError::invalid("Invalid Codex WebSocket endpoint"))?;
    let uri = request.uri();
    if !matches!(uri.scheme_str(), Some("ws" | "wss"))
      || uri.host().is_none()
      || uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
      || endpoint.contains('#')
      || request_timeout.is_zero()
    {
      return Err(InteractionError::invalid(
        "Codex endpoint must be an explicit WS(S) URL without credentials or fragment, with a positive timeout",
      ));
    }
    if let Some(bearer) = bearer {
      if bearer.is_empty() {
        return Err(InteractionError::invalid("Codex bearer token must not be empty"));
      }
      request.headers_mut().insert(
        "authorization",
        format!("Bearer {bearer}")
          .parse()
          .map_err(|_| InteractionError::invalid("Invalid Codex authorization header"))?,
      );
    }
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(framing::MAX_FRAME_BYTES);
    config.max_frame_size = Some(framing::MAX_FRAME_BYTES);
    let (socket, _) = tokio::time::timeout(
      request_timeout,
      tokio_tungstenite::connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(|_| InteractionError::unavailable("Codex WebSocket connection timed out"))?
    .map_err(|_| InteractionError::unavailable("Could not connect to Codex WebSocket endpoint"))?;
    let (writer, mut reader) = socket.split();
    let (transport, incoming) = Self::new(Writer::WebSocket(writer), None, request_timeout);
    let weak = Arc::downgrade(&transport);
    let task = tokio::spawn(async move {
      loop {
        let message = reader.next().await;
        let Some(transport) = weak.upgrade() else { return };
        let value = match message {
          Some(Ok(Message::Text(text))) => {
            serde_json::from_str(&text).map_err(|_| "Codex WebSocket returned invalid JSON".to_owned())
          }
          Some(Ok(Message::Binary(bytes))) => {
            serde_json::from_slice(&bytes).map_err(|_| "Codex WebSocket returned invalid JSON".to_owned())
          }
          Some(Ok(Message::Ping(_))) => {
            // Tungstenite queued the required Pong while polling the reader.
            // Flush through the same sink lock used by all JSON writes.
            if transport.flush().await.is_err() {
              return;
            }
            continue;
          }
          Some(Ok(Message::Pong(_) | Message::Frame(_))) => continue,
          Some(Ok(Message::Close(_))) | None => Err("Codex closed its WebSocket connection".into()),
          Some(Err(_)) => Err("Codex WebSocket connection failed".into()),
        };
        match value.and_then(|value| transport.receive(value)) {
          Ok(()) => {}
          Err(reason) => {
            transport.disconnected(reason);
            return;
          }
        }
      }
    });
    transport.track(task);
    Ok((transport, incoming))
  }

  fn new(writer: Writer, mut child: Option<Child>, request_timeout: Duration) -> (Arc<Self>, mpsc::Receiver<Incoming>) {
    let stderr = child.as_mut().and_then(|child| child.stderr.take());
    let (incoming, receiver) = mpsc::channel(MAX_INCOMING + 1);
    let (cleanup, _) = watch::channel(None);
    let transport = Arc::new(Self {
      writer: Arc::new(AsyncMutex::new(Some(writer))),
      pending: Mutex::new(HashMap::new()),
      incoming,
      child: Mutex::new(child),
      tasks: Mutex::new(Vec::new()),
      stderr: Mutex::new(StderrTail::default()),
      closed: AtomicBool::new(false),
      cleanup,
      sequence: AtomicU64::new(1),
      request_timeout,
    });
    if let Some(mut stderr) = stderr {
      let weak = Arc::downgrade(&transport);
      transport.track(tokio::spawn(async move {
        let mut bytes = [0; 4096];
        loop {
          match stderr.read(&mut bytes).await {
            Ok(0) | Err(_) => return,
            Ok(length) => {
              let Some(transport) = weak.upgrade() else { return };
              transport
                .stderr
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(&bytes[..length]);
            }
          }
        }
      }));
    }
    (transport, receiver)
  }

  fn track(&self, task: JoinHandle<()>) {
    let mut tasks = self.tasks.lock().unwrap_or_else(|error| error.into_inner());
    if self.is_closed() {
      task.abort();
    } else {
      tasks.push(task);
    }
  }

  pub(super) fn is_closed(&self) -> bool {
    self.closed.load(Ordering::Acquire)
  }

  pub(super) fn stderr_tail(&self) -> String {
    self.stderr.lock().unwrap_or_else(|error| error.into_inner()).text()
  }

  pub(super) async fn rpc(&self, method: &str, params: Value, mutation: bool) -> Result<Value, InteractionError> {
    self.rpc_with_context(method, params, mutation, None).await
  }

  pub(super) async fn rpc_with_context(
    &self,
    method: &str,
    params: Value,
    mutation: bool,
    context: Option<u64>,
  ) -> Result<Value, InteractionError> {
    let mut number = self.sequence.load(Ordering::Relaxed);
    loop {
      let next = number
        .checked_add(1)
        .ok_or_else(|| InteractionError::unavailable("Codex RPC request IDs exhausted"))?;
      match self
        .sequence
        .compare_exchange_weak(number, next, Ordering::Relaxed, Ordering::Relaxed)
      {
        Ok(_) => break,
        Err(current) => number = current,
      }
    }
    let id = RpcId::String(format!("tokn:{number}"));
    let (reply, receive) = oneshot::channel();
    {
      let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());
      if self.is_closed() {
        return Err(InteractionError::unavailable("Codex RPC connection is closed"));
      }
      if pending.len() >= MAX_PENDING {
        return Err(InteractionError::unavailable("Codex RPC pending request limit reached"));
      }
      pending.insert(
        id.clone(),
        Pending {
          method: method.to_owned(),
          params: params.clone(),
          context,
          mutation,
          reply,
        },
      );
    }
    // Cancelled callers leave no entries behind. Writes themselves carry a
    // separate guard so cancellation after a partial frame closes the stream.
    let _pending = PendingGuard {
      transport: self,
      id: id.clone(),
    };
    self
      .write_value(json!({"id":id,"method":method,"params":params}), mutation)
      .await?;
    match tokio::time::timeout(self.request_timeout, receive).await {
      Ok(Ok(result)) => result,
      _ => Err(transport_error(
        mutation,
        "Codex did not acknowledge the RPC request; it was not retried",
      )),
    }
  }

  pub(super) async fn notify(&self, method: &str, params: Value) -> Result<(), InteractionError> {
    self.write_value(json!({"method":method,"params":params}), false).await
  }

  pub(super) async fn respond(&self, id: &RpcId, result: Value) -> Result<(), InteractionError> {
    self.write_value(json!({"id":id,"result":result}), true).await
  }

  pub(super) async fn respond_error(&self, id: &RpcId, code: i64, message: &str) -> Result<(), InteractionError> {
    self
      .write_value(json!({"id":id,"error":{"code":code,"message":message}}), true)
      .await
  }

  async fn write_value(&self, value: Value, mutation: bool) -> Result<(), InteractionError> {
    let bytes = serde_json::to_vec(&value).map_err(|_| InteractionError::invalid("Invalid Codex RPC request"))?;
    if bytes.len() > framing::MAX_FRAME_BYTES {
      return Err(InteractionError::invalid("Codex RPC request exceeds the 34 MiB limit"));
    }
    let mut writer = tokio::time::timeout(WRITE_TIMEOUT, self.writer.lock())
      .await
      .map_err(|_| InteractionError::unavailable("Codex RPC writer is busy; request was not sent"))?;
    if self.is_closed() {
      return Err(InteractionError::unavailable("Codex RPC connection is closed"));
    }
    let Some(writer) = writer.as_mut() else {
      return Err(InteractionError::unavailable("Codex RPC input is closed"));
    };
    let mut guard = WriteGuard {
      transport: self,
      complete: false,
    };
    let result = tokio::time::timeout(WRITE_TIMEOUT, async {
      match writer {
        Writer::Stdio(writer) => {
          writer.write_all(&bytes).await?;
          writer.write_all(b"\n").await?;
          writer.flush().await
        }
        Writer::WebSocket(writer) => writer
          .send(Message::Text(String::from_utf8(bytes).expect("JSON is UTF-8").into()))
          .await
          .map_err(std::io::Error::other),
      }
    })
    .await;
    guard.complete = true;
    match result {
      Ok(Ok(())) => Ok(()),
      _ => {
        self.disconnected("Codex RPC write failed; the connection is no longer usable".into());
        Err(transport_error(
          mutation,
          "Codex RPC write failed; the request may have been accepted",
        ))
      }
    }
  }

  async fn flush(&self) -> Result<(), InteractionError> {
    let mut writer = tokio::time::timeout(WRITE_TIMEOUT, self.writer.lock())
      .await
      .map_err(|_| {
        self.disconnected("Codex WebSocket heartbeat could not obtain the writer".into());
        InteractionError::unavailable("Codex WebSocket writer timed out")
      })?;
    let Some(Writer::WebSocket(writer)) = writer.as_mut() else {
      return Ok(());
    };
    match tokio::time::timeout(WRITE_TIMEOUT, writer.flush()).await {
      Ok(Ok(())) => Ok(()),
      _ => {
        self.disconnected("Codex WebSocket heartbeat write failed".into());
        Err(InteractionError::unavailable("Codex WebSocket heartbeat write failed"))
      }
    }
  }

  fn receive(&self, native: Value) -> Result<(), String> {
    if self.is_closed() {
      return Ok(());
    }
    match wire::RpcMessage::decode(native) {
      wire::RpcMessage::Response { response, native } => {
        let pending = self
          .pending
          .lock()
          .unwrap_or_else(|error| error.into_inner())
          .remove(&response.id);
        if let Some(pending) = pending {
          self.deliver(Incoming::Response {
            method: pending.method,
            params: pending.params,
            context: pending.context,
            mutation: pending.mutation,
            result: Ok(response.result),
            reply: pending.reply,
          })
        } else {
          self.deliver(Incoming::Native(native))
        }
      }
      wire::RpcMessage::Error { error, native } => {
        let Some(id) = error.id else {
          return Err("Codex RPC error has no request ID".into());
        };
        let pending = self
          .pending
          .lock()
          .unwrap_or_else(|error| error.into_inner())
          .remove(&id);
        if let Some(pending) = pending {
          let message = error.error.message.chars().take(2048).collect::<String>();
          self.deliver(Incoming::Response {
            method: pending.method,
            params: pending.params,
            context: pending.context,
            mutation: pending.mutation,
            result: Err(InteractionError::new(
              ErrorKind::Rejected,
              format!("Codex RPC {}: {message}", error.error.code),
            )),
            reply: pending.reply,
          })
        } else {
          self.deliver(Incoming::Native(native))
        }
      }
      wire::RpcMessage::Request { request, native } => self.deliver(Incoming::Request {
        id: request.id,
        method: request.method,
        params: request.params.unwrap_or(Value::Null),
        native,
      }),
      wire::RpcMessage::Notification { notification, native } => self.deliver(Incoming::Notification {
        method: notification.method,
        params: notification.params.unwrap_or(Value::Null),
        native,
      }),
      wire::RpcMessage::Unknown(native) => {
        if native.get("id").is_some() || native.get("result").is_some() || native.get("error").is_some() {
          Err("Codex returned a malformed RPC response or server request".into())
        } else {
          self.deliver(Incoming::Native(native))
        }
      }
    }
  }

  fn deliver(&self, incoming: Incoming) -> Result<(), String> {
    // Keep one slot for the terminal notification. Overflow must be observable;
    // silently dropping questions or turn notifications would corrupt state.
    if self.incoming.capacity() <= 1 {
      return Err("Codex RPC event receiver fell behind its bounded buffer".into());
    }
    self
      .incoming
      .try_send(incoming)
      .map_err(|_| "Codex RPC event receiver is unavailable".into())
  }

  fn disconnected(&self, reason: String) {
    if self.closed.swap(true, Ordering::AcqRel) {
      return;
    }
    for (_, pending) in self.pending.lock().unwrap_or_else(|error| error.into_inner()).drain() {
      let _ = pending
        .reply
        .send(Err(transport_error(pending.mutation, reason.clone())));
    }
    let _ = self.incoming.try_send(Incoming::Disconnected { reason });
    self.start_cleanup();
  }

  fn start_cleanup(&self) {
    let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|error| error.into_inner()));
    for task in tasks {
      task.abort();
    }
    let child = self.child.lock().unwrap_or_else(|error| error.into_inner()).take();
    let writer = self.writer.clone();
    let cleanup = self.cleanup.clone();
    // A detached task owns the process until wait finishes. Cancelling close()
    // cannot drop an unreaped child or leave the native input handle open.
    tokio::spawn(async move {
      if let Some(writer) = writer.lock().await.take() {
        match writer {
          Writer::Stdio(mut writer) => {
            let _ = tokio::time::timeout(WRITE_TIMEOUT, writer.shutdown()).await;
          }
          Writer::WebSocket(mut writer) => {
            let _ = tokio::time::timeout(WRITE_TIMEOUT, writer.close()).await;
          }
        }
      }
      let result = if let Some(mut child) = child {
        if matches!(
          tokio::time::timeout(Duration::from_secs(2), child.wait()).await,
          Ok(Ok(_))
        ) {
          Ok(())
        } else {
          let _ = child.start_kill();
          tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .map_err(|_| InteractionError::unavailable("Codex child cleanup timed out"))
            .and_then(|result| {
              result
                .map(|_| ())
                .map_err(|_| InteractionError::unavailable("Codex child cleanup failed"))
            })
        }
      } else {
        Ok(())
      };
      cleanup.send_replace(Some(result));
    });
  }

  pub(super) async fn close(&self) -> Result<(), InteractionError> {
    let mut cleanup = self.cleanup.subscribe();
    self.disconnected("Codex RPC adapter closed".into());
    loop {
      if let Some(result) = cleanup.borrow().clone() {
        return result;
      }
      cleanup
        .changed()
        .await
        .map_err(|_| InteractionError::unavailable("Codex cleanup task was interrupted"))?;
    }
  }
}

impl Drop for Transport {
  fn drop(&mut self) {
    for task in self
      .tasks
      .get_mut()
      .unwrap_or_else(|error| error.into_inner())
      .drain(..)
    {
      task.abort();
    }
    // Owned launch config also sets kill_on_drop. Attached endpoints store no child.
    if let Some(mut child) = self.child.get_mut().unwrap_or_else(|error| error.into_inner()).take() {
      let _ = child.start_kill();
      if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
          let _ = child.wait().await;
        });
      }
    }
  }
}

struct PendingGuard<'a> {
  transport: &'a Transport,
  id: RpcId,
}

impl Drop for PendingGuard<'_> {
  fn drop(&mut self) {
    self
      .transport
      .pending
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .remove(&self.id);
  }
}

struct WriteGuard<'a> {
  transport: &'a Transport,
  complete: bool,
}

impl Drop for WriteGuard<'_> {
  fn drop(&mut self) {
    if !self.complete {
      self
        .transport
        .disconnected("Codex RPC write was cancelled; the connection is no longer usable".into());
    }
  }
}

fn transport_error(mutation: bool, message: impl Into<String>) -> InteractionError {
  if mutation {
    InteractionError::uncertain(message)
  } else {
    InteractionError::unavailable(message)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

  fn connection() -> (
    Arc<Transport>,
    mpsc::Receiver<Incoming>,
    ReadHalf<DuplexStream>,
    WriteHalf<DuplexStream>,
  ) {
    let (adapter, peer) = tokio::io::duplex(4096);
    let (reader, writer) = tokio::io::split(adapter);
    let (peer_reader, peer_writer) = tokio::io::split(peer);
    let (transport, incoming) = Transport::stdio(reader, writer, None, Duration::from_secs(2));
    (transport, incoming, peer_reader, peer_writer)
  }

  async fn send(writer: &mut (impl AsyncWrite + Unpin), value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.unwrap();
  }

  async fn complete_response(incoming: &mut mpsc::Receiver<Incoming>) {
    let Some(Incoming::Response { result, reply, .. }) = incoming.recv().await else {
      panic!("expected ordered response")
    };
    let _ = reply.send(result);
  }

  #[tokio::test]
  async fn correlates_replies_and_preserves_server_request_id_types() {
    let (transport, mut incoming, reader, mut writer) = connection();
    let mut reader = RpcReader::new(reader);
    let caller = transport.clone();
    let request = tokio::spawn(async move { caller.rpc("model/list", json!({}), false).await });
    let native = reader.read_frame().await.unwrap().unwrap();
    send(
      &mut writer,
      json!({"method":"thread/started","params":{"threadId":"one"}}),
    )
    .await;
    send(
      &mut writer,
      json!({"id":42,"method":"item/tool/requestUserInput","params":{"questions":[]}}),
    )
    .await;
    send(&mut writer, json!({"id":native["id"],"result":{"data":[]}})).await;
    assert!(matches!(incoming.recv().await, Some(Incoming::Notification { method, .. }) if method == "thread/started"));
    let Some(Incoming::Request { id, .. }) = incoming.recv().await else {
      panic!("missing server request")
    };
    assert_eq!(id, RpcId::Number(42));
    complete_response(&mut incoming).await;
    assert_eq!(request.await.unwrap().unwrap(), json!({"data":[]}));
    transport.respond(&id, json!({"answers":{}})).await.unwrap();
    assert_eq!(
      reader.read_frame().await.unwrap().unwrap(),
      json!({"id":42,"result":{"answers":{}}})
    );
    transport.close().await.unwrap();
  }

  #[tokio::test]
  async fn cancelled_reply_wait_leaves_connection_usable_and_preserves_late_response() {
    let (transport, mut incoming, reader, mut writer) = connection();
    let mut reader = RpcReader::new(reader);
    let caller = transport.clone();
    let request = tokio::spawn(async move { caller.rpc("thread/read", json!({}), false).await });
    let first = reader.read_frame().await.unwrap().unwrap();
    request.abort();
    let _ = request.await;
    assert!(transport.pending.lock().unwrap().is_empty());
    let late = json!({"id":first["id"],"result":{"late":true}});
    send(&mut writer, late.clone()).await;
    assert!(matches!(incoming.recv().await, Some(Incoming::Native(value)) if value == late));
    let caller = transport.clone();
    let request = tokio::spawn(async move { caller.rpc("thread/read", json!({}), false).await });
    let second = reader.read_frame().await.unwrap().unwrap();
    assert_ne!(first["id"], second["id"]);
    send(&mut writer, json!({"id":second["id"],"result":null})).await;
    complete_response(&mut incoming).await;
    assert!(request.await.unwrap().unwrap().is_null());
    assert!(!transport.is_closed());
    transport.close().await.unwrap();
  }

  #[tokio::test]
  async fn partial_write_cancellation_is_terminal() {
    let (adapter, mut peer) = tokio::io::duplex(1);
    let (reader, writer) = tokio::io::split(adapter);
    let (transport, mut incoming) = Transport::stdio(reader, writer, None, Duration::from_secs(2));
    let caller = transport.clone();
    let request = tokio::spawn(async move {
      caller
        .rpc("turn/start", json!({"text":"long enough to block"}), true)
        .await
    });
    let mut byte = [0];
    peer.read_exact(&mut byte).await.unwrap();
    request.abort();
    let _ = request.await;
    assert!(transport.is_closed());
    assert!(matches!(incoming.recv().await, Some(Incoming::Disconnected { reason }) if reason.contains("cancelled")));
    assert_eq!(
      transport.rpc("thread/read", json!({}), false).await.unwrap_err().kind,
      ErrorKind::Unavailable
    );
    transport.close().await.unwrap();
  }

  #[tokio::test]
  async fn disconnect_distinguishes_uncertain_mutation_from_unavailable_metadata() {
    for mutation in [false, true] {
      let (transport, _incoming, reader, mut writer) = connection();
      let mut reader = RpcReader::new(reader);
      let caller = transport.clone();
      let request = tokio::spawn(async move { caller.rpc("turn/start", json!({}), mutation).await });
      reader.read_frame().await.unwrap().unwrap();
      writer.shutdown().await.unwrap();
      let error = request.await.unwrap().unwrap_err();
      assert_eq!(
        error.kind,
        if mutation {
          ErrorKind::Uncertain
        } else {
          ErrorKind::Unavailable
        }
      );
      transport.close().await.unwrap();
    }
  }

  #[tokio::test]
  async fn explicit_native_error_is_rejection_without_disconnecting() {
    let (transport, mut incoming, reader, mut writer) = connection();
    let mut reader = RpcReader::new(reader);
    let caller = transport.clone();
    let request = tokio::spawn(async move { caller.rpc("turn/steer", json!({}), true).await });
    let native = reader.read_frame().await.unwrap().unwrap();
    send(
      &mut writer,
      json!({"id":native["id"],"error":{"code":-32000,"message":"turn ID mismatch"}}),
    )
    .await;
    complete_response(&mut incoming).await;
    let error = request.await.unwrap().unwrap_err();
    assert_eq!(error.kind, ErrorKind::Rejected);
    assert!(error.message.contains("-32000"));
    assert!(!transport.is_closed());
    transport.close().await.unwrap();
  }

  #[tokio::test]
  async fn event_overflow_reserves_a_terminal_notification() {
    let (transport, mut incoming, _reader, _writer) = connection();
    for _ in 0..MAX_INCOMING {
      transport.receive(json!({"method":"future/event"})).unwrap();
    }
    let reason = transport.receive(json!({"method":"future/event"})).unwrap_err();
    transport.disconnected(reason);
    for _ in 0..MAX_INCOMING {
      assert!(matches!(incoming.recv().await, Some(Incoming::Notification { .. })));
    }
    assert!(matches!(incoming.recv().await, Some(Incoming::Disconnected { reason }) if reason.contains("fell behind")));
    transport.close().await.unwrap();
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn cancelling_owned_close_preserves_shared_cleanup_and_reaps_child() {
    use std::process::Stdio;

    let mut child = tokio::process::Command::new("/bin/sh")
      .args(["-c", "exec /bin/sleep 60"])
      .env_clear()
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .spawn()
      .unwrap();
    let pid = child.id().unwrap();
    let reader = child.stdout.take().unwrap();
    let writer = child.stdin.take().unwrap();
    let (transport, mut incoming) = Transport::stdio(reader, writer, Some(child), Duration::from_secs(2));
    let process_exists = || async {
      tokio::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap()
        .success()
    };
    assert!(process_exists().await);
    let first_transport = transport.clone();
    let first_close = tokio::spawn(async move { first_transport.close().await });
    assert!(matches!(incoming.recv().await, Some(Incoming::Disconnected { .. })));
    // sleep ignores stdin closure, keeping the detached cleanup task in its
    // grace period after close's caller has been cancelled.
    first_close.abort();
    assert!(first_close.await.unwrap_err().is_cancelled());
    assert!(process_exists().await);
    tokio::time::timeout(Duration::from_secs(10), transport.close())
      .await
      .expect("shared owned-process cleanup did not finish")
      .unwrap();
    assert!(!process_exists().await, "owned process was not killed and reaped");
    tokio::time::timeout(Duration::from_millis(200), transport.close())
      .await
      .expect("completed cleanup was not shared with later callers")
      .unwrap();
  }

  #[tokio::test]
  async fn websocket_attachment_supports_bidirectional_rpc() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
      let (stream, _) = listener.accept().await.unwrap();
      let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
      let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
        panic!("expected request")
      };
      let request: Value = serde_json::from_str(&text).unwrap();
      socket
        .send(Message::Text(
          json!({"id":request["id"],"result":{"ok":true}}).to_string().into(),
        ))
        .await
        .unwrap();
      socket
        .send(Message::Text(
          json!({"id":"question","method":"question","params":{}})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
      let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
        panic!("expected answer")
      };
      assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({"id":"question","result":{"answer":"yes"}})
      );
      while let Some(message) = socket.next().await {
        if matches!(message, Ok(Message::Close(_))) {
          break;
        }
      }
    });
    let (transport, mut incoming) = Transport::websocket(&format!("ws://{address}"), None, Duration::from_secs(2))
      .await
      .unwrap();
    let caller = transport.clone();
    let initialize = tokio::spawn(async move { caller.rpc("initialize", json!({}), false).await });
    complete_response(&mut incoming).await;
    assert_eq!(initialize.await.unwrap().unwrap(), json!({"ok":true}));
    let Some(Incoming::Request { id, .. }) = incoming.recv().await else {
      panic!("missing question")
    };
    transport.respond(&id, json!({"answer":"yes"})).await.unwrap();
    transport.close().await.unwrap();
    server.await.unwrap();
  }
}
