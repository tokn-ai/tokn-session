//! Host-owned trust boundary for encrypted requests. The Hub is only a carrier.
#[path = "connector_requests.rs"]
mod requests;

use super::{ConnectorConfig, PairedHostConfig};
use crate::{
  host_passkeys::{CEREMONY_SECONDS, HostPasskeys},
  protocol::{self, Frame},
  secure::{HostAuthOperation, InnerMessage, NoiseIdentity, NoiseResponder, SecureChannel},
};

use std::{
  sync::Arc,
  time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::discovery::DiscoveryGroup;
use tokn_hub_transport::{BoxTransport, WebRtcPeer};

pub(super) fn discovery_group(config: &ConnectorConfig) -> Result<Option<DiscoveryGroup>, String> {
  crate::account_host::membership_group(config)
}

#[derive(Clone)]
enum RecordOutput {
  Relay {
    sender: mpsc::Sender<Frame>,
    channel_id: u64,
  },
  Direct(mpsc::Sender<Vec<u8>>),
}

impl RecordOutput {
  async fn send(&self, record: Vec<u8>) -> Result<(), String> {
    self.send_with(|| Ok(record)).await
  }

  // Reserve capacity before consuming a Noise nonce. The policy timer can
  // cancel a stalled write and still deliver a decryptable revocation error.
  async fn send_with(&self, record: impl FnOnce() -> Result<Vec<u8>, String>) -> Result<(), String> {
    async fn reserve<T>(sender: &mpsc::Sender<T>) -> Result<mpsc::Permit<'_, T>, String> {
      tokio::time::timeout(Duration::from_secs(10), sender.reserve())
        .await
        .map_err(|_| "Encrypted transport stalled; delivery may be uncertain")?
        .map_err(|_| "Encrypted transport closed".into())
    }
    match self {
      Self::Relay { sender, channel_id } => reserve(sender).await?.send(Frame::SecureData {
        channel_id: *channel_id,
        data: protocol::encode(&record()?),
      }),
      Self::Direct(sender) => reserve(sender).await?.send(record()?),
    }
    Ok(())
  }
}

/// Peers outlive individual Hub tunnel connections. Only connector shutdown,
/// peer failure, or host-owned authorization expiry/revocation retires them.
pub(super) struct DirectManager {
  host: Arc<Host>,
  host_id: String,
  config: ConnectorConfig,
  client: reqwest::Client,
  shutdown: CancellationToken,
  peers: Arc<Semaphore>,
  channels: Arc<Semaphore>,
}

impl DirectManager {
  pub(super) fn new(
    host: Arc<Host>,
    host_id: String,
    config: ConnectorConfig,
    client: reqwest::Client,
    shutdown: CancellationToken,
  ) -> Arc<Self> {
    Arc::new(Self {
      host,
      host_id,
      config,
      client,
      shutdown,
      peers: Arc::new(Semaphore::new(16)),
      channels: Arc::new(Semaphore::new(128)),
    })
  }

  async fn negotiate(self: &Arc<Self>, recipient: &str, sdp: &str) -> Result<(String, NegotiatedPeer), String> {
    let permit = self
      .peers
      .clone()
      .try_acquire_owned()
      .map_err(|_| "Direct peer capacity reached; use encrypted Hub relay")?;
    let (peer, answer) = tokio::select! {
      _ = self.shutdown.cancelled() => return Err("Host connector is shutting down".into()),
      result = tokio::time::timeout(Duration::from_secs(20), WebRtcPeer::answer(self.config.ice_servers.clone(), sdp)) => {
        result.map_err(|_| "Direct negotiation timed out; use encrypted Hub relay")??
      }
    };
    self.host.verify(&self.host_id, recipient)?;
    Ok((
      answer,
      NegotiatedPeer {
        manager: self.clone(),
        recipient: recipient.into(),
        peer,
        _permit: permit,
      },
    ))
  }

  async fn serve_peer(self: &Arc<Self>, peer: WebRtcPeer, recipient: String) {
    let mut tasks = tokio::task::JoinSet::new();
    let closed = peer.closed();
    let mut policy_check = tokio::time::interval(Duration::from_secs(1));
    let mut revoked = false;
    loop {
      tokio::select! {
        biased;
        _ = self.shutdown.cancelled() => break,
        _ = closed.cancelled() => break,
        _ = policy_check.tick() => {
          if self.host.verify(&self.host_id, &recipient).is_err() {
            revoked = true;
            break;
          }
        }
        accepted = peer.accept_record() => {
          let Ok(Some(mut transport)) = accepted else { break; };
          let Ok(permit) = self.channels.clone().try_acquire_owned() else {
            transport.close().await;
            continue;
          };
          let manager = self.clone();
          let recipient = recipient.clone();
          tasks.spawn(async move {
            let _permit = permit;
            manager.serve_record(transport, &recipient).await;
          });
        }
        _ = tasks.join_next(), if !tasks.is_empty() => {},
      }
    }
    if revoked {
      // Every running exchange checks trust each second. Let it send its
      // authenticated terminal error before shutting down SCTP/UDP, so a quiet
      // recipient does not have to wait for ICE failure to observe revocation.
      // Incomplete handshakes/bodies remain bounded and are cancelled below.
      let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while tasks.join_next().await.is_some() {}
      })
      .await;
    }
    tasks.abort_all();
    peer.close().await;
  }

  async fn serve_record(self: &Arc<Self>, mut transport: BoxTransport, recipient: &str) {
    let (incoming, received) = mpsc::channel(16);
    let (outgoing, mut records) = mpsc::channel(16);
    let output = RecordOutput::Direct(outgoing);
    let exchange = run_records(
      &self.host,
      &self.host_id,
      &self.config,
      &self.client,
      &output,
      received,
      Some(recipient),
      None,
    );
    tokio::pin!(exchange);
    loop {
      tokio::select! {
        biased;
        _ = self.shutdown.cancelled() => break,
        _ = &mut exchange => {
          // A terminal record may have just been enqueued by the exchange.
          while let Ok(record) = records.try_recv() {
            if transport.send(record).await.is_err() { break; }
          }
          break;
        }
        record = records.recv() => {
          let Some(record) = record else { break; };
          if transport.send(record).await.is_err() { break; }
        }
        result = async {
          let slot = incoming.reserve().await.map_err(|_| "Host closed incoming stream")?;
          let record = transport.receive().await?.ok_or("Direct connection closed")?;
          slot.send(bytes::Bytes::from(record));
          Ok::<(), String>(())
        } => {
          if result.is_err() { break; }

        }
      }
    }
    transport.close().await;
  }
}

// Dropping an unsent answer drops the WebRTC peer and releases its capacity.
// Only delivery on the authenticated signaling channel starts serving it.
struct NegotiatedPeer {
  manager: Arc<DirectManager>,
  recipient: String,
  peer: WebRtcPeer,
  _permit: tokio::sync::OwnedSemaphorePermit,
}

impl NegotiatedPeer {
  fn start(self) {
    tokio::spawn(async move {
      let _permit = self._permit;
      self.manager.serve_peer(self.peer, self.recipient).await;
    });
  }
}

pub(super) struct Host {
  identity: NoiseIdentity,
  paired: PairedHostConfig,
  passkeys: Option<HostPasskeys>,
  requests: Arc<Semaphore>,
  discovery_group: Option<DiscoveryGroup>,
}

impl Host {
  pub(super) fn load(config: &ConnectorConfig) -> Result<Self, String> {
    let paired = &config.paired;
    crate::onboarding::read_totp_secret(&paired.state_file)?;
    let profile = paired
      .state_file
      .parent()
      .map(|directory| crate::onboarding::HostProfile::load(&directory.join("host.json")))
      .transpose()?
      .flatten();
    if profile
      .as_ref()
      .is_some_and(|profile| profile.host_id != paired.host_id)
    {
      return Err("Saved passkey host configuration does not match this connector".into());
    }
    let origin = match profile.and_then(|profile| profile.passkey_origin) {
      Some(origin) => Some(
        crate::host_passkeys::validate_origin(&origin)?
          .origin()
          .ascii_serialization(),
      ),
      None => {
        let origin = crate::onboarding::canonical_hub(&config.hub_url)?;
        crate::host_passkeys::validate_origin(&origin)
          .ok()
          .map(|origin| origin.origin().ascii_serialization())
      }
    };
    crate::onboarding::validate_host_passkey_origin(&paired.state_file, origin.as_deref())?;
    let passkeys = origin
      .map(|origin| HostPasskeys::new(paired.state_file.clone(), &paired.host_id, &config.name, &origin))
      .transpose()?;
    Ok(Self {
      identity: NoiseIdentity::load_or_create(&paired.noise_key_file)?,
      paired: paired.clone(),
      passkeys,
      requests: Arc::new(Semaphore::new(protocol::MAX_REQUESTS)),
      discovery_group: discovery_group(config)?,
    })
  }

  pub(super) fn public_key(&self) -> String {
    self.identity.public_key()
  }

  fn verify(&self, host_id: &str, recipient: &str) -> Result<(), String> {
    if self.paired.host_id != host_id {
      return Err("Incorrect paired host target".into());
    }
    if crate::onboarding::is_authorized(&self.paired.state_file, recipient)? {
      Ok(())
    } else {
      Err("Device is not paired with this host".into())
    }
  }

  fn verify_runtime_management(
    &self,
    config: &ConnectorConfig,
    recipient: &str,
    method: &str,
    path: &str,
  ) -> Result<(), String> {
    if !protocol::runtime_management_route(method, path) {
      return Ok(());
    }
    if !config.allow_control || !crate::onboarding::can_manage_runtimes(&self.paired.state_file, recipient)? {
      return Err(
        "Remote runtime management is disabled on this host; enable it locally in Settings → Agent runtimes".into(),
      );
    }
    Ok(())
  }

  async fn pair(
    &self,
    host_id: &str,
    first: &[u8],
    incoming: &mut mpsc::Receiver<bytes::Bytes>,
    outgoing: &RecordOutput,
  ) -> Result<(), String> {
    let config = &self.paired;
    if config.host_id != host_id {
      return Err("Incorrect pairing target".into());
    }
    let step = crate::pairing::peek_step(first)?;
    let secret = crate::onboarding::read_totp_secret(&config.state_file)?;
    crate::onboarding::begin_pairing(&config.state_file, now()?, step)?;
    let (pending, reply) = crate::pairing::HostPairing::respond(&secret, host_id, &self.identity, first, now()?)?;
    send_record(outgoing, reply).await?;
    let (authenticated, ack) =
      pending.finish_with_discovery(&receive(incoming).await?, now()?, self.discovery_group.clone())?;
    // Persist consumption and authorization together before letting the client
    // save its pin. Concurrent completions with the same TOTP step lose here.
    crate::onboarding::authorize_pairing(
      &config.state_file,
      &authenticated.client_public_key,
      authenticated.step,
      now()?,
      authenticated.client_kind,
    )?;
    send_record(outgoing, ack).await
  }

  async fn authenticate(
    &self,
    host_id: &str,
    operation: HostAuthOperation,
    payload: serde_json::Value,
    channel: &mut SecureChannel,
    incoming: &mut mpsc::Receiver<bytes::Bytes>,
    outgoing: &RecordOutput,
  ) -> Result<(), String> {
    let config = &self.paired;
    if config.host_id != host_id {
      return Err("Incorrect passkey authentication target".into());
    }
    let passkeys = self
      .passkeys
      .as_ref()
      .ok_or("Configure --passkey-origin with the Hub's stable browser origin to use host passkeys")?;
    let peer = channel.remote_public_key().to_owned();
    let binding = channel.channel_binding().to_owned();
    let (pending, payload) = passkeys.start(operation, payload, &peer, &binding, now()?)?;
    send_record(outgoing, channel.encrypt(&InnerMessage::AuthResponse { payload })?).await?;
    let finish = tokio::time::timeout(Duration::from_secs(CEREMONY_SECONDS), incoming.recv())
      .await
      .map_err(|_| "Host passkey ceremony expired; start again")?
      .ok_or("Host passkey channel closed")?;
    let InnerMessage::AuthRequest { operation, payload } = channel.decrypt(&finish)? else {
      return Err("Finish the host passkey ceremony on its original encrypted channel".into());
    };
    let payload = passkeys.finish(pending, operation, payload, &peer, &binding, now()?)?;
    send_record(outgoing, channel.encrypt(&InnerMessage::AuthResponse { payload })?).await
  }
}

fn now() -> Result<u64, String> {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|value| value.as_secs())
    .map_err(|_| "Invalid host clock".into())
}

async fn send_record(outgoing: &RecordOutput, record: Vec<u8>) -> Result<(), String> {
  outgoing.send(record).await
}

async fn receive(incoming: &mut mpsc::Receiver<bytes::Bytes>) -> Result<bytes::Bytes, String> {
  tokio::time::timeout(Duration::from_secs(10), incoming.recv())
    .await
    .map_err(|_| "Encrypted request timed out")?
    .ok_or_else(|| "Encrypted channel closed".into())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
  host: &Host,
  host_id: &str,
  config: &ConnectorConfig,
  client: &reqwest::Client,
  outgoing: &mpsc::Sender<Frame>,
  channel_id: u64,
  incoming: mpsc::Receiver<bytes::Bytes>,
  direct: Option<&Arc<DirectManager>>,
) -> Result<(), String> {
  let output = RecordOutput::Relay {
    sender: outgoing.clone(),
    channel_id,
  };
  run_records(host, host_id, config, client, &output, incoming, None, direct).await
}

#[allow(clippy::too_many_arguments)]
async fn run_records(
  host: &Host,
  host_id: &str,
  config: &ConnectorConfig,
  client: &reqwest::Client,
  outgoing: &RecordOutput,
  mut incoming: mpsc::Receiver<bytes::Bytes>,
  expected_device: Option<&str>,
  direct: Option<&Arc<DirectManager>>,
) -> Result<(), String> {
  let first = receive(&mut incoming).await?;
  if crate::pairing::is_pairing_record(&first) {
    if expected_device.is_some() {
      return Err("Pair this device through the encrypted Hub channel first".into());
    }
    return host.pair(host_id, &first, &mut incoming, outgoing).await;
  }
  let (reply, mut channel) = NoiseResponder::new(&host.identity)?.accept(&first)?;
  // A peer is created only for the device that authenticated its signaling.
  // A fresh Noise IK on each DataChannel still checks the host pin and key.
  if expected_device.is_some_and(|expected| expected != channel.remote_public_key()) {
    return Err("Direct channel identity differs from its authenticated signaling device".into());
  }
  send_record(outgoing, reply).await?;
  let header = channel.decrypt(&receive(&mut incoming).await?)?;
  if let InnerMessage::AuthRequest { operation, payload } = header {
    if expected_device.is_some() {
      return Err("Authenticate this device through the encrypted Hub channel first".into());
    }
    let result = host
      .authenticate(host_id, operation, payload, &mut channel, &mut incoming, outgoing)
      .await;
    if let Err(message) = &result {
      if let Ok(record) = channel.encrypt(&InnerMessage::Error {
        message: message.clone(),
      }) {
        let _ = send_record(outgoing, record).await;
      }
    }
    return result;
  }
  if let InnerMessage::DiscoveryRequest {} = header {
    let result = host
      .verify(host_id, channel.remote_public_key())
      .and_then(|_| discovery_group(config));
    let frame = match &result {
      Ok(group) => InnerMessage::DiscoveryResponse { group: group.clone() },
      Err(message) => InnerMessage::Error {
        message: message.clone(),
      },
    };
    outgoing.send(channel.encrypt(&frame)?).await?;
    return result.map(|_| ());
  }
  requests::serve(
    host,
    host_id,
    config,
    client,
    channel,
    incoming,
    outgoing,
    header,
    expected_device.is_none(),
    direct,
  )
  .await
}

async fn forward_device(
  config: &ConnectorConfig,
  client: &reqwest::Client,
  method: &str,
  path: &str,
  body: Vec<u8>,
) -> Result<reqwest::Response, String> {
  // run_records verified the encrypted peer's active device authorization and
  // both host permissions before it admits a runtime-management path here.
  if !protocol::allowed_route(method, path, config.allow_control) && !protocol::runtime_management_route(method, path) {
    return Err("Route unavailable or host control is disabled".into());
  }
  if !config.allow_control
    && let Some(value) =
      protocol::read_only_session_control_response(path, &body, "Agent control is disabled on this host")
  {
    return Ok(reqwest::Response::from(
      axum::http::Response::builder()
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&value).map_err(|_| "Could not encode control status")?)
        .map_err(|_| "Could not encode control status")?,
    ));
  }
  if !config.allow_control
    && let Some(value) = protocol::read_only_model_response(path, "Agent control is disabled on this host")
  {
    return Ok(reqwest::Response::from(
      axum::http::Response::builder()
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&value).map_err(|_| "Could not encode model status")?)
        .map_err(|_| "Could not encode model status")?,
    ));
  }
  if matches!(
    path,
    "/api/v1/get_session_input_status" | "/api/v1/get_session_question_status"
  ) && !config.allow_control
  {
    return Ok(reqwest::Response::from(
      axum::http::Response::builder()
        .header("content-type", "application/json")
        .body(
          serde_json::to_vec(&serde_json::json!({
            "available": false, "message": "Agent input is disabled on this host", "max_length": 0, "question_ids": [],
            "images_available": false,
            "images_stageable": false, "max_images": 0, "max_image_bytes": 0,
            "max_total_image_bytes": 0, "image_mime_types": []
          }))
          .map_err(|_| "Could not encode input status")?,
        )
        .map_err(|_| "Could not encode input status")?,
    ));
  }
  let mut url = config.local_url.clone();
  url.set_path(path);
  let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| "Invalid request method")?;
  let mut request = client
    .request(method, url)
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .body(body);
  if let Some(token) = &config.local_token {
    request = request.bearer_auth(token);
  }
  request
    .send()
    .await
    .map_err(|_| "Local API request failed; delivery may be uncertain and will not be retried".into())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::secure::NoiseInitiator;
  use futures_util::StreamExt;
  use serde_json::{Value, json};
  use std::{
    path::Path,
    sync::{Arc, Mutex},
  };
  use webauthn_authenticator_rs::{WebauthnAuthenticator, softpasskey::SoftPasskey};

  fn paired_host(directory: &Path) -> ConnectorConfig {
    let state_file = directory.join("host-access.json");
    crate::onboarding::initialize_host_access(&state_file, &crate::pairing::TotpSecret::generate()).unwrap();
    ConnectorConfig {
      hub_url: "https://hub.example.com".parse().unwrap(),
      local_url: "http://127.0.0.1:5558".parse().unwrap(),
      key_file: directory.join("host-enrollment.key"),
      name: "Workstation".into(),
      local_token: None,
      allow_control: false,
      insecure_loopback: false,
      ice_servers: Vec::new(),
      paired: PairedHostConfig {
        host_id: "11111111-1111-4111-8111-111111111111".into(),
        noise_key_file: directory.join("host-noise.key"),
        state_file,
      },
    }
  }

  struct TestInput(mpsc::Sender<bytes::Bytes>);
  impl TestInput {
    async fn send(&self, record: Vec<u8>) -> Result<(), mpsc::error::SendError<bytes::Bytes>> {
      self.0.send(record.into()).await
    }
  }

  async fn open_host(
    config: &ConnectorConfig,
    device: &NoiseIdentity,
  ) -> (
    tokio::task::JoinHandle<Result<(), String>>,
    TestInput,
    mpsc::Receiver<Frame>,
    SecureChannel,
  ) {
    open_host_with_direct(config, device, None).await
  }

  async fn open_host_with_direct(
    config: &ConnectorConfig,
    device: &NoiseIdentity,
    direct: Option<Arc<DirectManager>>,
  ) -> (
    tokio::task::JoinHandle<Result<(), String>>,
    TestInput,
    mpsc::Receiver<Frame>,
    SecureChannel,
  ) {
    let host = Arc::new(Host::load(config).unwrap());
    let mut initiator = NoiseInitiator::new(device, &host.public_key()).unwrap();
    let config = config.clone();
    let host_id = config.paired.host_id.clone();
    let (incoming, received) = mpsc::channel(16);
    let (outgoing, mut frames) = mpsc::channel(16);
    let task = tokio::spawn(async move {
      run(
        &host,
        &host_id,
        &config,
        &reqwest::Client::new(),
        &outgoing,
        1,
        received,
        direct.as_ref(),
      )
      .await
    });
    incoming.send(initiator.start().unwrap().into()).await.unwrap();
    let Frame::SecureData { data, .. } = frames.recv().await.unwrap() else {
      panic!("Expected Noise handshake response");
    };
    let channel = initiator
      .finish(&protocol::decode(&data, protocol::MAX_SECURE_RECORD).unwrap())
      .unwrap();
    (task, TestInput(incoming), frames, channel)
  }

  async fn read_inner(frames: &mut mpsc::Receiver<Frame>, channel: &mut SecureChannel) -> InnerMessage {
    let Frame::SecureData { data, .. } = frames.recv().await.unwrap() else {
      panic!("Expected encrypted response");
    };
    channel
      .decrypt(&protocol::decode(&data, protocol::MAX_SECURE_RECORD).unwrap())
      .unwrap()
  }

  async fn stream_send(input: &TestInput, channel: &mut SecureChannel, id: u32, frame: InnerMessage) {
    input
      .send(
        channel
          .encrypt(&InnerMessage::Stream {
            stream_id: id,
            frame: Box::new(frame),
          })
          .unwrap(),
      )
      .await
      .unwrap();
  }

  async fn stream_frame(output: &mut mpsc::Receiver<Frame>, channel: &mut SecureChannel, id: u32) -> InnerMessage {
    let InnerMessage::Stream { stream_id, frame } =
      tokio::time::timeout(Duration::from_secs(5), read_inner(output, channel))
        .await
        .expect("The other stream must make progress")
    else {
      panic!("Expected stream response")
    };
    assert_eq!(stream_id, id);
    *frame
  }

  #[tokio::test]
  async fn one_handshake_serves_discovery_commands_and_events_with_independent_credits() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = paired_host(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.local_url = format!("http://{}/", listener.local_addr().unwrap()).parse().unwrap();
    let api = tokio::spawn(async move {
      let app = axum::Router::new()
        .route(
          "/api/v1/events",
          axum::routing::post(|| async { vec![b'e'; protocol::CHUNK_SIZE * (protocol::RESPONSE_WINDOW + 2)] }),
        )
        .route(
          "/api/v1/health",
          axum::routing::get(|| async { axum::Json(json!({"version":protocol::VIEWER_API_VERSION})) }),
        );
      axum::serve(listener, app).await.unwrap();
    });
    let device = NoiseIdentity::generate().unwrap();
    let state = &config.paired.state_file;
    let current = now().unwrap();
    crate::onboarding::authorize_device(state, &device.public_key(), current / 30, current).unwrap();
    let (task, input, mut output, mut channel) = open_host(&config, &device).await;
    let start = |method: &str, command: &str| InnerMessage::DeviceRequest {
      method: method.into(),
      path: format!("/api/v1/{command}"),
    };
    stream_send(&input, &mut channel, 1, InnerMessage::DiscoveryRequest {}).await;
    assert_eq!(
      stream_frame(&mut output, &mut channel, 1).await,
      InnerMessage::DiscoveryResponse { group: None }
    );

    stream_send(&input, &mut channel, 2, start("POST", "events")).await;
    stream_send(&input, &mut channel, 2, InnerMessage::RequestEnd {}).await;
    assert!(matches!(
      stream_frame(&mut output, &mut channel, 2).await,
      InnerMessage::Response { status: 200, .. }
    ));
    for _ in 0..protocol::RESPONSE_WINDOW {
      assert!(matches!(
        stream_frame(&mut output, &mut channel, 2).await,
        InnerMessage::Chunk { .. }
      ));
    }
    // Leave the event stream out of credit. A denied command must complete
    // independently, and a later read can still use the same Noise channel.
    stream_send(&input, &mut channel, 3, start("POST", "start_rpc_runtime")).await;
    stream_send(&input, &mut channel, 3, InnerMessage::RequestEnd {}).await;
    assert!(matches!(
      stream_frame(&mut output, &mut channel, 3).await,
      InnerMessage::Error { .. }
    ));
    for id in [4, 5] {
      stream_send(&input, &mut channel, id, start("GET", "health")).await;
      stream_send(&input, &mut channel, id, InnerMessage::RequestEnd {}).await;
      assert!(matches!(
        stream_frame(&mut output, &mut channel, id).await,
        InnerMessage::Response { status: 200, .. }
      ));
      assert!(matches!(
        stream_frame(&mut output, &mut channel, id).await,
        InnerMessage::Chunk { .. }
      ));
      assert!(matches!(
        stream_frame(&mut output, &mut channel, id).await,
        InnerMessage::End {}
      ));
      stream_send(&input, &mut channel, id, InnerMessage::Window { credits: 1 }).await;
    }
    // Revocation must terminate even a completely silent, stalled subscription.
    crate::onboarding::remove_device(state, &device.public_key()).unwrap();
    assert!(matches!(
      tokio::time::timeout(Duration::from_secs(4), read_inner(&mut output, &mut channel))
        .await
        .unwrap(),
      InnerMessage::Error { .. }
    ));
    assert!(task.await.unwrap().is_err());
    api.abort();
  }

  #[tokio::test]
  async fn a_cancelled_stream_cannot_reuse_its_identity() {
    let directory = tempfile::tempdir().unwrap();
    let config = paired_host(directory.path());
    let device = NoiseIdentity::generate().unwrap();
    let current = now().unwrap();
    crate::onboarding::authorize_device(&config.paired.state_file, &device.public_key(), current / 30, current)
      .unwrap();
    let (task, input, mut output, mut channel) = open_host(&config, &device).await;
    let start = InnerMessage::DeviceRequest {
      method: "GET".into(),
      path: "/api/v1/health".into(),
    };
    stream_send(&input, &mut channel, 1, start.clone()).await;
    stream_send(&input, &mut channel, 1, InnerMessage::Cancel {}).await;
    stream_send(&input, &mut channel, 2, InnerMessage::DiscoveryRequest {}).await;
    assert!(matches!(
      stream_frame(&mut output, &mut channel, 2).await,
      InnerMessage::DiscoveryResponse { .. }
    ));
    stream_send(&input, &mut channel, 1, start).await;
    assert!(matches!(
      read_inner(&mut output, &mut channel).await,
      InnerMessage::Error { .. }
    ));
    assert!(task.await.unwrap().is_err());
  }

  async fn authenticate_wire(
    config: &ConnectorConfig,
    device: &NoiseIdentity,
    authenticator: &mut WebauthnAuthenticator<SoftPasskey>,
    register: bool,
  ) -> Value {
    let (task, incoming, mut frames, mut channel) = open_host(config, device).await;
    let (start, finish) = if register {
      (HostAuthOperation::RegisterStart, HostAuthOperation::RegisterFinish)
    } else {
      (HostAuthOperation::LoginStart, HostAuthOperation::LoginFinish)
    };
    incoming
      .send(
        channel
          .encrypt(&InnerMessage::AuthRequest {
            operation: start,
            payload: json!({}),
          })
          .unwrap(),
      )
      .await
      .unwrap();
    let InnerMessage::AuthResponse { payload } = read_inner(&mut frames, &mut channel).await else {
      panic!("Expected host passkey challenge");
    };
    let origin = config.hub_url.clone();
    let credential = if register {
      serde_json::to_value(
        authenticator
          .do_registration(origin, serde_json::from_value(payload["options"].clone()).unwrap())
          .unwrap(),
      )
      .unwrap()
    } else {
      serde_json::to_value(
        authenticator
          .do_authentication(origin, serde_json::from_value(payload["options"].clone()).unwrap())
          .unwrap(),
      )
      .unwrap()
    };
    incoming
      .send(
        channel
          .encrypt(&InnerMessage::AuthRequest {
            operation: finish,
            payload: json!({"credential": credential}),
          })
          .unwrap(),
      )
      .await
      .unwrap();
    let InnerMessage::AuthResponse { payload } = read_inner(&mut frames, &mut channel).await else {
      panic!("Expected host passkey confirmation");
    };
    task.await.unwrap().unwrap();
    payload
  }

  #[tokio::test]
  async fn discovery_metadata_requires_independent_device_authorization_and_account_membership() {
    let directory = tempfile::tempdir().unwrap();
    let config = paired_host(directory.path());
    let profile_path = directory.path().join("host.json");
    let profile = crate::onboarding::HostProfile {
      version: 1,
      host_id: config.paired.host_id.clone(),
      hub_url: config.hub_url.to_string(),
      name: config.name.clone(),
      viewer_url: config.local_url.to_string(),
      allow_control: false,
      insecure_loopback: false,
      passkey_origin: Some(config.hub_url.to_string()),
      ice_servers: Vec::new(),
    };
    profile.save(&profile_path).unwrap();
    let device = NoiseIdentity::generate().unwrap();
    let request = |channel: &mut SecureChannel| channel.encrypt(&InnerMessage::DiscoveryRequest {}).unwrap();
    let (task, incoming, mut frames, mut channel) = open_host(&config, &device).await;
    incoming.send(request(&mut channel)).await.unwrap();
    assert!(matches!(
      read_inner(&mut frames, &mut channel).await,
      InnerMessage::Error { .. }
    ));
    assert!(task.await.unwrap().is_err());
    let current = now().unwrap();
    let state_file = &config.paired.state_file;
    crate::onboarding::authorize_device(state_file, &device.public_key(), current / 30, current).unwrap();
    let account_group = DiscoveryGroup {
      namespace: "alice".into(),
      secret_digest: "A".repeat(43),
    };
    let membership_path = directory.path().join("host-account.json");
    for group in [None, Some(account_group)] {
      if let Some(group) = &group {
        crate::onboarding::save_configuration(
          &membership_path,
          &json!({
            "hub_origin": profile.hub_url,
            "claim": { "host_id": profile.host_id, "machine_address": "alice:workstation", "group": group },
          }),
        )
        .unwrap();
      }
      let (task, incoming, mut frames, mut channel) = open_host(&config, &device).await;
      incoming.send(request(&mut channel)).await.unwrap();
      assert_eq!(
        read_inner(&mut frames, &mut channel).await,
        InnerMessage::DiscoveryResponse { group }
      );
      task.await.unwrap().unwrap();
    }
    crate::onboarding::remove_device(state_file, &device.public_key()).unwrap();
    let (task, incoming, mut frames, mut channel) = open_host(&config, &device).await;
    incoming.send(request(&mut channel)).await.unwrap();
    assert!(matches!(
      read_inner(&mut frames, &mut channel).await,
      InnerMessage::Error { .. }
    ));
    assert!(task.await.unwrap().is_err());
  }

  #[tokio::test]
  async fn paired_interactions_require_device_authorization_and_current_host_control() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = paired_host(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.local_url = format!("http://{}/", listener.local_addr().unwrap()).parse().unwrap();
    let forwarded = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let observed = forwarded.clone();
    let api = tokio::spawn(async move {
      let app = axum::Router::new().fallback(
        move |axum::extract::OriginalUri(uri): axum::extract::OriginalUri, axum::Json(payload): axum::Json<Value>| {
          let observed = observed.clone();
          async move {
            observed.lock().unwrap().push((uri.path().to_owned(), payload.clone()));
            axum::Json(if uri.path() == "/api/v1/list_session_interactions" {
              json!({
                "session_key":"one", "runtime_id":"private-runtime", "instance_id":"private-instance",
                "interactions":[{"interaction_id":"permission-one", "details":"private-operation"}]
              })
            } else {
              json!({"request_id":payload["request"]["request_id"], "status":"accepted", "message":"Response sent"})
            })
          }
        },
      );
      axum::serve(listener, app).await.unwrap();
    });
    let device = NoiseIdentity::generate().unwrap();
    for command in ["list_session_interactions", "respond_session_interaction"] {
      let (task, incoming, mut frames, mut channel) = open_host(&config, &device).await;
      incoming
        .send(
          channel
            .encrypt(
              &InnerMessage::DeviceRequest {
                method: "POST".into(),
                path: format!("/api/v1/{command}"),
              }
              .on_stream(1),
            )
            .unwrap(),
        )
        .await
        .unwrap();
      assert!(matches!(
        read_inner(&mut frames, &mut channel).await,
        InnerMessage::Error { .. }
      ));
      assert!(task.await.unwrap().is_err());
      assert!(forwarded.lock().unwrap().is_empty());
    }
    let current = now().unwrap();
    crate::onboarding::authorize_device(&config.paired.state_file, &device.public_key(), current / 30, current)
      .unwrap();
    // Agent control does not require the independent runtime-admin permission.
    assert!(!crate::onboarding::can_manage_runtimes(&config.paired.state_file, &device.public_key()).unwrap());
    for allow_control in [false, true] {
      config.allow_control = allow_control;
      for command in ["list_session_interactions", "respond_session_interaction"] {
        let path = format!("/api/v1/{command}");
        let payload = if command == "list_session_interactions" {
          json!({"request":{"session_key":"one"}})
        } else {
          json!({"request":{
            "session_key":"one", "request_id":"9a1b2c3d-1234-4321-9876-0123456789ab",
            "runtime_id":"private-runtime", "instance_id":"private-instance", "interaction_id":"permission-one",
            "response":{"type":"permission", "decision":"allow_once"}
          }})
        };
        let before = forwarded.lock().unwrap().len();
        let (task, incoming, mut frames, mut channel) = open_host(&config, &device).await;
        for message in [
          InnerMessage::DeviceRequest {
            method: "POST".into(),
            path: path.clone(),
          },
          InnerMessage::RequestBody {
            data: protocol::encode(&serde_json::to_vec(&payload).unwrap()),
          },
          InnerMessage::RequestEnd {},
        ] {
          incoming
            .send(channel.encrypt(&message.on_stream(1)).unwrap())
            .await
            .unwrap();
        }
        if !allow_control && command == "respond_session_interaction" {
          assert!(matches!(
            stream_frame(&mut frames, &mut channel, 1).await,
            InnerMessage::Error { .. }
          ));
          drop(incoming);
          task.await.unwrap().unwrap();
          assert_eq!(forwarded.lock().unwrap().len(), before);
          continue;
        }
        assert!(matches!(
          stream_frame(&mut frames, &mut channel, 1).await,
          InnerMessage::Response { status: 200, .. }
        ));
        let InnerMessage::Chunk { data } = stream_frame(&mut frames, &mut channel, 1).await else {
          panic!("Expected interaction response");
        };
        let response: Value = serde_json::from_slice(&protocol::decode(&data, protocol::CHUNK_SIZE).unwrap()).unwrap();
        if allow_control {
          assert_eq!(forwarded.lock().unwrap().last(), Some(&(path, payload)));
          assert_eq!(forwarded.lock().unwrap().len(), before + 1);
          if command == "list_session_interactions" {
            assert_eq!(response["runtime_id"], "private-runtime");
          } else {
            assert_eq!(response["status"], "accepted");
          }
        } else {
          assert_eq!(
            response,
            json!({"session_key":"one", "runtime_id":null, "instance_id":null, "interactions":[]})
          );
          assert_eq!(forwarded.lock().unwrap().len(), before);
        }
        assert!(matches!(
          stream_frame(&mut frames, &mut channel, 1).await,
          InnerMessage::End {}
        ));
        drop(incoming);
        task.await.unwrap().unwrap();
      }
    }
    api.abort();
  }

  #[tokio::test]
  async fn runtime_administration_requires_both_host_permissions_before_forwarding() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = paired_host(directory.path());
    let device = NoiseIdentity::generate().unwrap();
    let state_file = config.paired.state_file.clone();
    let current = now().unwrap();
    crate::onboarding::authorize_device(&state_file, &device.public_key(), current / 30, current).unwrap();
    for (control, management) in [(false, false), (true, false), (false, true)] {
      config.allow_control = control;
      crate::onboarding::set_runtime_management_enabled(&state_file, management).unwrap();
      let (task, incoming, mut frames, mut channel) = open_host(&config, &device).await;
      incoming
        .send(
          channel
            .encrypt(
              &InnerMessage::DeviceRequest {
                method: "POST".into(),
                path: "/api/v1/start_rpc_runtime".into(),
              }
              .on_stream(1),
            )
            .unwrap(),
        )
        .await
        .unwrap();
      incoming
        .send(channel.encrypt(&InnerMessage::RequestEnd {}.on_stream(1)).unwrap())
        .await
        .unwrap();
      let InnerMessage::Error { message } = stream_frame(&mut frames, &mut channel, 1).await else {
        panic!("Runtime request should fail before any local API request");
      };
      assert!(message.contains("Runtime") || message.contains("runtime"));
      drop(incoming);
      task.await.unwrap().unwrap();
    }
    config.allow_control = true;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.local_url = format!("http://{}/", listener.local_addr().unwrap()).parse().unwrap();
    let api = tokio::spawn(async move {
      let app = axum::Router::new().route(
        "/api/v1/start_rpc_runtime",
        axum::routing::post(|| async { axum::Json(json!({"connected":true})) }),
      );
      axum::serve(listener, app).await.unwrap();
    });
    let (task, incoming, mut frames, mut channel) = open_host(&config, &device).await;
    incoming
      .send(
        channel
          .encrypt(
            &InnerMessage::DeviceRequest {
              method: "POST".into(),
              path: "/api/v1/start_rpc_runtime".into(),
            }
            .on_stream(1),
          )
          .unwrap(),
      )
      .await
      .unwrap();
    incoming
      .send(channel.encrypt(&InnerMessage::RequestEnd {}.on_stream(1)).unwrap())
      .await
      .unwrap();
    assert!(matches!(
      stream_frame(&mut frames, &mut channel, 1).await,
      InnerMessage::Response { status: 200, .. }
    ));
    let InnerMessage::Chunk { data } = stream_frame(&mut frames, &mut channel, 1).await else {
      panic!("Expected runtime response");
    };
    assert_eq!(
      serde_json::from_slice::<Value>(&protocol::decode(&data, protocol::CHUNK_SIZE).unwrap()).unwrap(),
      json!({"connected":true})
    );
    assert!(matches!(
      stream_frame(&mut frames, &mut channel, 1).await,
      InnerMessage::End {}
    ));
    drop(incoming);
    task.await.unwrap().unwrap();
    api.abort();
    let host = Host::load(&config).unwrap();
    host
      .verify_runtime_management(&config, &device.public_key(), "POST", "/api/v1/start_rpc_runtime")
      .unwrap();
    crate::onboarding::set_runtime_management_enabled(&state_file, false).unwrap();
    assert!(
      host
        .verify_runtime_management(&config, &device.public_key(), "POST", "/api/v1/start_rpc_runtime")
        .is_err()
    );
  }

  #[tokio::test]
  async fn passkey_commands_are_constrained_before_authorization_and_remember_verified_device_keys() {
    let directory = tempfile::tempdir().unwrap();
    let config = paired_host(directory.path());
    let device = NoiseIdentity::generate().unwrap();
    for header in [
      InnerMessage::DeviceRequest {
        method: "GET".into(),
        path: "/api/v1/health".into(),
      },
      InnerMessage::DirectConfigRequest {},
      InnerMessage::DiscoveryRequest {},
      InnerMessage::DirectOffer { sdp: "v=0\r\n".into() },
      InnerMessage::AuthRequest {
        operation: HostAuthOperation::RegisterStart,
        payload: json!({}),
      },
    ] {
      let (task, incoming, mut frames, mut channel) = open_host(&config, &device).await;
      incoming.send(channel.encrypt(&header).unwrap()).await.unwrap();
      assert!(matches!(
        read_inner(&mut frames, &mut channel).await,
        InnerMessage::Error { .. }
      ));
      assert!(task.await.unwrap().is_err());
    }
    let state_file = &config.paired.state_file;
    let paired = NoiseIdentity::generate().unwrap();
    let now = now().unwrap();
    crate::onboarding::authorize_device(state_file, &paired.public_key(), now / 30, now).unwrap();
    let mut authenticator = WebauthnAuthenticator::new(SoftPasskey::new(true));
    assert_eq!(
      authenticate_wire(&config, &paired, &mut authenticator, true).await["registered"],
      true
    );
    assert!(!crate::onboarding::is_authorized(state_file, &device.public_key()).unwrap());
    let accepted = authenticate_wire(&config, &device, &mut authenticator, false).await;
    assert_eq!(accepted["device_public_key"], device.public_key());
    let reopened = Host::load(&config).unwrap();
    reopened.verify(&config.paired.host_id, &device.public_key()).unwrap();
    // Saved explicit origins and a URL-derived origin must have the same
    // canonical representation before checking the persisted credential RP.
    let mut profile = crate::onboarding::HostProfile {
      version: 1,
      host_id: config.paired.host_id.clone(),
      hub_url: config.hub_url.to_string(),
      name: config.name.clone(),
      viewer_url: config.local_url.to_string(),
      allow_control: config.allow_control,
      insecure_loopback: config.insecure_loopback,
      passkey_origin: Some(config.hub_url.to_string()),
      ice_servers: Vec::new(),
    };
    let profile_path = directory.path().join("host.json");
    profile.save(&profile_path).unwrap();
    assert!(Host::load(&config).is_ok());
    profile.passkey_origin = Some("https://other.example.com".into());
    profile.save(&profile_path).unwrap();
    assert!(Host::load(&config).is_err());
    crate::onboarding::remove_device(state_file, &device.public_key()).unwrap();
    assert!(reopened.verify(&config.paired.host_id, &device.public_key()).is_err());
  }

  #[tokio::test]
  async fn direct_channel_cannot_substitute_a_different_noise_device() {
    let directory = tempfile::tempdir().unwrap();
    let config = paired_host(directory.path());
    let host = Arc::new(Host::load(&config).unwrap());
    let expected = NoiseIdentity::generate().unwrap().public_key();
    let impostor = NoiseIdentity::generate().unwrap();
    let mut initiator = NoiseInitiator::new(&impostor, &host.public_key()).unwrap();
    let (incoming, received) = mpsc::channel(16);
    let (outgoing, mut sent) = mpsc::channel(16);
    let output = RecordOutput::Direct(outgoing);
    let task = tokio::spawn(async move {
      run_records(
        &host,
        &config.paired.host_id,
        &config,
        &reqwest::Client::new(),
        &output,
        received,
        Some(&expected),
        None,
      )
      .await
    });
    incoming.send(initiator.start().unwrap().into()).await.unwrap();
    let error = task.await.unwrap().unwrap_err();
    assert!(error.contains("signaling device"));
    assert!(
      sent.recv().await.is_none(),
      "Do not complete a substituted direct handshake"
    );
  }

  #[tokio::test]
  async fn authenticated_direct_peer_survives_signaling_close_and_revocation_stops_stream() {
    use axum::{
      Router,
      response::sse::{Event, Sse},
      routing::{get, post},
    };
    use futures_util::stream;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_url = format!("http://{}/", listener.local_addr().unwrap()).parse().unwrap();
    let app = Router::new()
      .route(
        "/api/v1/health",
        get(|| async { axum::Json(json!({"api_version": 1})) }),
      )
      .route(
        "/api/v1/list_sessions",
        post(|body: axum::body::Bytes| async move { axum::Json(json!({"length": body.len()})) }),
      )
      .route(
        "/api/v1/events",
        get(|| async {
          Sse::new(
            stream::once(async { Ok::<_, std::convert::Infallible>(Event::default().event("ready").data("{}")) })
              .chain(stream::pending()),
          )
        }),
      );
    let api = tokio::spawn(async move {
      axum::serve(listener, app).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = paired_host(directory.path());
    config.local_url = local_url;
    let device = NoiseIdentity::generate().unwrap();
    let state_file = config.paired.state_file.clone();
    let current = now().unwrap();
    crate::onboarding::authorize_device(&state_file, &device.public_key(), current / 30, current).unwrap();
    let host = Arc::new(Host::load(&config).unwrap());
    let host_id = config.paired.host_id.clone();
    let shutdown = CancellationToken::new();
    let manager = DirectManager::new(
      host.clone(),
      host_id,
      config.clone(),
      reqwest::Client::new(),
      shutdown.clone(),
    );
    let (peer, offer) = WebRtcPeer::offer(Vec::new()).await.unwrap();
    let (task, incoming, mut frames, mut signal) = open_host_with_direct(&config, &device, Some(manager)).await;
    stream_send(&incoming, &mut signal, 1, InnerMessage::DirectConfigRequest {}).await;
    assert!(matches!(
      stream_frame(&mut frames, &mut signal, 1).await,
      InnerMessage::DirectConfig { .. }
    ));
    stream_send(&incoming, &mut signal, 2, InnerMessage::DirectOffer { sdp: offer }).await;
    let InnerMessage::DirectAnswer { sdp } = stream_frame(&mut frames, &mut signal, 2).await else {
      panic!("Expected direct answer")
    };
    drop(incoming);
    task.await.unwrap().unwrap();
    drop(frames); // Simulates a Hub tunnel failure after negotiation.
    peer.accept_answer(&sdp).await.unwrap();
    // The direct pump must accept the complete supported upload size without
    // overflowing its small queue when the sender can deliver records quickly.
    async fn read_from(transport: &mut BoxTransport, channel: &mut SecureChannel, id: u32) -> InnerMessage {
      let InnerMessage::Stream { stream_id, frame } =
        channel.decrypt(&transport.receive().await.unwrap().unwrap()).unwrap()
      else {
        panic!("Expected direct stream response");
      };
      assert_eq!(stream_id, id);
      *frame
    }
    let mut transport = peer.open_record().await.unwrap();
    let mut initiator = NoiseInitiator::new(&device, &host.public_key()).unwrap();
    transport.send(initiator.start().unwrap()).await.unwrap();
    let mut channel = initiator.finish(&transport.receive().await.unwrap().unwrap()).unwrap();
    for frame in
      tokn_hub_client_core::exchange::RequestFrames::new("POST", "/api/v1/list_sessions", &vec![42; protocol::MAX_BODY])
        .unwrap()
    {
      transport
        .send(channel.encrypt(&frame.on_stream(1)).unwrap())
        .await
        .unwrap();
    }
    assert!(matches!(
      read_from(&mut transport, &mut channel, 1).await,
      InnerMessage::Response { status: 200, .. }
    ));
    let InnerMessage::Chunk { data } = read_from(&mut transport, &mut channel, 1).await else {
      panic!("Expected upload response");
    };
    let received: Value = serde_json::from_slice(&protocol::decode(&data, protocol::CHUNK_SIZE).unwrap()).unwrap();
    assert_eq!(received["length"], protocol::MAX_BODY);
    transport
      .send(
        channel
          .encrypt(&InnerMessage::Window { credits: 1 }.on_stream(1))
          .unwrap(),
      )
      .await
      .unwrap();
    assert!(matches!(
      read_from(&mut transport, &mut channel, 1).await,
      InnerMessage::End {}
    ));
    // Direct negotiation is relay-only, even for an already authorized direct session.
    transport
      .send(
        channel
          .encrypt(&InnerMessage::DirectConfigRequest {}.on_stream(2))
          .unwrap(),
      )
      .await
      .unwrap();
    assert!(matches!(
      read_from(&mut transport, &mut channel, 2).await,
      InnerMessage::Error { .. }
    ));
    for frame in tokn_hub_client_core::exchange::RequestFrames::new("GET", "/api/v1/events", &[]).unwrap() {
      transport
        .send(channel.encrypt(&frame.on_stream(3)).unwrap())
        .await
        .unwrap();
    }
    assert!(matches!(
      read_from(&mut transport, &mut channel, 3).await,
      InnerMessage::Response { status: 200, .. }
    ));
    let InnerMessage::Chunk { data } = read_from(&mut transport, &mut channel, 3).await else {
      panic!("Expected direct SSE");
    };
    assert!(
      String::from_utf8(protocol::decode(&data, protocol::CHUNK_SIZE).unwrap())
        .unwrap()
        .contains("event: ready")
    );
    transport
      .send(
        channel
          .encrypt(&InnerMessage::Window { credits: 1 }.on_stream(3))
          .unwrap(),
      )
      .await
      .unwrap();
    crate::onboarding::remove_device(&state_file, &device.public_key()).unwrap();
    let terminal = tokio::time::timeout(Duration::from_secs(5), transport.receive())
      .await
      .expect("Revocation must terminate an existing direct stream");
    match terminal {
      Ok(Some(record)) => assert!(matches!(channel.decrypt(&record).unwrap(), InnerMessage::Error { .. })),
      Ok(None) | Err(_) => {}
    }
    shutdown.cancel();
    peer.close().await;
    api.abort();
  }

  #[test]
  fn concurrent_pairings_with_one_code_authorize_only_one_device() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("access.json");
    let secret = crate::pairing::TotpSecret::generate();
    crate::onboarding::initialize_host_access(&path, &secret).unwrap();
    let now = now().unwrap();
    let step = now / 30;
    let first = NoiseIdentity::generate().unwrap().public_key();
    let second = NoiseIdentity::generate().unwrap().public_key();
    crate::onboarding::begin_pairing(&path, now, step).unwrap();
    crate::onboarding::begin_pairing(&path, now, step).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let (first_result, second_result) = std::thread::scope(|scope| {
      let a = scope.spawn(|| {
        barrier.wait();
        crate::onboarding::authorize_device(&path, &first, step, now)
      });
      let b = scope.spawn(|| {
        barrier.wait();
        crate::onboarding::authorize_device(&path, &second, step, now)
      });
      (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(first_result.is_ok(), second_result.is_ok());
    assert_ne!(
      crate::onboarding::is_authorized(&path, &first).unwrap(),
      crate::onboarding::is_authorized(&path, &second).unwrap()
    );
    assert!(crate::onboarding::begin_pairing(&path, now, step).is_err());
  }
}
