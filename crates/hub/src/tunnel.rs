//! Authenticated host enrollment and bounded HTTP multiplexing.
use crate::{
  protocol::{self, Frame},
  store::{HostRecord, Store},
};
use axum::{
  body::{Body, Bytes},
  extract::ws::{Message, WebSocket},
  http::{Method, StatusCode, header},
  response::Response,
};
use ed25519_dalek::{Signature, VerifyingKey};
use rand::{RngCore, rngs::OsRng};
use serde::Serialize;
use std::{
  collections::HashMap,
  io,
  sync::{Arc, Mutex, Weak},
  time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const PAIR_LIFETIME: Duration = Duration::from_secs(300);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct HubTunnels {
  store: Store,
  inner: Arc<Mutex<HubInner>>,
  handshakes: Arc<Semaphore>,
  enrollments: Arc<Semaphore>,
  approved: Arc<Semaphore>,
  secure_capacity: Arc<Semaphore>,
  shutdown: CancellationToken,
}

#[derive(Default)]
struct HubInner {
  pending: HashMap<String, Pending>,
  online: HashMap<String, Arc<Connection>>,
}

struct Pending {
  record: HostRecord,
  expires: Instant,
  approval: oneshot::Sender<HostRecord>,
}

#[derive(Clone, Serialize)]
pub struct PendingEnrollment {
  pub pairing_code: String,
  pub host_id: String,
  pub name: String,
  pub access: String,
  pub expires_in: u64,
}

struct Connection {
  // Only socket handlers own strong leases. Keeping an old HTTP response body
  // alive cannot retain host capacity after that socket disconnects.
  capacity: Weak<OwnedSemaphorePermit>,
  outgoing: mpsc::Sender<Frame>,
  requests: Mutex<HashMap<u64, InFlight>>,
  secure_channels: Mutex<HashMap<u64, mpsc::Sender<Bytes>>>,
  next_id: std::sync::atomic::AtomicU64,
  allow_control: bool,
  secure_only: std::sync::atomic::AtomicBool,
  shutdown: CancellationToken,
}

struct InFlight {
  headers: Option<oneshot::Sender<Result<(u16, Option<String>), String>>>,
  chunks: mpsc::Sender<Bytes>,
  failure: Arc<Mutex<Option<String>>>,
}

pub struct ProxyError {
  pub status: StatusCode,
  pub message: String,
}

impl ProxyError {
  fn new(status: StatusCode, message: impl Into<String>) -> Self {
    Self {
      status,
      message: message.into(),
    }
  }
}

struct RequestGuard {
  connection: Arc<Connection>,
  request_id: u64,
}

struct SecureGuard {
  connection: Arc<Connection>,
  channel_id: u64,
}

impl Drop for SecureGuard {
  fn drop(&mut self) {
    if self
      .connection
      .secure_channels
      .lock()
      .unwrap()
      .remove(&self.channel_id)
      .is_some()
      && self
        .connection
        .outgoing
        .try_send(Frame::SecureClose {
          channel_id: self.channel_id,
        })
        .is_err()
    {
      self.connection.shutdown.cancel();
    }
  }
}

impl Drop for RequestGuard {
  fn drop(&mut self) {
    if self
      .connection
      .requests
      .lock()
      .unwrap()
      .remove(&self.request_id)
      .is_some()
    {
      // A saturated control queue makes cancellation uncertain. Closing this
      // connection reliably cancels every task on the host, without replay.
      if self
        .connection
        .outgoing
        .try_send(Frame::Cancel {
          request_id: self.request_id,
        })
        .is_err()
      {
        self.connection.shutdown.cancel();
      }
    }
  }
}

impl HubTunnels {
  pub fn new(store: Store) -> Self {
    Self::with_limits(store, 32, 16, 64)
  }

  fn with_limits(store: Store, handshakes: usize, enrollments: usize, approved: usize) -> Self {
    Self {
      store,
      inner: Arc::new(Mutex::new(HubInner::default())),
      handshakes: Arc::new(Semaphore::new(handshakes)),
      enrollments: Arc::new(Semaphore::new(enrollments)),
      approved: Arc::new(Semaphore::new(approved)),
      secure_capacity: Arc::new(Semaphore::new(128)),
      shutdown: CancellationToken::new(),
    }
  }

  pub fn pending(&self) -> Vec<PendingEnrollment> {
    let mut inner = self.inner.lock().unwrap();
    inner
      .pending
      .retain(|_, pending| pending.expires > Instant::now() && !pending.approval.is_closed());
    let mut result: Vec<_> = inner
      .pending
      .iter()
      .map(|(code, pending)| PendingEnrollment {
        pairing_code: code.clone(),
        host_id: pending.record.host_id.clone(),
        name: pending.record.name.clone(),
        access: pending.record.access.clone(),
        expires_in: pending.expires.saturating_duration_since(Instant::now()).as_secs(),
      })
      .collect();
    result.sort_by(|a, b| a.host_id.cmp(&b.host_id));
    result
  }

  pub fn approve(&self, code: &str) -> Result<HostRecord, String> {
    let mut inner = self.inner.lock().unwrap();
    let pending = inner.pending.remove(code).ok_or("Unknown or expired pairing code")?;
    if pending.expires <= Instant::now() || pending.approval.is_closed() {
      return Err("Pairing request expired or host disconnected".into());
    }
    self.store.approve_host(pending.record.clone())?;
    // A host that disconnects immediately after approval stays approved and can
    // reconnect using the same identity, without repeating enrollment.
    let _ = pending.approval.send(pending.record.clone());
    Ok(pending.record)
  }

  pub fn revoke(&self, host_id: &str) -> Result<(), String> {
    let mut inner = self.inner.lock().unwrap();
    self.store.remove_host(host_id)?;
    if let Some(connection) = inner.online.remove(host_id) {
      connection.shutdown.cancel();
    }
    inner.pending.retain(|_, pending| pending.record.host_id != host_id);
    Ok(())
  }

  pub fn online(&self, host_id: &str) -> bool {
    self
      .inner
      .lock()
      .unwrap()
      .online
      .get(host_id)
      .is_some_and(|connection| !connection.shutdown.is_cancelled())
  }

  pub fn access(&self, host_id: &str) -> Option<String> {
    self
      .inner
      .lock()
      .unwrap()
      .online
      .get(host_id)
      .map(|connection| if connection.allow_control { "control" } else { "view" }.into())
  }

  pub fn secure_only(&self, host_id: &str) -> bool {
    self
      .inner
      .lock()
      .unwrap()
      .online
      .get(host_id)
      .is_some_and(|connection| connection.secure_only.load(std::sync::atomic::Ordering::Relaxed))
  }

  pub fn reserve_secure_channel(&self) -> Result<OwnedSemaphorePermit, String> {
    self
      .secure_capacity
      .clone()
      .try_acquire_owned()
      .map_err(|_| "Hub secure channel capacity reached".into())
  }

  pub fn shutdown(&self) {
    self.shutdown.cancel();
    let mut inner = self.inner.lock().unwrap();
    inner.pending.clear();
    for connection in inner.online.values() {
      connection.shutdown.cancel();
    }
  }

  pub async fn handle_socket(&self, mut socket: WebSocket) {
    let Ok(handshake_permit) = self.handshakes.clone().try_acquire_owned() else {
      return;
    };
    let result = tokio::time::timeout(IO_TIMEOUT, self.authenticate(&mut socket)).await;
    let Ok(Ok((record, allow_control, encrypted_registration, discovery_group))) = result else {
      return;
    };
    let registered = if encrypted_registration {
      drop(handshake_permit);
      self
        .store
        .register_encrypted_host_with_discovery(record, discovery_group.as_ref())
    } else {
      self.await_approval(&mut socket, record, handshake_permit).await
    };
    let record = match registered {
      Ok(record) => record,
      Err(error) if encrypted_registration => {
        let _ = send(
          &mut socket,
          &Frame::RegistrationRejected {
            message: registration_diagnostic(&error).into(),
          },
        )
        .await;
        return;
      }
      Err(_) => return,
    };
    let (outgoing, mut outgoing_rx) = mpsc::channel(64);
    let (connection, _approved_permit) = {
      // Recheck under the same lock as revoke, including the latest grant. A
      // revoked/reapproved identity cannot retain its earlier, broader access.
      let mut inner = self.inner.lock().unwrap();
      let Some(stored) = self.store.host(&record.host_id).ok().flatten() else {
        return;
      };
      if stored.public_key != record.public_key {
        return;
      }
      // Replacements inherit their identity's existing slot, even when every
      // approved slot is occupied. Other identities must acquire their own slot.
      let permit = inner
        .online
        .get(&record.host_id)
        .and_then(|previous| previous.capacity.upgrade())
        .or_else(|| self.approved.clone().try_acquire_owned().ok().map(Arc::new));
      let Some(permit) = permit else { return };
      let connection = Arc::new(Connection {
        capacity: Arc::downgrade(&permit),
        outgoing,
        requests: Mutex::new(HashMap::new()),
        secure_channels: Mutex::new(HashMap::new()),
        next_id: std::sync::atomic::AtomicU64::new(1),
        allow_control: allow_control && stored.access == "control",
        secure_only: std::sync::atomic::AtomicBool::new(encrypted_registration),
        shutdown: self.shutdown.child_token(),
      });
      if let Some(previous) = inner.online.insert(record.host_id.clone(), connection.clone()) {
        previous.shutdown.cancel();
      }
      (connection, permit)
    };
    let _ = async {
      send(
        &mut socket,
        &Frame::Ready {
          host_id: record.host_id.clone(),
          allow_control: connection.allow_control,
        },
      )
      .await?;
      let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
      let mut last_received = Instant::now();
      loop {
        tokio::select! {
          biased;
          _ = connection.shutdown.cancelled() => break,
          incoming = socket.recv() => {
            let Some(Ok(incoming)) = incoming else { break };
            last_received = Instant::now();
            match incoming {
              Message::Text(text) if text.len() <= protocol::MAX_FRAME => {
                let frame: Frame = serde_json::from_str(&text).map_err(|_| "Invalid tunnel message")?;
                connection.receive(frame)?;
              }
              Message::Ping(data) => { send_message(&mut socket, Message::Pong(data)).await?; }
              Message::Pong(_) => {},
              _ => break,
            }
          }
          frame = outgoing_rx.recv() => {
            let Some(frame) = frame else { break };
            send(&mut socket, &frame).await?;
          }
          _ = heartbeat.tick() => {
            if last_received.elapsed() > Duration::from_secs(45) { break; }
            send_message(&mut socket, Message::Ping(Bytes::new())).await?;
          }
        }
      }
      Ok::<_, String>(())
    }
    .await;
    connection.shutdown.cancel();
    connection
      .fail_all("Host disconnected; delivery of submitted input may be uncertain. Requests are never replayed.");
    let mut inner = self.inner.lock().unwrap();
    if inner
      .online
      .get(&record.host_id)
      .is_some_and(|current| Arc::ptr_eq(current, &connection))
    {
      inner.online.remove(&record.host_id);
    }
  }

  async fn authenticate(
    &self,
    socket: &mut WebSocket,
  ) -> Result<
    (
      HostRecord,
      bool,
      bool,
      Option<tokn_hub_client_core::discovery::DiscoveryGroup>,
    ),
    String,
  > {
    let mut random = [0u8; 32];
    OsRng.fill_bytes(&mut random);
    let nonce = protocol::encode(&random);
    send(
      socket,
      &Frame::Challenge {
        version: protocol::VERSION,
        nonce: nonce.clone(),
      },
    )
    .await?;
    let Some(Ok(Message::Text(text))) = socket.recv().await else {
      return Err("Missing host authentication".into());
    };
    if text.len() > 4096 {
      return Err("Host authentication is too large".into());
    }
    let (version, registered_id, public_key, name, allow_control, signature, discovery_group) =
      match serde_json::from_str(&text).map_err(|_| "Invalid host authentication")? {
        Frame::Authenticate {
          version,
          public_key,
          name,
          allow_control,
          signature,
        } => (version, None, public_key, name, allow_control, signature, None),
        Frame::Register {
          version,
          host_id,
          public_key,
          name,
          allow_control,
          signature,
          discovery_group,
        } if protocol::valid_host_uuid(&host_id) => (
          version,
          Some(host_id),
          public_key,
          name,
          allow_control,
          signature,
          discovery_group,
        ),
        _ => return Err("Expected host authentication or encrypted registration".into()),
      };
    if version != protocol::VERSION || name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control)
    {
      return Err("Invalid host version or name".into());
    }
    if let Some(group) = &discovery_group {
      group.validate()?;
    }
    let key_bytes: [u8; 32] = protocol::decode(&public_key, 32)?
      .try_into()
      .map_err(|_| "Invalid host key")?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| "Invalid host key")?;
    let signature = Signature::from_slice(&protocol::decode(&signature, 64)?).map_err(|_| "Invalid host proof")?;
    let proof = if let Some(host_id) = &registered_id {
      protocol::discovery_registration_proof(
        &nonce,
        host_id,
        &public_key,
        &name,
        allow_control,
        discovery_group.as_ref(),
      )
    } else {
      protocol::proof(&nonce, &public_key, &name, allow_control)
    };
    key
      .verify_strict(&proof, &signature)
      .map_err(|_| "Invalid host proof")?;
    let encrypted_registration = registered_id.is_some();
    Ok((
      HostRecord {
        host_id: registered_id.unwrap_or_else(|| protocol::host_id(&key_bytes)),
        public_key,
        name,
        access: if allow_control { "control" } else { "view" }.into(),
      },
      allow_control,
      encrypted_registration,
      discovery_group,
    ))
  }

  async fn await_approval(
    &self,
    socket: &mut WebSocket,
    record: HostRecord,
    handshake_permit: OwnedSemaphorePermit,
  ) -> Result<HostRecord, String> {
    let (approval, mut approved) = oneshot::channel();
    let (code, _enrollment_permit) = {
      let mut inner = self.inner.lock().unwrap();
      if let Some(stored) = self.store.host(&record.host_id)? {
        if stored.public_key != record.public_key {
          return Err("Host key changed".into());
        }
        return Ok(stored);
      }
      let enrollment_permit = self
        .enrollments
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Pending enrollment capacity reached")?;
      inner.pending.retain(|_, pending| {
        pending.record.host_id != record.host_id && pending.expires > Instant::now() && !pending.approval.is_closed()
      });
      let mut random = [0u8; 9];
      OsRng.fill_bytes(&mut random);
      let code = protocol::encode(&random);
      inner.pending.insert(
        code.clone(),
        Pending {
          record,
          expires: Instant::now() + PAIR_LIFETIME,
          approval,
        },
      );
      (code, enrollment_permit)
    };
    // Pending enrollment is a separate, smaller budget. An unknown proved key
    // waiting five minutes for approval must not block approved reconnections.
    drop(handshake_permit);
    let result = async {
      send(
        socket,
        &Frame::Pending {
          code: code.clone(),
          expires_in: PAIR_LIFETIME.as_secs(),
        },
      )
      .await?;
      let deadline = tokio::time::sleep(PAIR_LIFETIME);
      tokio::pin!(deadline);
      let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
      loop {
        tokio::select! {
          _ = self.shutdown.cancelled() => return Err("Hub stopped".into()),
          _ = &mut deadline => return Err("Pairing expired".into()),
          approved = &mut approved => return approved.map_err(|_| "Pairing cancelled".into()),
          incoming = socket.recv() => match incoming {
            Some(Ok(Message::Ping(data))) => send_message(socket, Message::Pong(data)).await?,
            Some(Ok(Message::Pong(_))) => {},
            _ => return Err("Host disconnected during pairing".into()),
          },
          _ = heartbeat.tick() => send_message(socket, Message::Ping(Bytes::new())).await?,
        }
      }
    }
    .await;
    self.inner.lock().unwrap().pending.remove(&code);
    result
  }

  pub async fn proxy(&self, host_id: &str, method: Method, path: &str, body: Bytes) -> Result<Response, ProxyError> {
    let connection = self
      .inner
      .lock()
      .unwrap()
      .online
      .get(host_id)
      .cloned()
      .filter(|connection| !connection.shutdown.is_cancelled())
      .ok_or_else(|| ProxyError::new(StatusCode::BAD_GATEWAY, "Host is offline"))?;
    if connection.secure_only.load(std::sync::atomic::Ordering::Relaxed) {
      return Err(ProxyError::new(
        StatusCode::FORBIDDEN,
        "This host requires the installed encrypted client",
      ));
    }
    if !protocol::allowed_route(method.as_str(), path, connection.allow_control) {
      return Err(ProxyError::new(
        StatusCode::FORBIDDEN,
        "Route is unavailable or host control is disabled",
      ));
    }
    if body.len() > protocol::MAX_BODY {
      return Err(ProxyError::new(StatusCode::PAYLOAD_TOO_LARGE, "Request is too large"));
    }
    let request_id = connection.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (headers, ready) = oneshot::channel();
    let (chunks, mut received) = mpsc::channel(protocol::RESPONSE_WINDOW);
    let failure = Arc::new(Mutex::new(None));
    {
      let mut requests = connection.requests.lock().unwrap();
      if requests.len() >= protocol::MAX_REQUESTS {
        return Err(ProxyError::new(
          StatusCode::TOO_MANY_REQUESTS,
          "Host request capacity reached",
        ));
      }
      requests.insert(
        request_id,
        InFlight {
          headers: Some(headers),
          chunks,
          failure: failure.clone(),
        },
      );
    }
    let guard = RequestGuard {
      connection: connection.clone(),
      request_id,
    };
    connection
      .outgoing
      .try_send(Frame::Request {
        request_id,
        method: method.to_string(),
        path: path.into(),
        body: protocol::encode(&body),
      })
      .map_err(|_| ProxyError::new(StatusCode::BAD_GATEWAY, "Host tunnel is busy or disconnected"))?;
    let (status, _content_type) = tokio::time::timeout(Duration::from_secs(30), ready)
      .await
      .map_err(|_| {
        ProxyError::new(
          StatusCode::GATEWAY_TIMEOUT,
          "Host response timed out; requests are never replayed",
        )
      })?
      .map_err(|_| ProxyError::new(StatusCode::BAD_GATEWAY, "Host disconnected"))?
      .map_err(|error| ProxyError::new(StatusCode::BAD_GATEWAY, error))?;
    let stream = async_stream::try_stream! {
      let _guard = guard;
      loop {
        let next = tokio::select! {
          biased;
          _ = connection.shutdown.cancelled() => Err(io::Error::other("Host disconnected or access revoked")),
          next = tokio::time::timeout(Duration::from_secs(90), received.recv()) =>
            next.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Host response stalled")),
        }?;
        let Some(chunk) = next else { break };
        yield chunk;
        if connection.requests.lock().unwrap().contains_key(&request_id) {
          tokio::time::timeout(IO_TIMEOUT, connection.outgoing.send(Frame::Window { request_id, credits: 1 })).await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Host response flow control stalled"))?
            .map_err(|_| io::Error::other("Host disconnected"))?;
        }
      }
      let error = failure.lock().unwrap().take();
      if let Some(error) = error { Err::<(), _>(io::Error::other(error))?; }
    };
    let stream = futures_util::StreamExt::map(stream, |item: Result<Bytes, io::Error>| item);
    let response = Response::builder()
      .status(status)
      .header(header::CACHE_CONTROL, "no-store")
      .header("x-accel-buffering", "no")
      .header("x-content-type-options", "nosniff")
      .header(
        header::CONTENT_TYPE,
        if path == "/api/v1/events" {
          "text/event-stream"
        } else {
          "application/json"
        },
      );
    response
      .body(Body::from_stream(stream))
      .map_err(|_| ProxyError::new(StatusCode::BAD_GATEWAY, "Invalid host response"))
  }

  /// One native client socket carries one encrypted request or event stream.
  /// Neither public metadata nor a Hub login grants authority at the host.
  pub async fn handle_secure_socket(&self, host_id: &str, mut socket: WebSocket, _permit: OwnedSemaphorePermit) {
    let connection = self.inner.lock().unwrap().online.get(host_id).cloned();
    let Some(connection) = connection.filter(|connection| !connection.shutdown.is_cancelled()) else {
      return;
    };
    let channel_id = connection.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Include handshake, headers and terminal records beyond the chunk window.
    let (sender, mut received) = mpsc::channel(protocol::RESPONSE_WINDOW + 4);
    {
      let mut channels = connection.secure_channels.lock().unwrap();
      if channels.len() >= protocol::MAX_REQUESTS {
        return;
      }
      channels.insert(channel_id, sender);
    }
    let _guard = SecureGuard {
      connection: connection.clone(),
      channel_id,
    };
    let _ = async {
      secure_enqueue(&connection, Frame::SecureOpen { channel_id }).await?;
      let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
      let mut last_received = Instant::now();
      loop {
        tokio::select! {
          biased;
          _ = connection.shutdown.cancelled() => break,
          record = received.recv() => {
            let Some(record) = record else { break };
            send_message(&mut socket, Message::Binary(record)).await?;
          }
          incoming = socket.recv() => {
            last_received = Instant::now();
            match incoming {
              Some(Ok(Message::Binary(record))) if !record.is_empty() && record.len() <= protocol::MAX_SECURE_RECORD => {
                secure_enqueue(&connection, Frame::SecureData { channel_id, data: protocol::encode(&record) }).await?;
              }
              Some(Ok(Message::Ping(data))) => send_message(&mut socket, Message::Pong(data)).await?,
              Some(Ok(Message::Pong(_))) => {},
              _ => break,
            }
          }
          _ = heartbeat.tick() => {
            if last_received.elapsed() > Duration::from_secs(45) { break; }
            send_message(&mut socket, Message::Ping(Bytes::new())).await?;
          }
        }
      }
      Ok::<_, String>(())
    }.await;
  }
}

async fn secure_enqueue(connection: &Connection, frame: Frame) -> Result<(), String> {
  tokio::time::timeout(IO_TIMEOUT, connection.outgoing.send(frame))
    .await
    .map_err(|_| "Secure tunnel stalled")?
    .map_err(|_| "Secure tunnel closed".into())
}

impl Connection {
  fn receive(&self, frame: Frame) -> Result<(), String> {
    match frame {
      Frame::SecureOnly {} => {
        self.secure_only.store(true, std::sync::atomic::Ordering::Relaxed);
        return Ok(());
      }
      Frame::SecureData { channel_id, data } => {
        let bytes = protocol::decode(&data, protocol::MAX_SECURE_RECORD)?;
        if bytes.is_empty() {
          return Err("Empty encrypted record".into());
        }
        let mut channels = self.secure_channels.lock().unwrap();
        if let Some(channel) = channels.get(&channel_id) {
          if channel.try_send(Bytes::from(bytes)).is_err() {
            channels.remove(&channel_id);
            if self.outgoing.try_send(Frame::SecureClose { channel_id }).is_err() {
              self.shutdown.cancel();
            }
          }
        }
        return Ok(());
      }
      Frame::SecureClose { channel_id } => {
        self.secure_channels.lock().unwrap().remove(&channel_id);
        return Ok(());
      }
      _ => {}
    }
    let mut requests = self.requests.lock().unwrap();
    match frame {
      Frame::Response {
        request_id,
        status,
        content_type,
      } => {
        if !(200..=599).contains(&status)
          || content_type
            .as_ref()
            .is_some_and(|value| value.len() > 256 || axum::http::HeaderValue::from_str(value).is_err())
        {
          return Err("Invalid response headers".into());
        }
        if let Some(request) = requests.get_mut(&request_id) {
          let headers = request.headers.take().ok_or("Duplicate response headers")?;
          let _ = headers.send(Ok((status, content_type)));
        }
      }
      Frame::Chunk { request_id, data } => {
        let bytes = protocol::decode(&data, protocol::CHUNK_SIZE)?;
        if let Some(request) = requests.get(&request_id) {
          if request.headers.is_some() {
            return Err("Response body preceded headers".into());
          }
          if request.chunks.try_send(Bytes::from(bytes)).is_err() {
            let request = requests.remove(&request_id).unwrap();
            *request.failure.lock().unwrap() =
              Some("Client could not keep up with the host stream; reconnect to refresh".into());
            if self.outgoing.try_send(Frame::Cancel { request_id }).is_err() {
              self.shutdown.cancel();
            }
          }
        }
      }
      Frame::End { request_id } => {
        if let Some(request) = requests.remove(&request_id) {
          if let Some(headers) = request.headers {
            let _ = headers.send(Err("Host ended before sending response headers".into()));
          }
        }
      }
      Frame::Error { request_id, message } => {
        if message.len() > 1024 {
          return Err("Host error is too large".into());
        }
        if let Some(request) = requests.remove(&request_id) {
          *request.failure.lock().unwrap() = Some(message.clone());
          if let Some(headers) = request.headers {
            let _ = headers.send(Err(message));
          }
        }
      }
      _ => return Err("Unexpected message from host".into()),
    }
    Ok(())
  }

  fn fail_all(&self, message: &str) {
    self.secure_channels.lock().unwrap().clear();
    for (_, request) in self.requests.lock().unwrap().drain() {
      *request.failure.lock().unwrap() = Some(message.into());
      if let Some(headers) = request.headers {
        let _ = headers.send(Err(message.into()));
      }
    }
  }
}

/// Only intentional, public registration failures may leave the Hub. Database
/// errors can contain local details and never become remote diagnostics.
fn registration_diagnostic(error: &str) -> &'static str {
  match error {
    "New host registration rate limit reached; retry in one minute" => {
      "New host registration rate limit reached; retry in one minute"
    }
    "Encrypted host registration capacity reached; a Hub administrator can revoke unused hosts" => {
      "Encrypted host registration capacity reached; a Hub administrator can revoke unused hosts"
    }
    "Encrypted host revocation ledger is full; Hub administrator maintenance is required before registering new hosts" => {
      "Encrypted host revocation ledger is full; Hub administrator maintenance is required before registering new hosts"
    }
    "Host UUID registration is revoked or belongs to a different identity" => {
      "Host UUID registration is revoked or belongs to a different identity"
    }
    "Host UUID is registered to a different identity" => "Host UUID is registered to a different identity",
    _ => "Hub could not register this host; contact its administrator",
  }
}

async fn send(socket: &mut WebSocket, frame: &Frame) -> Result<(), String> {
  send_message(
    socket,
    Message::Text(serde_json::to_string(frame).map_err(|e| e.to_string())?.into()),
  )
  .await
}

async fn send_message(socket: &mut WebSocket, message: Message) -> Result<(), String> {
  tokio::time::timeout(IO_TIMEOUT, socket.send(message))
    .await
    .map_err(|_| "Tunnel send timed out")?
    .map_err(|_| "Tunnel disconnected".into())
}

#[cfg(test)]
#[path = "tunnel_tests.rs"]
mod tests;
