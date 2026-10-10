use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use webrtc::api::APIBuilder;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::{BoxTransport, MAX_RECORD, RECORD_CHANNEL_LABEL, RecordTransport, TransportResult, validate_record};

const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_SDP: usize = 32 * 1024;
const RECORD_QUEUE: usize = 16;
// The native request limit is 32, with one SSE and one health exchange.
const MAX_CHANNELS: usize = 34;
// The upstream peer retains closed data-channel descriptors until peer close.
// Bound the lifetime total as well as simultaneous channels.
const MAX_TOTAL_CHANNELS: usize = 512;
const MAX_BUFFERED: usize = 4 * MAX_RECORD;
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const RETIREMENT_TIMEOUT: Duration = Duration::from_secs(120);

/// One authenticated machine's direct carrier. Every request opens a separate
/// channel; each channel must perform its own Noise handshake above this layer.
#[derive(Clone)]
pub struct WebRtcPeer {
  inner: Arc<PeerInner>,
}

struct PeerInner {
  peer: Arc<RTCPeerConnection>,
  closed: CancellationToken,
  retiring: CancellationToken,
  incoming: Mutex<mpsc::Receiver<BoxTransport>>,
  first: Mutex<Option<DataChannelTransport>>,
  channels: Arc<Semaphore>,
  total_channels: Arc<AtomicUsize>,
  last_activity: Arc<StdMutex<Instant>>,
}

impl WebRtcPeer {
  /// Gather the offer's ICE candidates in one bounded signaling message.
  pub async fn offer(stun_urls: Vec<String>) -> TransportResult<(Self, String)> {
    let this = Self::build(stun_urls).await?;
    let result = timeout(NEGOTIATION_TIMEOUT, async {
      let first = this.create_record().await?;
      *this.inner.first.lock().await = Some(first);
      let offer = this.inner.peer.create_offer(None).await.map_err(rtc_error)?;
      this.gather(offer).await
    })
    .await
    .map_err(|_| "WebRTC offer timed out".to_string())
    .and_then(|result| result);
    match result {
      Ok(sdp) => Ok((this, sdp)),
      Err(error) => {
        this.close().await;
        Err(error)
      }
    }
  }

  /// Answer a complete offer. No unauthenticated signaling belongs here: the
  /// host caller must authorize the device before allocating this peer.
  pub async fn answer(stun_urls: Vec<String>, offer_sdp: &str) -> TransportResult<(Self, String)> {
    validate_sdp(offer_sdp)?;
    let this = Self::build(stun_urls).await?;
    let result = timeout(NEGOTIATION_TIMEOUT, async {
      let offer = RTCSessionDescription::offer(offer_sdp.to_owned()).map_err(rtc_error)?;
      this.inner.peer.set_remote_description(offer).await.map_err(rtc_error)?;
      let answer = this.inner.peer.create_answer(None).await.map_err(rtc_error)?;
      this.gather(answer).await
    })
    .await
    .map_err(|_| "WebRTC answer timed out".to_string())
    .and_then(|result| result);
    match result {
      Ok(sdp) => Ok((this, sdp)),
      Err(error) => {
        this.close().await;
        Err(error)
      }
    }
  }

  pub async fn accept_answer(&self, answer_sdp: &str) -> TransportResult<()> {
    validate_sdp(answer_sdp)?;
    let answer = RTCSessionDescription::answer(answer_sdp.to_owned()).map_err(rtc_error)?;
    timeout(NEGOTIATION_TIMEOUT, self.inner.peer.set_remote_description(answer))
      .await
      .map_err(|_| "WebRTC remote answer timed out".to_string())?
      .map_err(rtc_error)
  }

  pub async fn open_record(&self) -> TransportResult<BoxTransport> {
    if self.is_closed() {
      return Err("WebRTC peer is closed".into());
    }
    if self.inner.retiring.is_cancelled() {
      return Err("WebRTC peer is retiring".into());
    }
    let transport = match self.inner.first.lock().await.take() {
      Some(transport) => transport,
      None => self.create_record().await?,
    };
    transport.wait_open().await?;
    Ok(Box::new(transport))
  }

  /// Accept the next independently encrypted exchange. The queue and live
  /// channel count are bounded even if an authenticated device misbehaves.
  pub async fn accept_record(&self) -> TransportResult<Option<BoxTransport>> {
    let mut incoming = self.inner.incoming.lock().await;
    tokio::select! {
      biased;
      _ = self.inner.closed.cancelled() => Ok(None),
      transport = incoming.recv() => Ok(transport),
    }
  }

  pub fn closed(&self) -> CancellationToken {
    self.inner.closed.clone()
  }

  pub fn is_closed(&self) -> bool {
    self.inner.closed.is_cancelled()
  }

  /// Stop allocating new exchanges while already captured carriers finish.
  /// A route owner should move its read-only stream and new requests away, then
  /// retain this peer in `close_when_idle` until its outstanding leases drain.
  pub fn retire(&self) {
    self.inner.retiring.cancel();
  }

  pub fn retiring(&self) -> CancellationToken {
    self.inner.retiring.clone()
  }

  pub async fn close_when_idle(&self) {
    self.retire();
    self.inner.first.lock().await.take();
    let deadline = Instant::now() + RETIREMENT_TIMEOUT;
    while self.inner.channels.available_permits() != MAX_CHANNELS && !self.is_closed() {
      tokio::select! {
        _ = self.inner.closed.cancelled() => break,
        _ = tokio::time::sleep_until(deadline) => break,
        _ = tokio::time::sleep(Duration::from_millis(50)) => {},
      }
    }
    self.close().await;
  }

  pub async fn close(&self) {
    self.inner.closed.cancel();
    self.inner.first.lock().await.take();
    let _ = timeout(SEND_TIMEOUT, self.inner.peer.close()).await;
  }

  async fn gather(&self, description: RTCSessionDescription) -> TransportResult<String> {
    // Register before set_local_description so fast local gathering cannot race
    // the completion receiver.
    let mut complete = self.inner.peer.gathering_complete_promise().await;
    self
      .inner
      .peer
      .set_local_description(description)
      .await
      .map_err(rtc_error)?;
    tokio::select! {
      _ = self.inner.closed.cancelled() => return Err("WebRTC peer closed during ICE gathering".into()),
      _ = complete.recv() => {},
    }
    let description = self
      .inner
      .peer
      .local_description()
      .await
      .ok_or_else(|| "WebRTC local description is missing".to_string())?;
    validate_sdp(&description.sdp)?;
    Ok(description.sdp)
  }

  async fn create_record(&self) -> TransportResult<DataChannelTransport> {
    if self.inner.retiring.is_cancelled() {
      return Err("WebRTC peer is retiring".into());
    }
    let permit = self
      .inner
      .channels
      .clone()
      .try_acquire_owned()
      .map_err(|_| "too many WebRTC record channels".to_string())?;
    if self.inner.total_channels.fetch_add(1, Ordering::Relaxed) >= MAX_TOTAL_CHANNELS {
      self.retire();
      return Err("WebRTC peer exchange limit reached".into());
    }
    let channel = self
      .inner
      .peer
      .create_data_channel(
        RECORD_CHANNEL_LABEL,
        Some(RTCDataChannelInit {
          ordered: Some(true),
          ..Default::default()
        }),
      )
      .await
      .map_err(rtc_error)?;
    touch(&self.inner.last_activity);
    Ok(DataChannelTransport::new(
      channel,
      self.inner.closed.child_token(),
      self.inner.last_activity.clone(),
      permit,
    ))
  }

  async fn build(stun_urls: Vec<String>) -> TransportResult<Self> {
    validate_stun_urls(&stun_urls)?;
    let mut settings = SettingEngine::default();
    // Native clients may open a machine published on the same computer. Some
    // operating systems cannot route ICE back through their own LAN addresses.
    // Signaling is already authorized/encrypted before these candidates leave.
    settings.set_include_loopback_candidate(true);
    // Recover quickly through the existing relay when direct traffic stops.
    settings.set_ice_timeouts(
      Some(Duration::from_secs(5)),
      Some(Duration::from_secs(10)),
      Some(Duration::from_secs(2)),
    );
    let api = APIBuilder::new().with_setting_engine(settings).build();
    let ice_servers = if stun_urls.is_empty() {
      Vec::new()
    } else {
      vec![RTCIceServer {
        urls: stun_urls,
        ..Default::default()
      }]
    };
    let peer = Arc::new(
      api
        .new_peer_connection(RTCConfiguration {
          ice_servers,
          ..Default::default()
        })
        .await
        .map_err(rtc_error)?,
    );
    let closed = CancellationToken::new();
    let retiring = CancellationToken::new();
    let state_closed = closed.clone();
    peer.on_peer_connection_state_change(Box::new(move |state| {
      if matches!(
        state,
        RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
      ) {
        state_closed.cancel();
      }
      Box::pin(async {})
    }));
    let (incoming_tx, incoming) = mpsc::channel(MAX_CHANNELS);
    let channels = Arc::new(Semaphore::new(MAX_CHANNELS));
    let total_channels = Arc::new(AtomicUsize::new(0));
    let last_activity = Arc::new(StdMutex::new(Instant::now()));
    let accepted_channels = channels.clone();
    let accepted_closed = closed.clone();
    let accepted_activity = last_activity.clone();
    let accepted_total = total_channels.clone();
    peer.on_data_channel(Box::new(move |channel| {
      let incoming_tx = incoming_tx.clone();
      let permit = accepted_channels.clone().try_acquire_owned();
      let closed = accepted_closed.child_token();
      let activity = accepted_activity.clone();
      let over_limit = accepted_total.fetch_add(1, Ordering::Relaxed) >= MAX_TOTAL_CHANNELS;
      if over_limit {
        accepted_closed.cancel();
      }
      Box::pin(async move {
        if !valid_channel(&channel) || closed.is_cancelled() || permit.is_err() || over_limit {
          close_when_open(&channel);
          return;
        }
        touch(&activity);
        let transport: BoxTransport = Box::new(DataChannelTransport::new(channel, closed, activity, permit.unwrap()));
        // Overflow closes this exchange rather than making callbacks block the
        // peer's SCTP receive loop indefinitely.
        if let Err(error) = incoming_tx.try_send(transport) {
          let mut transport = error.into_inner();
          transport.close().await;
        }
      })
    }));
    let inner = Arc::new(PeerInner {
      peer: peer.clone(),
      closed: closed.clone(),
      retiring,
      incoming: Mutex::new(incoming),
      first: Mutex::new(None),
      channels: channels.clone(),
      total_channels,
      last_activity: last_activity.clone(),
    });
    // The cleanup task never owns PeerInner, so dropping all handles releases
    // sockets. It also bounds abandoned negotiations and idle peers.
    let weak = Arc::downgrade(&inner);
    tokio::spawn(async move {
      let startup_deadline = Instant::now() + Duration::from_secs(20);
      loop {
        tokio::select! {
          _ = closed.cancelled() => break,
          _ = tokio::time::sleep(Duration::from_secs(5)) => {},
        }
        if weak.upgrade().is_none() {
          break;
        }
        let state = peer.connection_state();
        if state != RTCPeerConnectionState::Connected && Instant::now() > startup_deadline {
          break;
        }
        if channels.available_permits() == MAX_CHANNELS && last_activity.lock().unwrap().elapsed() > PEER_IDLE_TIMEOUT {
          break;
        }
      }
      closed.cancel();
      let _ = timeout(SEND_TIMEOUT, peer.close()).await;
    });
    Ok(Self { inner })
  }
}

impl Drop for PeerInner {
  fn drop(&mut self) {
    self.closed.cancel();
  }
}

struct DataChannelTransport {
  channel: Arc<RTCDataChannel>,
  incoming: mpsc::Receiver<Vec<u8>>,
  closed: CancellationToken,
  opened: Arc<Notify>,
  error: Arc<StdMutex<Option<String>>>,
  activity: Arc<StdMutex<Instant>>,
  _permit: OwnedSemaphorePermit,
}

impl DataChannelTransport {
  fn new(
    channel: Arc<RTCDataChannel>,
    closed: CancellationToken,
    activity: Arc<StdMutex<Instant>>,
    permit: OwnedSemaphorePermit,
  ) -> Self {
    let (incoming_tx, incoming) = mpsc::channel(RECORD_QUEUE);
    let opened = Arc::new(Notify::new());
    let error = Arc::new(StdMutex::new(None));
    let open_notify = opened.clone();
    channel.on_open(Box::new(move || {
      open_notify.notify_waiters();
      Box::pin(async {})
    }));
    let channel_closed = closed.clone();
    channel.on_close(Box::new(move || {
      channel_closed.cancel();
      Box::pin(async {})
    }));
    let channel_error = error.clone();
    let channel_failed = closed.clone();
    channel.on_error(Box::new(move |_| {
      *channel_error.lock().unwrap() = Some("WebRTC data channel failed".into());
      channel_failed.cancel();
      Box::pin(async {})
    }));
    let message_error = error.clone();
    let message_closed = closed.clone();
    let message_channel = Arc::downgrade(&channel);
    let message_activity = activity.clone();
    channel.on_message(Box::new(move |message| {
      let incoming_tx = incoming_tx.clone();
      let error = message_error.clone();
      let closed = message_closed.clone();
      let channel = message_channel.clone();
      let activity = message_activity.clone();
      Box::pin(async move {
        let reason = if message.is_string || validate_record(&message.data).is_err() {
          Some("invalid WebRTC record")
        } else {
          // The callback is awaited by the SCTP read loop. Waiting on the
          // bounded queue applies backpressure to legitimate request-body
          // bursts instead of treating scheduling delay as a protocol error.
          let queued = tokio::select! {
            _ = closed.cancelled() => return,
            result = timeout(SEND_TIMEOUT, incoming_tx.send(message.data.to_vec())) => {
              matches!(result, Ok(Ok(())))
            },
          };
          if queued {
            touch(&activity);
            None
          } else {
            Some("WebRTC receive buffer stalled")
          }
        };
        if let Some(reason) = reason {
          *error.lock().unwrap() = Some(reason.into());
          closed.cancel();
          if let Some(channel) = channel.upgrade() {
            tokio::spawn(async move {
              let _ = channel.close().await;
            });
          }
        }
      })
    }));
    Self {
      channel,
      incoming,
      closed,
      opened,
      error,
      activity,
      _permit: permit,
    }
  }

  async fn wait_open(&self) -> TransportResult<()> {
    timeout(NEGOTIATION_TIMEOUT, async {
      loop {
        let opened = self.opened.notified();
        if self.closed.is_cancelled() {
          return Err("WebRTC data channel is closed".into());
        }
        if self.channel.ready_state() == RTCDataChannelState::Open {
          return Ok(());
        }
        tokio::select! {
          _ = self.closed.cancelled() => return Err("WebRTC data channel closed while opening".into()),
          _ = opened => {},
        }
      }
    })
    .await
    .map_err(|_| "WebRTC data channel opening timed out".to_string())?
  }
}

#[async_trait]
impl RecordTransport for DataChannelTransport {
  async fn send(&mut self, record: Vec<u8>) -> TransportResult<()> {
    validate_record(&record)?;
    self.wait_open().await?;
    let result = timeout(SEND_TIMEOUT, async {
      while self.channel.buffered_amount().await + record.len() > MAX_BUFFERED {
        tokio::select! {
          _ = self.closed.cancelled() => return Err("WebRTC data channel is closed".to_string()),
          _ = tokio::time::sleep(Duration::from_millis(10)) => {},
        }
      }
      let record = Bytes::from(record);
      tokio::select! {
        _ = self.closed.cancelled() => Err("WebRTC data channel is closed".to_string()),
        result = self.channel.send(&record) => result.map(|_| ()).map_err(rtc_error),
      }
    })
    .await
    .map_err(|_| "WebRTC send timed out; delivery is uncertain".to_string())
    .and_then(|result| result);
    if result.is_err() {
      self.close().await;
    } else {
      touch(&self.activity);
    }
    result
  }

  async fn receive(&mut self) -> TransportResult<Option<Vec<u8>>> {
    if let Some(error) = self.error.lock().unwrap().clone() {
      return Err(error);
    }
    let result = timeout(READ_TIMEOUT, async {
      tokio::select! {
        biased;
        record = self.incoming.recv() => Ok(record),
        _ = self.closed.cancelled() => match self.error.lock().unwrap().clone() {
          Some(error) => Err(error),
          None => Ok(self.incoming.try_recv().ok()),
        },
      }
    })
    .await
    .map_err(|_| "WebRTC record receive timed out".to_string())?;
    result
  }

  async fn close(&mut self) {
    self.closed.cancel();
    self.incoming.close();
    if self.channel.ready_state() == RTCDataChannelState::Connecting {
      close_when_open(&self.channel);
    }
    let _ = timeout(SEND_TIMEOUT, self.channel.close()).await;
  }
}

impl Drop for DataChannelTransport {
  fn drop(&mut self) {
    self.closed.cancel();
    let channel = self.channel.clone();
    if channel.ready_state() == RTCDataChannelState::Connecting {
      close_when_open(&channel);
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
      runtime.spawn(async move {
        let _ = timeout(SEND_TIMEOUT, channel.close()).await;
      });
    }
  }
}

fn close_when_open(channel: &Arc<RTCDataChannel>) {
  // on_data_channel runs before the upstream channel's handle_open. Closing
  // there directly would be undone when handle_open installs the SCTP stream.
  let weak = Arc::downgrade(channel);
  channel.on_open(Box::new(move || {
    Box::pin(async move {
      if let Some(channel) = weak.upgrade() {
        let _ = timeout(SEND_TIMEOUT, channel.close()).await;
      }
    })
  }));
}

fn valid_channel(channel: &RTCDataChannel) -> bool {
  channel.label() == RECORD_CHANNEL_LABEL
    && channel.ordered()
    && channel.max_packet_lifetime().is_none()
    && channel.max_retransmits().is_none()
    && !channel.negotiated()
}

pub fn validate_stun_urls(urls: &[String]) -> TransportResult<()> {
  if urls.len() > 8 {
    return Err("WebRTC requires at most eight STUN URLs".into());
  }
  for url in urls {
    if !url.starts_with("stun:")
      || url.len() > 512
      || url.contains(['@', '/', '\\', '?', '#'])
      || url.ends_with(':')
      || url.contains(|character: char| character.is_whitespace() || character.is_control())
    {
      return Err("WebRTC STUN URL is invalid".into());
    }
    let parsed = webrtc::ice::url::Url::parse_url(url).map_err(|_| "WebRTC STUN URL is invalid".to_string())?;
    // This backend gathers server-reflexive candidates with UDP STUN only.
    // Accepting stuns would silently send UDP instead of using TLS.
    if parsed.scheme != webrtc::ice::url::SchemeType::Stun
      || parsed.host.is_empty()
      || parsed.port == 0
      || !parsed.username.is_empty()
      || !parsed.password.is_empty()
    {
      return Err("WebRTC STUN URL must use stun without credentials".into());
    }
  }
  Ok(())
}

fn validate_sdp(sdp: &str) -> TransportResult<()> {
  if sdp.is_empty() || sdp.len() > MAX_SDP {
    return Err("WebRTC signaling description size is invalid".into());
  }
  for line in sdp.lines() {
    if let Some(value) = line.trim().strip_prefix("a=max-message-size:") {
      let size = value
        .parse::<usize>()
        .map_err(|_| "invalid WebRTC message size".to_string())?;
      if size != 0 && size < MAX_RECORD {
        return Err("WebRTC peer cannot carry encrypted records".into());
      }
    }
  }
  Ok(())
}

fn touch(activity: &StdMutex<Instant>) {
  *activity.lock().unwrap() = Instant::now();
}

fn rtc_error(error: webrtc::Error) -> String {
  format!("WebRTC transport failed: {error}")
}

#[cfg(test)]
mod tests {
  use super::*;

  async fn pair() -> (WebRtcPeer, WebRtcPeer) {
    let (client, offer) = WebRtcPeer::offer(Vec::new()).await.unwrap();
    let (host, answer) = WebRtcPeer::answer(Vec::new(), &offer).await.unwrap();
    client.accept_answer(&answer).await.unwrap();
    (client, host)
  }

  #[test]
  fn signaling_and_stun_configuration_are_bounded() {
    assert!(validate_sdp("").is_err());
    assert!(validate_sdp(&"a".repeat(MAX_SDP + 1)).is_err());
    assert!(validate_sdp("a=max-message-size:16384\r\n").is_err());
    assert!(validate_sdp("a=max-message-size:65536\r\n").is_ok());
    assert!(validate_sdp("a=max-message-size:0\r\n").is_ok());
    assert!(validate_stun_urls(&["turn:example.com".into()]).is_err());
    assert!(validate_stun_urls(&["stuns:example.com".into()]).is_err());
    assert!(validate_stun_urls(&["stun:example.com:3478".into()]).is_ok());
    assert!(validate_stun_urls(&["stun:user:secret@example.com:3478".into()]).is_err());
    assert!(validate_stun_urls(&["stun:example.com:0".into()]).is_err());
    assert!(validate_stun_urls(&["stun:example.com/path".into()]).is_err());
    assert!(validate_stun_urls(&["stun:example.com\\path".into()]).is_err());
    assert!(validate_stun_urls(&["stun:example.com:".into()]).is_err());
    assert!(validate_stun_urls(&vec!["stun:example.com".into(); 9]).is_err());
  }

  #[tokio::test]
  async fn native_peers_exchange_full_size_records_on_multiple_channels() {
    let (client, host) = pair().await;
    for round in 0..3 {
      let mut outgoing = client.open_record().await.unwrap();
      let record = vec![round; MAX_RECORD];
      outgoing.send(record.clone()).await.unwrap();
      let mut incoming = timeout(NEGOTIATION_TIMEOUT, host.accept_record())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
      assert_eq!(incoming.receive().await.unwrap(), Some(record));
      incoming.send(vec![round + 1; 64]).await.unwrap();
      assert_eq!(outgoing.receive().await.unwrap(), Some(vec![round + 1; 64]));
      assert!(outgoing.send(vec![0; MAX_RECORD + 1]).await.is_err());
      outgoing.close().await;
      incoming.close().await;
    }
    client.close().await;
    host.close().await;
    assert!(client.is_closed());
    assert!(client.open_record().await.is_err());
  }

  #[tokio::test]
  async fn closing_peer_interrupts_pending_receive() {
    let (client, host) = pair().await;
    let mut outgoing = client.open_record().await.unwrap();
    outgoing.send(vec![1]).await.unwrap();
    let mut incoming = host.accept_record().await.unwrap().unwrap();
    assert_eq!(incoming.receive().await.unwrap(), Some(vec![1]));
    host.close().await;
    assert_eq!(incoming.receive().await.unwrap(), None);
    client.close().await;
  }

  #[tokio::test]
  async fn receive_backpressure_preserves_request_body_bursts() {
    let (client, host) = pair().await;
    let mut outgoing = client.open_record().await.unwrap();
    let writer = tokio::spawn(async move {
      for index in 0..32 {
        outgoing.send(vec![index; 44 * 1024]).await.unwrap();
      }
      outgoing
    });
    let mut incoming = host.accept_record().await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    for index in 0..32 {
      assert_eq!(incoming.receive().await.unwrap(), Some(vec![index; 44 * 1024]));
    }
    writer.await.unwrap().close().await;
    incoming.close().await;
    client.close().await;
    host.close().await;
  }

  #[tokio::test]
  async fn text_messages_fail_the_binary_record_exchange() {
    let (client, host) = pair().await;
    let outgoing = client.inner.first.lock().await.take().unwrap();
    outgoing.wait_open().await.unwrap();
    let mut incoming = host.accept_record().await.unwrap().unwrap();
    outgoing.channel.send_text("not a Noise record").await.unwrap();
    assert!(incoming.receive().await.unwrap_err().contains("invalid WebRTC record"));
    drop(outgoing);
    incoming.close().await;
    client.close().await;
    host.close().await;
  }

  #[tokio::test]
  async fn dropping_last_handle_cancels_the_peer() {
    let (peer, _) = WebRtcPeer::offer(Vec::new()).await.unwrap();
    let closed = peer.closed();
    let retained = peer.clone();
    drop(peer);
    assert!(!closed.is_cancelled());
    drop(retained);
    timeout(Duration::from_secs(1), closed.cancelled()).await.unwrap();
  }

  #[tokio::test]
  async fn unexpected_channels_close_after_sctp_open() {
    let (client, host) = pair().await;
    let mut outgoing = client.open_record().await.unwrap();
    let mut incoming = host.accept_record().await.unwrap().unwrap();
    let channel = client.inner.peer.create_data_channel("unexpected", None).await.unwrap();
    timeout(Duration::from_secs(5), async {
      while channel.ready_state() != RTCDataChannelState::Closed {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .unwrap();
    assert!(host.inner.incoming.lock().await.is_empty());
    outgoing.close().await;
    incoming.close().await;
    client.close().await;
    host.close().await;
  }

  #[tokio::test]
  async fn peer_lifetime_exchange_limit_bounds_retained_descriptors() {
    let (peer, _) = WebRtcPeer::offer(Vec::new()).await.unwrap();
    peer.inner.total_channels.store(MAX_TOTAL_CHANNELS, Ordering::Relaxed);
    assert!(peer.create_record().await.is_err());
    assert!(peer.retiring().is_cancelled());
    assert!(!peer.is_closed());
    peer.close_when_idle().await;
    assert!(peer.is_closed());
  }

  #[tokio::test]
  async fn temporary_channel_capacity_does_not_retire_or_close_the_peer() {
    let (peer, _) = WebRtcPeer::offer(Vec::new()).await.unwrap();
    let capacity = peer
      .inner
      .channels
      .clone()
      .try_acquire_many_owned((MAX_CHANNELS - 1) as u32)
      .unwrap();
    assert!(peer.create_record().await.is_err());
    assert!(!peer.retiring().is_cancelled());
    assert!(!peer.is_closed());
    drop(capacity);
    peer.close().await;
  }

  #[tokio::test]
  async fn retiring_peer_keeps_inflight_records_until_their_leases_drain() {
    let (client, host) = pair().await;
    let mut outgoing = client.open_record().await.unwrap();
    let mut incoming = host.accept_record().await.unwrap().unwrap();
    client.inner.total_channels.store(MAX_TOTAL_CHANNELS, Ordering::Relaxed);
    assert!(client.open_record().await.is_err());
    assert!(client.retiring().is_cancelled());
    assert!(!client.is_closed());
    let draining = client.clone();
    let cleanup = tokio::spawn(async move { draining.close_when_idle().await });
    outgoing.send(vec![1]).await.unwrap();
    assert_eq!(incoming.receive().await.unwrap(), Some(vec![1]));
    incoming.send(vec![2]).await.unwrap();
    assert_eq!(outgoing.receive().await.unwrap(), Some(vec![2]));
    assert!(!client.is_closed());
    outgoing.close().await;
    drop(outgoing);
    timeout(Duration::from_secs(2), cleanup).await.unwrap().unwrap();
    assert!(client.is_closed());
    incoming.close().await;
    host.close().await;
  }
}
