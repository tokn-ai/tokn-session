//! Selection of physical record carriers; viewer commands never get retried here.
use crate::{
  Connection,
  exchange::{self, Exchange},
};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, watch};
use tokn_hub_client_core::secure::{InnerMessage, NoiseInitiator};
use tokn_hub_transport::{BoxTransport, WebRtcPeer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
  Direct,
  Relay,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TransportState {
  pub kind: TransportKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reason: Option<String>,
}

impl Default for TransportState {
  fn default() -> Self {
    Self {
      kind: TransportKind::Relay,
      reason: None,
    }
  }
}

pub(crate) struct DirectRoute {
  peer: Mutex<Option<WebRtcPeer>>,
  pub changes: watch::Sender<TransportState>,
}

impl Default for DirectRoute {
  fn default() -> Self {
    Self {
      peer: Mutex::new(None),
      changes: watch::channel(TransportState::default()).0,
    }
  }
}

impl DirectRoute {
  pub async fn open(&self, connection: &Connection) -> Result<BoxTransport, String> {
    let peer = self.peer.lock().await.clone();
    if let Some(peer) = peer {
      // Falling back before opening an exchange is safe: no command has been
      // encrypted or sent. A failed exchange itself is never replayed.
      if !peer.is_closed() {
        if let Ok(transport) = peer.open_record().await {
          return Ok(transport);
        }
      }
      if peer.is_closed() {
        self
          .fallback(connection, "Direct connection lost; using the encrypted Hub relay")
          .await;
      }
      // Local admission can be exhausted by independent exchanges. Route
      // this new request through the relay without disrupting existing work.
    }
    exchange::relay_transport(&connection.endpoint).await
  }

  fn publish(&self, connection: &Connection, state: TransportState) {
    self.changes.send_replace(state.clone());
    connection.transport_event(state);
  }

  async fn fallback(&self, connection: &Connection, reason: &str) {
    let peer = self.peer.lock().await.take();
    self.publish(
      connection,
      TransportState {
        kind: TransportKind::Relay,
        reason: Some(reason.into()),
      },
    );
    if let Some(peer) = peer {
      peer.close().await;
    }
  }

  pub async fn close(&self) {
    if let Some(peer) = self.peer.lock().await.take() {
      peer.close().await;
    }
  }
}

/// A negotiation that is cancelled midway must also release its UDP sockets.
struct Candidate(Option<WebRtcPeer>);
impl Candidate {
  fn peer(&self) -> &WebRtcPeer {
    self.0.as_ref().expect("candidate owns peer")
  }
}
impl Drop for Candidate {
  fn drop(&mut self) {
    if let Some(peer) = self.0.take() {
      tokio::spawn(async move {
        peer.close().await;
      });
    }
  }
}

async fn signal(connection: &Connection, message: InnerMessage) -> Result<InnerMessage, String> {
  let mut transport = exchange::relay_transport(&connection.endpoint).await?;
  let mut initiator = NoiseInitiator::new(&connection.identity, &connection.info.host_public_key)?;
  transport.send(initiator.start()?).await?;
  let mut channel = initiator.finish(&exchange::read_record(&mut transport).await?)?;
  transport.send(channel.encrypt(&message)?).await?;
  let reply = channel.decrypt(&exchange::read_record(&mut transport).await?)?;
  transport.close().await;
  match reply {
    InnerMessage::Error { message } => Err(message),
    reply => Ok(reply),
  }
}

async fn candidate(connection: &Connection) -> Result<Candidate, String> {
  let InnerMessage::DirectConfig { ice_servers } = signal(connection, InnerMessage::DirectConfigRequest {}).await?
  else {
    return Err("Host does not support direct connections".into());
  };
  let (peer, offer) = WebRtcPeer::offer(ice_servers).await?;
  let peer = Candidate(Some(peer));
  let InnerMessage::DirectAnswer { sdp } = signal(connection, InnerMessage::DirectOffer { sdp: offer }).await? else {
    return Err("Host did not answer direct connection negotiation".into());
  };
  peer.peer().accept_answer(&sdp).await?;
  let health = Exchange::open(
    peer.peer().open_record().await?,
    &connection.identity,
    &connection.info.host_public_key,
    "GET",
    "/api/v1/health",
    &[],
  )
  .await?
  .json()
  .await?;
  if health.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
    return Err("Direct machine uses an unsupported viewer API version".into());
  }
  Ok(peer)
}

pub(crate) async fn upgrade(connection: Arc<Connection>) {
  loop {
    if connection.cancellation.is_cancelled() {
      return;
    }
    upgrade_once(connection.clone()).await;
    // ICE may become possible after a network change, or a peer may retire
    // after bounded channel turnover. Retrying only negotiation/health keeps
    // the relay usable and never replays an application command.
    tokio::select! {
      _ = connection.cancellation.cancelled() => return,
      _ = tokio::time::sleep(Duration::from_secs(30)) => {},
    }
  }
}

async fn upgrade_once(connection: Arc<Connection>) {
  let result = tokio::select! {
    biased;
    _ = connection.cancellation.cancelled() => return,
    result = tokio::time::timeout(Duration::from_secs(20), candidate(&connection)) =>
      result.map_err(|_| "Direct negotiation timed out".to_owned()).and_then(|result| result),
  };
  let mut candidate = match result {
    Ok(peer) => peer,
    Err(_) => {
      if !connection.cancellation.is_cancelled() {
        connection.direct.publish(
          &connection,
          TransportState {
            kind: TransportKind::Relay,
            reason: Some("Direct connection unavailable; using the encrypted Hub relay".into()),
          },
        );
      }
      return;
    }
  };
  // Keep the peer alive until the machine is closed, independently of Hub
  // connectivity. Each request captures its own carrier and Noise channel.
  let mut installed = connection.direct.peer.lock().await;
  if connection.cancellation.is_cancelled() {
    return;
  }
  let peer = candidate.0.take().expect("candidate owns peer");
  *installed = Some(peer.clone());
  drop(installed);
  connection.direct.publish(
    &connection,
    TransportState {
      kind: TransportKind::Direct,
      reason: None,
    },
  );
  let closed = peer.closed();
  let retiring = peer.retiring();
  tokio::select! {
    _ = connection.cancellation.cancelled() => connection.direct.close().await,
    _ = retiring.cancelled() => {
      connection.direct.peer.lock().await.take();
      connection.direct.publish(&connection, TransportState {
        kind: TransportKind::Relay,
        reason: Some("Refreshing the direct connection; using the encrypted Hub relay".into()),
      });
      let cancellation = connection.cancellation.clone();
      tokio::spawn(async move {
        tokio::select! {
          _ = cancellation.cancelled() => peer.close().await,
          _ = peer.close_when_idle() => {},
        }
      });
    },
    _ = closed.cancelled() => {
      if !connection.cancellation.is_cancelled() {
        connection.direct.fallback(&connection, "Direct connection lost; using the encrypted Hub relay").await;
      }
    },
  }
}
