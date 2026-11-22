//! Selection of physical record carriers; viewer commands never get retried here.
use crate::{Connection, exchange::Exchange, pinned_relay, session::Session};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, watch};
use tokn_hub_client_core::{protocol, secure::InnerMessage};
use tokn_hub_transport::WebRtcPeer;

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

#[derive(Default)]
struct Channels {
  direct: Option<(WebRtcPeer, Arc<Session>)>,
  relay: Option<Arc<Session>>,
}

pub(crate) struct DirectRoute {
  channels: Mutex<Channels>,
  pub changes: watch::Sender<TransportState>,
}

impl Default for DirectRoute {
  fn default() -> Self {
    Self {
      channels: Mutex::default(),
      changes: watch::channel(TransportState::default()).0,
    }
  }
}

impl DirectRoute {
  pub async fn session(&self, connection: &Connection) -> Result<Arc<Session>, String> {
    let mut channels = self.channels.lock().await;
    if let Some((peer, session)) = &channels.direct {
      if !peer.is_closed() && !session.stopped().is_cancelled() {
        return Ok(session.clone());
      }
    }
    // Serialize first use (and recovery) so simultaneous requests perform
    // exactly one handshake. Only unsent work can select a new carrier.
    pinned_relay::session(
      &mut channels.relay,
      &connection.endpoint,
      &connection.identity,
      &connection.info.host_public_key,
      &connection.cancellation,
    )
    .await
  }

  fn publish(&self, connection: &Connection, state: TransportState) {
    self.changes.send_replace(state.clone());
    connection.transport_event(state);
  }

  pub async fn close(&self) {
    let channels = std::mem::take(&mut *self.channels.lock().await);
    if let Some((peer, _)) = channels.direct {
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

async fn candidate(connection: &Connection) -> Result<(Candidate, Arc<Session>), String> {
  let relay = connection.direct.session(connection).await?;
  let InnerMessage::DirectConfig { ice_servers } = relay.request_reply(InnerMessage::DirectConfigRequest {}).await?
  else {
    return Err("Host does not support direct connections".into());
  };
  let (peer, offer) = WebRtcPeer::offer(ice_servers).await?;
  let peer = Candidate(Some(peer));
  let InnerMessage::DirectAnswer { sdp } = relay.request_reply(InnerMessage::DirectOffer { sdp: offer }).await? else {
    return Err("Host did not answer direct connection negotiation".into());
  };
  peer.peer().accept_answer(&sdp).await?;
  let session = Session::authenticate(
    peer.peer().open_record().await?,
    &connection.identity,
    &connection.info.host_public_key,
    connection.cancellation.clone(),
  )
  .await?;
  let health = Exchange::open(&session, "GET", "/api/v1/health", &[])
    .await?
    .json()
    .await?;
  if health.get("version").and_then(serde_json::Value::as_u64) != Some(protocol::VIEWER_API_VERSION) {
    return Err("Direct machine uses an unsupported viewer API version. Update the machine's viewer.".into());
  }
  Ok((peer, session))
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
  let (mut candidate, session) = match result {
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
  // Publish the exact channel that passed the probe. Requests already on
  // the relay retain their senders until their responses have drained.
  let mut installed = connection.direct.channels.lock().await;
  if connection.cancellation.is_cancelled() {
    return;
  }
  let peer = candidate.0.take().expect("candidate owns peer");
  let session_closed = session.stopped();
  installed.direct = Some((peer.clone(), session));
  installed.relay.take();
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
  let draining = tokio::select! {
    _ = connection.cancellation.cancelled() => false,
    _ = retiring.cancelled() => true,
    _ = closed.cancelled() => false,
    _ = session_closed.cancelled() => false,
  };
  connection.direct.channels.lock().await.direct.take();
  if !connection.cancellation.is_cancelled() {
    connection.direct.publish(
      &connection,
      TransportState {
        kind: TransportKind::Relay,
        reason: Some("Direct connection ended; using the encrypted Hub relay".into()),
      },
    );
  }
  if draining {
    let cancellation = connection.cancellation.clone();
    tokio::spawn(async move {
      tokio::select! {
        _ = cancellation.cancelled() => peer.close().await,
        _ = peer.close_when_idle() => {},
      }
    });
  } else {
    peer.close().await;
  }
}
