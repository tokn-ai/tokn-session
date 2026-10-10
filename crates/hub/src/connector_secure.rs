//! Host-owned trust boundary for encrypted requests. The Hub is only a carrier.
use super::{ConnectorConfig, PairedHostConfig, SecureHostConfig, enqueue};
use crate::{
  host_passkeys::{CEREMONY_SECONDS, HostPasskeys},
  protocol::{self, Frame},
  secure::{HostAuthOperation, InnerMessage, NoiseIdentity, NoiseResponder, SecureChannel, SignedGrant},
};
use futures_util::StreamExt;
use std::{
  collections::HashSet,
  fs::OpenOptions,
  io::Read,
  path::Path,
  sync::Mutex,
  time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Semaphore, mpsc};

pub(super) struct Host {
  identity: NoiseIdentity,
  trust: HostTrust,
  passkeys: Option<HostPasskeys>,
}

enum HostTrust {
  SignedGrants(SecureHostConfig),
  PairedDevices(PairedHostConfig),
}

impl Host {
  pub(super) fn load(config: &ConnectorConfig) -> Result<Option<Self>, String> {
    let host = if let Some(paired) = &config.paired {
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
      Self {
        identity: NoiseIdentity::load_or_create(&paired.noise_key_file)?,
        trust: HostTrust::PairedDevices(paired.clone()),
        passkeys,
      }
    } else if let Some(config) = &config.secure {
      let bytes: [u8; 32] = protocol::decode(&config.owner_public_key, 32)?
        .try_into()
        .map_err(|_| "Invalid owner public key")?;
      ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| "Invalid owner public key")?;
      Self {
        identity: NoiseIdentity::load_or_create(&config.noise_key_file)?,
        trust: HostTrust::SignedGrants(config.clone()),
        passkeys: None,
      }
    } else {
      return Ok(None);
    };
    host.revocations()?;
    Ok(Some(host))
  }

  pub(super) fn public_key(&self) -> String {
    self.identity.public_key()
  }

  fn revocations(&self) -> Result<HashSet<String>, String> {
    let HostTrust::SignedGrants(config) = &self.trust else {
      return Ok(HashSet::new());
    };
    config
      .revocations_file
      .as_deref()
      .map(read_revocations)
      .transpose()
      .map(|value| value.unwrap_or_default())
  }

  fn verify(&self, grant: Option<&SignedGrant>, host_id: &str, recipient: &str) -> Result<(), String> {
    match (&self.trust, grant) {
      (HostTrust::SignedGrants(config), Some(grant)) => grant.verify(
        &config.owner_public_key,
        host_id,
        &self.public_key(),
        recipient,
        now()?,
        &self.revocations()?,
      ),
      (HostTrust::PairedDevices(config), None) if config.host_id == host_id => {
        if crate::onboarding::is_authorized(&config.state_file, recipient)? {
          Ok(())
        } else {
          Err("Device is not paired with this host".into())
        }
      }
      _ => Err("This host does not accept that authorization mode".into()),
    }
  }

  async fn pair(
    &self,
    host_id: &str,
    first: &[u8],
    incoming: &mut mpsc::Receiver<Vec<u8>>,
    outgoing: &mpsc::Sender<Frame>,
    channel_id: u64,
  ) -> Result<(), String> {
    let HostTrust::PairedDevices(config) = &self.trust else {
      return Err("Authenticator pairing is unavailable".into());
    };
    if config.host_id != host_id {
      return Err("Incorrect pairing target".into());
    }
    let step = crate::pairing::peek_step(first)?;
    let secret = crate::onboarding::read_totp_secret(&config.state_file)?;
    crate::onboarding::begin_pairing(&config.state_file, now()?, step)?;
    let (pending, reply) = crate::pairing::HostPairing::respond(&secret, host_id, &self.identity, first, now()?)?;
    send_record(outgoing, channel_id, reply).await?;
    let (authenticated, ack) = pending.finish(&receive(incoming).await?, now()?)?;
    // Persist consumption and authorization together before letting the client
    // save its pin. Concurrent completions with the same TOTP step lose here.
    crate::onboarding::authorize_device(
      &config.state_file,
      &authenticated.client_public_key,
      authenticated.step,
      now()?,
    )?;
    send_record(outgoing, channel_id, ack).await
  }

  async fn authenticate(
    &self,
    host_id: &str,
    operation: HostAuthOperation,
    payload: serde_json::Value,
    channel: &mut SecureChannel,
    incoming: &mut mpsc::Receiver<Vec<u8>>,
    outgoing: &mpsc::Sender<Frame>,
    channel_id: u64,
  ) -> Result<(), String> {
    let HostTrust::PairedDevices(config) = &self.trust else {
      return Err("This host does not support passkey device authorization".into());
    };
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
    send_record(
      outgoing,
      channel_id,
      channel.encrypt(&InnerMessage::AuthResponse { payload })?,
    )
    .await?;
    let finish = tokio::time::timeout(Duration::from_secs(CEREMONY_SECONDS), incoming.recv())
      .await
      .map_err(|_| "Host passkey ceremony expired; start again")?
      .ok_or("Host passkey channel closed")?;
    let InnerMessage::AuthRequest { operation, payload } = channel.decrypt(&finish)? else {
      return Err("Finish the host passkey ceremony on its original encrypted channel".into());
    };
    let payload = passkeys.finish(pending, operation, payload, &peer, &binding, now()?)?;
    send_record(
      outgoing,
      channel_id,
      channel.encrypt(&InnerMessage::AuthResponse { payload })?,
    )
    .await
  }
}

fn now() -> Result<u64, String> {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|value| value.as_secs())
    .map_err(|_| "Invalid host clock".into())
}

async fn send_record(outgoing: &mpsc::Sender<Frame>, channel_id: u64, record: Vec<u8>) -> Result<(), String> {
  enqueue(
    outgoing,
    Frame::SecureData {
      channel_id,
      data: protocol::encode(&record),
    },
  )
  .await
}

fn read_revocations(path: &Path) -> Result<HashSet<String>, String> {
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  #[cfg(not(unix))]
  if std::fs::symlink_metadata(path)
    .map_err(|_| "Cannot inspect grant revocations")?
    .file_type()
    .is_symlink()
  {
    return Err("Grant revocations must not be a symlink".into());
  }
  let file = options
    .open(path)
    .map_err(|_| "Cannot read configured grant revocations")?;
  let metadata = file.metadata().map_err(|_| "Cannot inspect grant revocations")?;
  if !metadata.is_file() || metadata.len() > 1024 * 1024 {
    return Err("Grant revocations must be a regular file of at most 1 MiB".into());
  }
  let mut bytes = Vec::new();
  file
    .take(1024 * 1024 + 1)
    .read_to_end(&mut bytes)
    .map_err(|_| "Cannot read grant revocations")?;
  if bytes.len() > 1024 * 1024 {
    return Err("Grant revocations exceed 1 MiB".into());
  }
  let ids: Vec<String> = serde_json::from_slice(&bytes).map_err(|_| "Invalid grant revocations JSON")?;
  if ids
    .iter()
    .any(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
  {
    return Err("Invalid revoked grant identifier".into());
  }
  Ok(ids.into_iter().collect())
}

async fn receive(incoming: &mut mpsc::Receiver<Vec<u8>>) -> Result<Vec<u8>, String> {
  tokio::time::timeout(Duration::from_secs(10), incoming.recv())
    .await
    .map_err(|_| "Encrypted request timed out")?
    .ok_or_else(|| "Encrypted channel closed".into())
}

pub(super) async fn run(
  host: &Host,
  host_id: &str,
  config: &ConnectorConfig,
  client: &reqwest::Client,
  outgoing: &mpsc::Sender<Frame>,
  channel_id: u64,
  mut incoming: mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
  let first = receive(&mut incoming).await?;
  if crate::pairing::is_pairing_record(&first) {
    return host.pair(host_id, &first, &mut incoming, outgoing, channel_id).await;
  }
  let (reply, mut channel) = NoiseResponder::new(&host.identity)?.accept(&first)?;
  enqueue(
    outgoing,
    Frame::SecureData {
      channel_id,
      data: protocol::encode(&reply),
    },
  )
  .await?;
  let header = channel.decrypt(&receive(&mut incoming).await?)?;
  if let InnerMessage::AuthRequest { operation, payload } = header {
    let result = host
      .authenticate(
        host_id,
        operation,
        payload,
        &mut channel,
        &mut incoming,
        outgoing,
        channel_id,
      )
      .await;
    if let Err(message) = &result {
      if let Ok(record) = channel.encrypt(&InnerMessage::Error {
        message: message.clone(),
      }) {
        let _ = send_record(outgoing, channel_id, record).await;
      }
    }
    return result;
  }
  let (method, path, grant) = match header {
    InnerMessage::Request { method, path, grant } => (method, path, Some(grant)),
    InnerMessage::DeviceRequest { method, path } => (method, path, None),
    _ => return Err("Expected encrypted request".into()),
  };
  let recipient = channel.remote_public_key().to_owned();
  if let Err(message) = host.verify(grant.as_ref(), host_id, &recipient) {
    if let Ok(record) = channel.encrypt(&InnerMessage::Error {
      message: message.clone(),
    }) {
      let _ = send_record(outgoing, channel_id, record).await;
    }
    return Err(message);
  }
  let body = tokio::time::timeout(Duration::from_secs(10), async {
    let mut body = Vec::new();
    let mut records = 0;
    loop {
      match channel.decrypt(&receive(&mut incoming).await?)? {
        InnerMessage::RequestBody { data } => {
          records += 1;
          if records > protocol::MAX_BODY.div_ceil(protocol::CHUNK_SIZE) {
            return Err("Too many request body records".into());
          }
          let bytes = protocol::decode(&data, protocol::CHUNK_SIZE)?;
          if body.len() + bytes.len() > protocol::MAX_BODY {
            return Err("Encrypted request body is too large".into());
          }
          body.extend_from_slice(&bytes);
        }
        InnerMessage::RequestEnd {} => return Ok::<_, String>(body),
        _ => return Err("Expected encrypted request body".into()),
      }
    }
  })
  .await
  .map_err(|_| "Encrypted request body timed out")??;
  host.verify(grant.as_ref(), host_id, &recipient)?;
  let channel = Mutex::new(channel);
  let window = Semaphore::new(protocol::RESPONSE_WINDOW);
  let result = {
    let response = forward(
      config,
      client,
      grant.as_ref(),
      &method,
      &path,
      body,
      outgoing,
      channel_id,
      &channel,
      &window,
    );
    let timeout = tokio::time::sleep(Duration::from_secs(120));
    tokio::pin!(response, timeout);
    let mut policy_check = tokio::time::interval(Duration::from_secs(1));
    loop {
      tokio::select! {
        biased;
        _ = policy_check.tick() => {
          if let Err(error) = host.verify(grant.as_ref(), host_id, &recipient) { break Err(error); }
        }
        _ = &mut timeout, if path != "/api/v1/events" => break Err("Encrypted request timed out; delivery may be uncertain and is never retried".into()),
        record = incoming.recv() => {
          let Some(record) = record else { break Err("Encrypted channel closed".into()) };
          let message = channel.lock().unwrap().decrypt(&record);
          match message {
            Ok(InnerMessage::Window { credits }) if credits > 0 && credits <= protocol::RESPONSE_WINDOW && window.available_permits() + credits <= protocol::RESPONSE_WINDOW => window.add_permits(credits),
            _ => break Err("Invalid encrypted response window".into()),
          }
        }
        result = &mut response => break result,
      }
    }
  };
  if let Err(message) = &result {
    let _ = send_inner(
      outgoing,
      channel_id,
      &channel,
      &InnerMessage::Error {
        message: message.clone(),
      },
    )
    .await;
  } else {
    // End may be queued behind the final chunk at the recipient. Keep the
    // receive side alive while it acknowledges that chunk and reads End, rather
    // than racing its final credit with an abrupt relay socket close.
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
      while let Some(record) = incoming.recv().await {
        match channel.lock().unwrap().decrypt(&record) {
          Ok(InnerMessage::Window { credits })
            if credits > 0
              && credits <= protocol::RESPONSE_WINDOW
              && window.available_permits() + credits <= protocol::RESPONSE_WINDOW =>
          {
            window.add_permits(credits)
          }
          _ => break,
        }
      }
    })
    .await;
  }
  result
}

async fn send_inner(
  outgoing: &mpsc::Sender<Frame>,
  channel_id: u64,
  channel: &Mutex<SecureChannel>,
  message: &InnerMessage,
) -> Result<(), String> {
  let bytes = channel.lock().unwrap().encrypt(message)?;
  enqueue(
    outgoing,
    Frame::SecureData {
      channel_id,
      data: protocol::encode(&bytes),
    },
  )
  .await
}

#[allow(clippy::too_many_arguments)]
async fn forward(
  config: &ConnectorConfig,
  client: &reqwest::Client,
  grant: Option<&SignedGrant>,
  method: &str,
  path: &str,
  body: Vec<u8>,
  outgoing: &mpsc::Sender<Frame>,
  channel_id: u64,
  channel: &Mutex<SecureChannel>,
  window: &Semaphore,
) -> Result<(), String> {
  let response = if let Some(grant) = grant {
    crate::secure_scope::forward(config, client, &grant.grant, method, path, body).await?
  } else {
    forward_device(config, client, method, path, body).await?
  };
  if response.status().is_redirection() {
    return Err("Local API redirects are forbidden".into());
  }
  send_inner(
    outgoing,
    channel_id,
    channel,
    &InnerMessage::Response {
      status: response.status().as_u16(),
      content_type: Some(
        if path == "/api/v1/events" {
          "text/event-stream"
        } else {
          "application/json"
        }
        .into(),
      ),
    },
  )
  .await?;
  let mut stream = response.bytes_stream();
  let mut total = 0usize;
  while let Some(chunk) = stream.next().await {
    let chunk = chunk.map_err(|_| "Local API response stream failed")?;
    total = total.saturating_add(chunk.len());
    if path != "/api/v1/events" && total > 128 * 1024 * 1024 {
      return Err("Local API response exceeds 128 MiB".into());
    }
    for part in chunk.chunks(protocol::CHUNK_SIZE) {
      // Only the authenticated recipient can return these encrypted credits.
      // The Hub cannot manufacture acknowledgements to make us spool plaintext.
      window
        .acquire()
        .await
        .map_err(|_| "Encrypted response cancelled")?
        .forget();
      send_inner(
        outgoing,
        channel_id,
        channel,
        &InnerMessage::Chunk {
          data: protocol::encode(part),
        },
      )
      .await?;
    }
  }
  send_inner(outgoing, channel_id, channel, &InnerMessage::End {}).await
}

async fn forward_device(
  config: &ConnectorConfig,
  client: &reqwest::Client,
  method: &str,
  path: &str,
  body: Vec<u8>,
) -> Result<reqwest::Response, String> {
  if !protocol::allowed_route(method, path, config.allow_control) {
    return Err("Route unavailable or host control is disabled".into());
  }
  if path == "/api/v1/get_session_input_status" && !config.allow_control {
    return Ok(reqwest::Response::from(
      axum::http::Response::builder()
        .header("content-type", "application/json")
        .body(
          serde_json::to_vec(&serde_json::json!({
            "available": false, "message": "Agent input is disabled on this host", "max_length": 0
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
  use serde_json::{Value, json};
  use std::{fs, sync::Arc};
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
      secure: None,
      paired: Some(PairedHostConfig {
        host_id: "11111111-1111-4111-8111-111111111111".into(),
        noise_key_file: directory.join("host-noise.key"),
        state_file,
      }),
    }
  }

  async fn open_host(
    config: &ConnectorConfig,
    device: &NoiseIdentity,
  ) -> (
    tokio::task::JoinHandle<Result<(), String>>,
    mpsc::Sender<Vec<u8>>,
    mpsc::Receiver<Frame>,
    SecureChannel,
  ) {
    let host = Arc::new(Host::load(config).unwrap().unwrap());
    let mut initiator = NoiseInitiator::new(device, &host.public_key()).unwrap();
    let config = config.clone();
    let host_id = config.paired.as_ref().unwrap().host_id.clone();
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
      )
      .await
    });
    incoming.send(initiator.start().unwrap()).await.unwrap();
    let Frame::SecureData { data, .. } = frames.recv().await.unwrap() else {
      panic!("Expected Noise handshake response");
    };
    let channel = initiator
      .finish(&protocol::decode(&data, protocol::MAX_SECURE_RECORD).unwrap())
      .unwrap();
    (task, incoming, frames, channel)
  }

  async fn read_inner(frames: &mut mpsc::Receiver<Frame>, channel: &mut SecureChannel) -> InnerMessage {
    let Frame::SecureData { data, .. } = frames.recv().await.unwrap() else {
      panic!("Expected encrypted response");
    };
    channel
      .decrypt(&protocol::decode(&data, protocol::MAX_SECURE_RECORD).unwrap())
      .unwrap()
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
  async fn passkey_commands_are_constrained_before_authorization_and_remember_verified_device_keys() {
    let directory = tempfile::tempdir().unwrap();
    let config = paired_host(directory.path());
    let device = NoiseIdentity::generate().unwrap();
    for header in [
      InnerMessage::DeviceRequest {
        method: "GET".into(),
        path: "/api/v1/health".into(),
      },
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
    let state_file = &config.paired.as_ref().unwrap().state_file;
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
    let reopened = Host::load(&config).unwrap().unwrap();
    reopened
      .verify(None, &config.paired.as_ref().unwrap().host_id, &device.public_key())
      .unwrap();
    // Saved explicit origins and a URL-derived origin must have the same
    // canonical representation before checking the persisted credential RP.
    let mut profile = crate::onboarding::HostProfile {
      version: 1,
      host_id: config.paired.as_ref().unwrap().host_id.clone(),
      hub_url: config.hub_url.to_string(),
      name: config.name.clone(),
      viewer_url: config.local_url.to_string(),
      allow_control: config.allow_control,
      insecure_loopback: config.insecure_loopback,
      passkey_origin: Some(config.hub_url.to_string()),
    };
    let profile_path = directory.path().join("host.json");
    profile.save(&profile_path).unwrap();
    assert!(Host::load(&config).is_ok());
    profile.passkey_origin = Some("https://other.example.com".into());
    profile.save(&profile_path).unwrap();
    assert!(Host::load(&config).is_err());
    crate::onboarding::remove_device(state_file, &device.public_key()).unwrap();
    assert!(
      reopened
        .verify(None, &config.paired.as_ref().unwrap().host_id, &device.public_key())
        .is_err()
    );
  }

  #[test]
  fn revocations_are_reloaded_and_invalid_files_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("revoked.json");
    assert!(read_revocations(&file).is_err());
    fs::write(&file, "[]").unwrap();
    assert!(read_revocations(&file).unwrap().is_empty());
    fs::write(&file, r#"["grant_1"]"#).unwrap();
    assert!(read_revocations(&file).unwrap().contains("grant_1"));
    fs::write(&file, "{}").unwrap();
    assert!(read_revocations(&file).is_err());
    #[cfg(unix)]
    {
      let link = directory.path().join("link.json");
      std::os::unix::fs::symlink(&file, &link).unwrap();
      assert!(read_revocations(&link).is_err());
      let fifo = directory.path().join("fifo");
      let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
      assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
      assert!(
        read_revocations(&fifo).is_err(),
        "a FIFO must fail without waiting for a writer"
      );
    }
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
