//! Account-owned host connections and bounded encrypted tunnel multiplexing.
use crate::{
  protocol::{self, Frame},
  store::{HostRecord, Store},
};
use axum::{
  body::Bytes,
  extract::ws::{Message, WebSocket},
};
use ed25519_dalek::{Signature, VerifyingKey};
use rand::{RngCore, rngs::OsRng};
use std::{
  collections::HashMap,
  sync::{Arc, Mutex, Weak},
  time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::{discovery::DiscoveryGroup, presence::MachinePresence};

const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct HubTunnels {
  store: Store,
  inner: Arc<Mutex<HubInner>>,
  handshakes: Arc<Semaphore>,
  approved: Arc<Semaphore>,
  secure_capacity: Arc<Semaphore>,
  shutdown: CancellationToken,
}

#[derive(Default)]
struct HubInner {
  online: HashMap<String, Arc<Connection>>,
}

struct Connection {
  // Only host socket handlers own strong leases. An old client socket cannot
  // retain host capacity after its connector disconnects.
  capacity: Weak<OwnedSemaphorePermit>,
  outgoing: mpsc::Sender<Frame>,
  secure_channels: Mutex<HashMap<u64, mpsc::Sender<Bytes>>>,
  next_id: std::sync::atomic::AtomicU64,
  allow_control: bool,
  connector_version: Option<String>,
  shutdown: CancellationToken,
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

impl HubTunnels {
  pub fn new(store: Store) -> Self {
    Self::with_limits(store, 32, 64)
  }

  fn with_limits(store: Store, handshakes: usize, approved: usize) -> Self {
    Self {
      store,
      inner: Arc::new(Mutex::new(HubInner::default())),
      handshakes: Arc::new(Semaphore::new(handshakes)),
      approved: Arc::new(Semaphore::new(approved)),
      secure_capacity: Arc::new(Semaphore::new(128)),
      shutdown: CancellationToken::new(),
    }
  }

  pub fn revoke(&self, host_id: &str) -> Result<(), String> {
    let mut inner = self.inner.lock().unwrap();
    self.store.remove_host(host_id)?;
    if let Some(connection) = inner.online.remove(host_id) {
      connection.shutdown.cancel();
    }
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

  pub(crate) fn machine_presence(&self, host_id: String, include_version: bool) -> MachinePresence {
    let inner = self.inner.lock().unwrap();
    let connection = inner
      .online
      .get(&host_id)
      .filter(|connection| !connection.shutdown.is_cancelled());
    let connector_version = if include_version {
      connection.and_then(|connection| connection.connector_version.clone())
    } else {
      None
    };
    MachinePresence {
      host_id,
      online: connection.is_some(),
      connector_version,
    }
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

  pub fn reserve_secure_channel(&self) -> Result<OwnedSemaphorePermit, String> {
    self
      .secure_capacity
      .clone()
      .try_acquire_owned()
      .map_err(|_| "Hub secure channel capacity reached".into())
  }

  pub fn shutdown(&self) {
    self.shutdown.cancel();
    let inner = self.inner.lock().unwrap();
    for connection in inner.online.values() {
      connection.shutdown.cancel();
    }
  }

  pub async fn handle_socket(&self, socket: WebSocket) {
    self.handle_socket_with_version(socket, None).await;
  }

  pub(crate) async fn handle_socket_with_version(&self, mut socket: WebSocket, connector_version: Option<String>) {
    let Ok(handshake_permit) = self.handshakes.clone().try_acquire_owned() else {
      return;
    };
    let result = tokio::time::timeout(IO_TIMEOUT, self.authenticate(&mut socket)).await;
    let Ok(Ok(authenticated)) = result else {
      return;
    };
    drop(handshake_permit);
    let registered = match authenticated {
      Some((record, group)) => self.store.register_tunnel_host(record, group.as_ref()),
      None => Err(crate::store::ACCOUNT_REQUIRED.into()),
    };
    let record = match registered {
      Ok(record) => record,
      Err(error) => {
        let _ = send(
          &mut socket,
          &Frame::RegistrationRejected {
            message: registration_diagnostic(&error).into(),
          },
        )
        .await;
        return;
      }
    };
    let (outgoing, mut outgoing_rx) = mpsc::channel(64);
    let (connection, _approved_permit) = {
      // Recheck under the same lock as revoke so an identity revoked during
      // the handshake cannot install a connection.
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
        secure_channels: Mutex::new(HashMap::new()),
        next_id: std::sync::atomic::AtomicU64::new(1),
        allow_control: record.access == "control" && stored.access == "control",
        connector_version,
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
    connection.secure_channels.lock().unwrap().clear();
    let mut inner = self.inner.lock().unwrap();
    if inner
      .online
      .get(&record.host_id)
      .is_some_and(|current| Arc::ptr_eq(current, &connection))
    {
      inner.online.remove(&record.host_id);
    }
  }

  async fn authenticate(&self, socket: &mut WebSocket) -> Result<Option<(HostRecord, Option<DiscoveryGroup>)>, String> {
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
    Ok(registered_id.map(|host_id| {
      let record = HostRecord {
        host_id,
        public_key,
        name,
        access: if allow_control { "control" } else { "view" }.into(),
      };
      (record, discovery_group)
    }))
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
      Frame::SecureOnly {} => Ok(()),
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
        Ok(())
      }
      Frame::SecureClose { channel_id } => {
        self.secure_channels.lock().unwrap().remove(&channel_id);
        Ok(())
      }
      _ => Err("Unexpected message from host".into()),
    }
  }
}

/// Only intentional, public registration failures may leave the Hub. Database
/// errors can contain local details and never become remote diagnostics.
fn registration_diagnostic(error: &str) -> &'static str {
  match error {
    crate::store::ACCOUNT_REQUIRED => crate::store::ACCOUNT_REQUIRED,
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
