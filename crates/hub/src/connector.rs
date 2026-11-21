//! Outbound connector: authenticated tunnel to a Hub, fixed loopback viewer target.
use crate::protocol::{self, Frame};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use rand::{Rng, rngs::OsRng};
use serde::{Deserialize, Serialize};
use std::{
  collections::HashMap,
  fs::{self, OpenOptions},
  io::{Read, Write},
  net::IpAddr,
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};
use tokio::{net::TcpStream, sync::mpsc, task::JoinSet};
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream, connect_async_with_config,
  tungstenite::{Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::presence::{CONNECTOR_VERSION_HEADER, HUB_VERSION_HEADER, valid_software_version};
use url::{Host, Url};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[path = "connector_secure.rs"]
mod secure_transport;

#[derive(Clone)]
pub struct PairedHostConfig {
  pub host_id: String,
  pub noise_key_file: PathBuf,
  pub state_file: PathBuf,
}

#[derive(Clone)]
pub struct ConnectorConfig {
  pub hub_url: Url,
  pub local_url: Url,
  pub key_file: PathBuf,
  pub name: String,
  pub local_token: Option<String>,
  pub allow_control: bool,
  pub insecure_loopback: bool,
  /// Explicit STUN discovery servers; relay fallback needs no third-party service.
  pub ice_servers: Vec<String>,
  pub paired: PairedHostConfig,
}

impl ConnectorConfig {
  pub fn validate(&self) -> Result<Url, String> {
    crate::secure::validate_ice_servers(&self.ice_servers)?;
    if !protocol::valid_host_uuid(&self.paired.host_id) {
      return Err("Paired host identity must be a canonical random UUID".into());
    }
    if self.name.trim().is_empty() || self.name.len() > 128 || self.name.chars().any(char::is_control) {
      return Err("Host name must contain 1–128 bytes and no control characters".into());
    }
    for url in [&self.hub_url, &self.local_url] {
      if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
      {
        return Err("Hub and local API URLs must be origins without credentials, paths, queries, or fragments".into());
      }
    }
    if self.local_url.scheme() != "http" || !numeric_loopback(&self.local_url) {
      return Err("Local API must be an HTTP URL with a numeric loopback address".into());
    }
    let mut endpoint = self.hub_url.clone();
    match endpoint.scheme() {
      "https" | "wss" => {
        endpoint.set_scheme("wss").map_err(|_| "Invalid Hub URL")?;
      }
      "http" | "ws"
        if self.insecure_loopback && (numeric_loopback(&endpoint) || endpoint.host_str() == Some("localhost")) =>
      {
        endpoint.set_scheme("ws").map_err(|_| "Invalid Hub URL")?;
      }
      _ => {
        return Err(
          "Hub requires HTTPS/WSS; insecure transport is allowed only for explicit loopback development".into(),
        );
      }
    }
    endpoint.set_path(protocol::TUNNEL_PATH);
    Ok(endpoint)
  }
}

fn numeric_loopback(url: &Url) -> bool {
  matches!(url.host(), Some(Host::Ipv4(ip)) if IpAddr::V4(ip).is_loopback())
    || matches!(url.host(), Some(Host::Ipv6(ip)) if IpAddr::V6(ip).is_loopback())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
  version: u32,
  secret_key: String,
}

pub(crate) fn load_key(path: &Path) -> Result<SigningKey, String> {
  if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
    fs::create_dir_all(parent).map_err(|e| format!("Could not create host key directory: {e}"))?;
  }
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  match options.open(path) {
    Ok(mut file) => {
      let key = SigningKey::generate(&mut OsRng);
      let data = serde_json::to_vec(&KeyFile {
        version: protocol::VERSION,
        secret_key: protocol::encode(&key.to_bytes()),
      })
      .map_err(|e| e.to_string())?;
      file
        .write_all(&data)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("Could not save host key: {e}"))?;
      Ok(key)
    }
    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
      let mut options = OpenOptions::new();
      options.read(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
      }
      #[cfg(not(unix))]
      if fs::symlink_metadata(path)
        .map_err(|e| format!("Could not inspect host key: {e}"))?
        .file_type()
        .is_symlink()
      {
        return Err("Host key must not be a symlink".into());
      }
      let file = options
        .open(path)
        .map_err(|e| format!("Could not open host key: {e}"))?;
      let metadata = file
        .metadata()
        .map_err(|e| format!("Could not inspect host key: {e}"))?;
      if !metadata.is_file() || metadata.len() > 4096 {
        return Err("Host key must be a small regular file, not a symlink".into());
      }
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
          return Err("Host key permissions must deny access to group and other users (chmod 600)".into());
        }
      }
      let mut data = Vec::new();
      file.take(4097).read_to_end(&mut data).map_err(|e| e.to_string())?;
      let stored: KeyFile = serde_json::from_slice(&data).map_err(|_| "Invalid host key file")?;
      decode_key(stored)
    }
    Err(error) => Err(format!("Could not create host key: {error}")),
  }
}

fn decode_key(stored: KeyFile) -> Result<SigningKey, String> {
  if stored.version != protocol::VERSION {
    return Err("Unsupported host key file version".into());
  }
  let bytes: [u8; 32] = protocol::decode(&stored.secret_key, 32)?
    .try_into()
    .map_err(|_| "Invalid host private key")?;
  Ok(SigningKey::from_bytes(&bytes))
}

/// Validate an enrollment/signing identity without creating a missing key.
pub fn validate_existing_identity(path: &Path) -> Result<(), String> {
  let stored = tokn_session_storage::read_private_json::<KeyFile>(path, 4096)?
    .ok_or_else(|| format!("Saved host identity is missing: {}", path.display()))?;
  decode_key(stored).map(|_| ())
}

pub fn initialize_identity(path: &Path) -> Result<(), String> {
  load_key(path).map(|_| ())
}

/// Holds exclusive ownership of a saved enrollment key during registration or hosting.
pub struct HostLease {
  key_file: PathBuf,
  _lock: std::fs::File,
}
impl HostLease {
  pub fn is_held(key_file: &Path) -> Result<bool, String> {
    let mut path = key_file.as_os_str().to_owned();
    path.push(".connector");
    crate::onboarding::lock_held(Path::new(&path))
  }
  pub fn acquire(key_file: &Path) -> Result<Self, String> {
    let mut path = key_file.as_os_str().to_owned();
    path.push(".connector");
    let lock = crate::onboarding::lock(Path::new(&path)).map_err(|error| {
      format!("Could not acquire host ownership: {error}. Stop hosting on this computer before retrying.")
    })?;
    Ok(Self {
      key_file: key_file.to_owned(),
      _lock: lock,
    })
  }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ConnectorStatus {
  Connecting,
  Online,
  Reconnecting { error: String },
}
pub type StatusSink = Arc<dyn Fn(ConnectorStatus) + Send + Sync>;
fn notify(sink: &Option<StatusSink>, status: ConnectorStatus) {
  if let Some(sink) = sink {
    sink(status);
  }
}

pub async fn run(config: ConnectorConfig, shutdown: CancellationToken) -> Result<(), String> {
  let lease = HostLease::acquire(&config.key_file)?;
  run_owned(config, shutdown, lease, None).await
}

/// Setup callers acquire ownership before changing saved configuration.
pub async fn run_owned(
  config: ConnectorConfig,
  shutdown: CancellationToken,
  lease: HostLease,
  status: Option<StatusSink>,
) -> Result<(), String> {
  if lease.key_file != config.key_file {
    return Err("Host ownership does not match the connector identity".into());
  }
  let _lease = lease;
  let endpoint = config.validate()?;
  let key = load_key(&config.key_file)?;
  let secure = Arc::new(secure_transport::Host::load(&config)?);
  let client = reqwest::Client::builder()
    .no_proxy()
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .connect_timeout(Duration::from_secs(5))
    .read_timeout(Duration::from_secs(90))
    .build()
    .map_err(|e| e.to_string())?;
  let direct = secure_transport::DirectManager::new(
    secure.clone(),
    config.paired.host_id.clone(),
    config.clone(),
    client.clone(),
    shutdown.clone(),
  );
  eprintln!("Host identity: {}", config.paired.host_id);
  eprintln!("Host encryption public key: {}", secure.public_key());
  let mut backoff = 1u64;
  loop {
    notify(&status, ConnectorStatus::Connecting);
    let started = Instant::now();
    let result = tokio::select! {
      _ = shutdown.cancelled() => return Ok(()),
      result = connect_once(&config, &endpoint, &key, &client, secure.clone(), direct.clone(), &status) => result,
    };
    if let Err(error) = result {
      notify(&status, ConnectorStatus::Reconnecting { error: error.clone() });
      eprintln!("Hub connection ended: {error}");
    }
    if started.elapsed() > Duration::from_secs(60) {
      backoff = 1;
    }
    let jitter = rand::thread_rng().gen_range(0..=500);
    tokio::select! {
      _ = shutdown.cancelled() => return Ok(()),
      _ = tokio::time::sleep(Duration::from_millis(backoff * 1000 + jitter)) => {},
    }
    backoff = (backoff * 2).min(30);
  }
}

async fn connect_once(
  config: &ConnectorConfig,
  endpoint: &Url,
  key: &SigningKey,
  client: &reqwest::Client,
  secure: Arc<secure_transport::Host>,
  direct: Arc<secure_transport::DirectManager>,
  status: &Option<StatusSink>,
) -> Result<(), String> {
  let ws_config = WebSocketConfig::default()
    .max_message_size(Some(protocol::MAX_FRAME))
    .max_frame_size(Some(protocol::MAX_FRAME));
  let mut request = endpoint
    .as_str()
    .into_client_request()
    .map_err(|e| format!("Could not build Hub request: {e}"))?;
  request.headers_mut().insert(
    CONNECTOR_VERSION_HEADER,
    HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
  );
  let (mut socket, response) = tokio::time::timeout(
    Duration::from_secs(15),
    connect_async_with_config(request, Some(ws_config), false),
  )
  .await
  .map_err(|_| "Hub connection timed out")?
  .map_err(|e| format!("Could not connect to Hub: {e}"))?;
  let hub_version = response
    .headers()
    .get(HUB_VERSION_HEADER)
    .and_then(|value| value.to_str().ok())
    .filter(|version| valid_software_version(version));
  tokio::time::timeout(Duration::from_secs(10), authenticate(&mut socket, config, key))
    .await
    .map_err(|_| "Hub authentication timed out")??;
  let (outgoing, mut outgoing_rx) = mpsc::channel::<Frame>(64);
  let mut secure_channels: HashMap<u64, (tokio::task::AbortHandle, mpsc::Sender<Vec<u8>>)> = HashMap::new();
  let mut tasks = JoinSet::new();
  let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
  let mut last_received = Instant::now();
  let mut ready = false;
  loop {
    tokio::select! {
      incoming = socket.next() => {
        let Some(incoming) = incoming else { return Err("Hub disconnected".into()) };
        let incoming = incoming.map_err(|_| "Hub tunnel failed")?;
        last_received = Instant::now();
        match incoming {
          Message::Text(text) if text.len() <= protocol::MAX_FRAME => {
            let frame: Frame = serde_json::from_str(&text).map_err(|_| "Invalid Hub tunnel message")?;
            match frame {
              Frame::RegistrationRejected { message } if !ready => {
                return Err(registration_failure(&message));
              }
              Frame::Ready { host_id, allow_control } if !ready => {
                if host_id != config.paired.host_id { return Err("Hub returned an incorrect host identity".into()); }
                ready = true;
                notify(status, ConnectorStatus::Online);
                enqueue(&outgoing, Frame::SecureOnly {}).await?;
                eprintln!("Connected to Hub as {host_id} ({})", if config.allow_control && allow_control { "control enabled" } else { "view access" });
                if let Some(version) = hub_version {
                  eprintln!("Hub version: {version}");
                }
              }
              Frame::SecureOpen { channel_id } if ready => {
                if secure_channels.contains_key(&channel_id) { return Err("Hub reused an active secure channel".into()); }
                if secure_channels.len() >= protocol::MAX_REQUESTS {
                  enqueue(&outgoing, Frame::SecureClose { channel_id }).await?;
                  continue;
                }
                let secure = secure.clone();
                let (incoming, received) = mpsc::channel(protocol::MAX_BODY / protocol::CHUNK_SIZE + 4);
                let outgoing = outgoing.clone();
                let config = config.clone();
                let client = client.clone();
                let host_id = config.paired.host_id.clone();
                let direct = direct.clone();
                let abort = tasks.spawn(async move {
                  let _ = secure_transport::run(&secure, &host_id, &config, &client, &outgoing, channel_id, received, Some(&direct)).await;
                  let _ = enqueue(&outgoing, Frame::SecureClose { channel_id }).await;
                  channel_id
                });
                secure_channels.insert(channel_id, (abort, incoming));
              }
              Frame::SecureData { channel_id, data } if ready => {
                let record = protocol::decode(&data, protocol::MAX_SECURE_RECORD)?;
                if let Some((_, incoming)) = secure_channels.get(&channel_id) {
                  if record.is_empty() || incoming.try_send(record).is_err() {
                    if let Some((task, _)) = secure_channels.remove(&channel_id) { task.abort(); }
                    enqueue(&outgoing, Frame::SecureClose { channel_id }).await?;
                  }
                }
              }
              Frame::SecureClose { channel_id } if ready => {
                if let Some((task, _)) = secure_channels.remove(&channel_id) { task.abort(); }
              }
              _ => return Err("Unexpected Hub tunnel message".into()),
            }
          }
          Message::Ping(data) => send_message(&mut socket, Message::Pong(data)).await?,
          Message::Pong(_) => {},
          _ => return Err("Hub closed the connection".into()),
        }
      }
      frame = outgoing_rx.recv() => {
        let Some(frame) = frame else { return Err("Tunnel output closed".into()) };
        send(&mut socket, &frame).await?;
      }
      task = tasks.join_next(), if !tasks.is_empty() => {
        if let Some(Ok(id)) = task {
          secure_channels.remove(&id);
        }
      }
      _ = heartbeat.tick() => {
        if last_received.elapsed() > Duration::from_secs(45) { return Err("Hub heartbeat timed out".into()); }
        send_message(&mut socket, Message::Ping(Vec::new().into())).await?;
      }
    }
  }
}

fn registration_failure(message: &str) -> String {
  if message.trim().is_empty() || message.len() > 256 || message.chars().any(char::is_control) {
    return "Invalid Hub registration diagnostic".into();
  }
  format!("Hub rejected registration: {message}")
}

async fn authenticate(socket: &mut Socket, config: &ConnectorConfig, key: &SigningKey) -> Result<(), String> {
  let Some(Ok(Message::Text(text))) = socket.next().await else {
    return Err("Missing Hub challenge".into());
  };
  if text.len() > 1024 {
    return Err("Hub challenge is too large".into());
  }
  let Frame::Challenge { version, nonce } = serde_json::from_str(&text).map_err(|_| "Invalid Hub challenge")? else {
    return Err("Missing Hub challenge".into());
  };
  if version != protocol::VERSION || protocol::decode(&nonce, 32)?.len() != 32 {
    return Err("Unsupported Hub protocol or invalid challenge".into());
  }
  let public_key = protocol::encode(key.verifying_key().as_bytes());
  let paired = &config.paired;
  let discovery_group = secure_transport::discovery_group(config)?;
  let signature = key.sign(&protocol::discovery_registration_proof(
    &nonce,
    &paired.host_id,
    &public_key,
    &config.name,
    config.allow_control,
    discovery_group.as_ref(),
  ));
  send(
    socket,
    &Frame::Register {
      version: protocol::VERSION,
      host_id: paired.host_id.clone(),
      public_key,
      name: config.name.clone(),
      allow_control: config.allow_control,
      discovery_group,
      signature: protocol::encode(&signature.to_bytes()),
    },
  )
  .await
}

async fn enqueue(outgoing: &mpsc::Sender<Frame>, frame: Frame) -> Result<(), String> {
  tokio::time::timeout(Duration::from_secs(10), outgoing.send(frame))
    .await
    .map_err(|_| "Tunnel output stalled")?
    .map_err(|_| "Tunnel disconnected".into())
}

async fn send(socket: &mut Socket, frame: &Frame) -> Result<(), String> {
  send_message(
    socket,
    Message::Text(serde_json::to_string(frame).map_err(|e| e.to_string())?.into()),
  )
  .await
}

async fn send_message(socket: &mut Socket, message: Message) -> Result<(), String> {
  tokio::time::timeout(Duration::from_secs(10), socket.send(message))
    .await
    .map_err(|_| "Tunnel send timed out")?
    .map_err(|_| "Tunnel disconnected".into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registration_diagnostics_are_bounded_and_cannot_inject_terminal_controls() {
    assert_eq!(
      registration_failure("Registration is temporarily unavailable"),
      "Hub rejected registration: Registration is temporarily unavailable"
    );
    for message in ["\u{1b}[2Jspoofed output".into(), " ".into(), "x".repeat(257)] {
      assert_eq!(registration_failure(&message), "Invalid Hub registration diagnostic");
    }
  }

  fn config(hub: &str, local: &str) -> ConnectorConfig {
    ConnectorConfig {
      hub_url: hub.parse().unwrap(),
      local_url: local.parse().unwrap(),
      key_file: PathBuf::from("unused"),
      name: "Test host".into(),
      local_token: None,
      allow_control: false,
      insecure_loopback: false,
      ice_servers: Vec::new(),
      paired: PairedHostConfig {
        host_id: "f1b8648d-5a2c-469d-85a2-c462cf440111".into(),
        noise_key_file: PathBuf::from("unused-noise"),
        state_file: PathBuf::from("unused-access"),
      },
    }
  }

  #[test]
  fn targets_are_fixed_loopback_and_hub_requires_tls() {
    let valid = config("https://hub.example.com", "http://127.0.0.1:5558");
    assert_eq!(
      valid.validate().unwrap().as_str(),
      "wss://hub.example.com/hub/v1/tunnel"
    );
    for target in [
      "http://localhost:5558",
      "http://192.168.1.1",
      "http://127.0.0.1/private",
      "http://user:pass@127.0.0.1",
      "http://127.0.0.1?target=other",
    ] {
      assert!(
        config("https://hub.example.com", target).validate().is_err(),
        "{target}"
      );
    }
    assert!(config("http://hub.example.com", "http://127.0.0.1").validate().is_err());
    let mut local = config("http://127.0.0.1:8080", "http://[::1]:5558");
    assert!(local.validate().is_err());
    local.insecure_loopback = true;
    assert!(local.validate().is_ok());
  }

  #[test]
  fn key_identity_survives_restart_and_insecure_files_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("identity.json");
    let first = load_key(&file).unwrap();
    let second = load_key(&file).unwrap();
    assert_eq!(first.verifying_key(), second.verifying_key());
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
      assert!(load_key(&file).is_err());
    }
  }
}
