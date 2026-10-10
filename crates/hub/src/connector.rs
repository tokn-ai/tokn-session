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
use tokio::{
  net::TcpStream,
  sync::{Semaphore, mpsc},
  task::JoinSet,
};
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream, connect_async_with_config,
  tungstenite::{Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[path = "connector_secure.rs"]
mod secure_transport;

#[derive(Clone)]
pub struct SecureHostConfig {
  pub noise_key_file: PathBuf,
  pub owner_public_key: String,
  pub revocations_file: Option<PathBuf>,
}

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
  pub secure: Option<SecureHostConfig>,
  pub paired: Option<PairedHostConfig>,
}

impl ConnectorConfig {
  pub fn validate(&self) -> Result<Url, String> {
    crate::secure::validate_ice_servers(&self.ice_servers)?;
    if self.secure.is_some() && self.paired.is_some() {
      return Err("Choose paired-device access or legacy signed grants, not both".into());
    }
    if self
      .paired
      .as_ref()
      .is_some_and(|paired| !protocol::valid_host_uuid(&paired.host_id))
    {
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

fn routing_id(config: &ConnectorConfig, key: &SigningKey) -> String {
  config
    .paired
    .as_ref()
    .map(|paired| paired.host_id.clone())
    .unwrap_or_else(|| protocol::host_id(key.verifying_key().as_bytes()))
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

fn load_key(path: &Path) -> Result<SigningKey, String> {
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
      if stored.version != protocol::VERSION {
        return Err("Unsupported host key file version".into());
      }
      let bytes: [u8; 32] = protocol::decode(&stored.secret_key, 32)?
        .try_into()
        .map_err(|_| "Invalid host private key")?;
      Ok(SigningKey::from_bytes(&bytes))
    }
    Err(error) => Err(format!("Could not create host key: {error}")),
  }
}

pub fn initialize_identity(path: &Path) -> Result<(), String> {
  load_key(path).map(|_| ())
}

/// Holds one connector's ownership for a saved enrollment key across CLI/app processes.
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
      format!("Could not acquire host ownership: {error}. Stop any other connector before hosting in the app.")
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
  let secure = secure_transport::Host::load(&config)?.map(Arc::new);
  let client = reqwest::Client::builder()
    .no_proxy()
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .connect_timeout(Duration::from_secs(5))
    .read_timeout(Duration::from_secs(90))
    .build()
    .map_err(|e| e.to_string())?;
  let direct = secure.as_ref().map(|host| {
    secure_transport::DirectManager::new(
      host.clone(),
      routing_id(&config, &key),
      config.clone(),
      client.clone(),
      shutdown.clone(),
    )
  });
  eprintln!("Host identity: {}", routing_id(&config, &key));
  if let Some(secure) = &secure {
    eprintln!("Host encryption public key: {}", secure.public_key());
  }
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
  secure: Option<Arc<secure_transport::Host>>,
  direct: Option<Arc<secure_transport::DirectManager>>,
  status: &Option<StatusSink>,
) -> Result<(), String> {
  let ws_config = WebSocketConfig::default()
    .max_message_size(Some(protocol::MAX_FRAME))
    .max_frame_size(Some(protocol::MAX_FRAME));
  let (mut socket, _) = tokio::time::timeout(
    Duration::from_secs(15),
    connect_async_with_config(endpoint.as_str(), Some(ws_config), false),
  )
  .await
  .map_err(|_| "Hub connection timed out")?
  .map_err(|e| format!("Could not connect to Hub: {e}"))?;
  tokio::time::timeout(Duration::from_secs(10), authenticate(&mut socket, config, key))
    .await
    .map_err(|_| "Hub authentication timed out")??;
  let (outgoing, mut outgoing_rx) = mpsc::channel::<Frame>(64);
  let mut requests = HashMap::new();
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
              Frame::RegistrationRejected { message } if !ready && config.paired.is_some() => {
                return Err(registration_failure(&message));
              }
              Frame::Pending { code, expires_in } if !ready => {
                if code.len() > 64 || code.chars().any(char::is_control) { return Err("Invalid pairing code".into()); }
                eprintln!("Approve host '{}' in the Hub using pairing code {code} (expires in {expires_in}s).", config.name);
              }
              Frame::Ready { host_id, allow_control } if !ready => {
                if host_id != routing_id(config, key) { return Err("Hub returned an incorrect host identity".into()); }
                ready = true;
                notify(status, ConnectorStatus::Online);
                if secure.is_some() { enqueue(&outgoing, Frame::SecureOnly {}).await?; }
                eprintln!("Connected to Hub as {host_id} ({})", if config.allow_control && allow_control { "control enabled" } else { "view access" });
              }
              Frame::Request { request_id, method, path, body } if ready => {
                if secure.is_some() {
                  enqueue(&outgoing, Frame::Error { request_id, message: "This host requires end-to-end encrypted access".into() }).await?;
                  continue;
                }
                if requests.contains_key(&request_id) { return Err("Hub reused an active request ID".into()); }
                if requests.len() >= protocol::MAX_REQUESTS || !protocol::allowed_route(&method, &path, config.allow_control) {
                  enqueue(&outgoing, Frame::Error { request_id, message: "Host route is unavailable or request capacity reached".into() }).await?;
                  continue;
                }
                let body = protocol::decode(&body, protocol::MAX_BODY)?;
                let config = config.clone();
                let client = client.clone();
                let outgoing = outgoing.clone();
                let window = Arc::new(Semaphore::new(protocol::RESPONSE_WINDOW));
                let credits = window.clone();
                let abort = tasks.spawn(async move {
                  let future = forward(&config, &client, &outgoing, &credits, request_id, &method, &path, body);
                  let result = if path == "/api/v1/events" { future.await } else {
                    tokio::time::timeout(Duration::from_secs(120), future).await.unwrap_or_else(|_| Err("Local API request timed out; delivery may be uncertain and will not be retried".into()))
                  };
                  if let Err(message) = result { let _ = enqueue(&outgoing, Frame::Error { request_id, message }).await; }
                  (request_id, false)
                });
                requests.insert(request_id, (abort, window));
              }
              Frame::SecureOpen { channel_id } if ready => {
                if secure_channels.contains_key(&channel_id) { return Err("Hub reused an active secure channel".into()); }
                let Some(secure) = secure.clone().filter(|_| secure_channels.len() < protocol::MAX_REQUESTS) else {
                  enqueue(&outgoing, Frame::SecureClose { channel_id }).await?;
                  continue;
                };
                let (incoming, received) = mpsc::channel(protocol::MAX_BODY / protocol::CHUNK_SIZE + 4);
                let outgoing = outgoing.clone();
                let config = config.clone();
                let client = client.clone();
                let host_id = routing_id(&config, key);
                let direct = direct.clone();
                let abort = tasks.spawn(async move {
                  let _ = secure_transport::run(&secure, &host_id, &config, &client, &outgoing, channel_id, received, direct.as_ref()).await;
                  let _ = enqueue(&outgoing, Frame::SecureClose { channel_id }).await;
                  (channel_id, true)
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
              Frame::Cancel { request_id } if ready => {
                if let Some((task, _)) = requests.remove(&request_id) { task.abort(); }
              }
              Frame::Window { request_id, credits } if ready => {
                if credits == 0 || credits > protocol::RESPONSE_WINDOW { return Err("Invalid response window".into()); }
                if let Some((_, window)) = requests.get(&request_id) {
                  if window.available_permits() + credits > protocol::RESPONSE_WINDOW { return Err("Response window overflow".into()); }
                  window.add_permits(credits);
                }
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
        if let Some(Ok((id, encrypted))) = task {
          if encrypted { secure_channels.remove(&id); } else { requests.remove(&id); }
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
  if let Some(paired) = &config.paired {
    let signature = key.sign(&protocol::registration_proof(
      &nonce,
      &paired.host_id,
      &public_key,
      &config.name,
      config.allow_control,
    ));
    return send(
      socket,
      &Frame::Register {
        version: protocol::VERSION,
        host_id: paired.host_id.clone(),
        public_key,
        name: config.name.clone(),
        allow_control: config.allow_control,
        signature: protocol::encode(&signature.to_bytes()),
      },
    )
    .await;
  }
  let signature = key.sign(&protocol::proof(
    &nonce,
    &public_key,
    &config.name,
    config.allow_control,
  ));
  send(
    socket,
    &Frame::Authenticate {
      version: protocol::VERSION,
      public_key,
      name: config.name.clone(),
      allow_control: config.allow_control,
      signature: protocol::encode(&signature.to_bytes()),
    },
  )
  .await
}

async fn forward(
  config: &ConnectorConfig,
  client: &reqwest::Client,
  outgoing: &mpsc::Sender<Frame>,
  window: &Arc<Semaphore>,
  request_id: u64,
  method: &str,
  path: &str,
  body: Vec<u8>,
) -> Result<(), String> {
  // Validate independently of Hub routing. Never resolve a destination supplied
  // by the remote peer: only replace the path on the configured loopback origin.
  if !protocol::allowed_route(method, path, config.allow_control) {
    return Err("Route unavailable".into());
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
  let response = request
    .send()
    .await
    .map_err(|_| "Local API request failed; delivery may be uncertain and will not be retried")?;
  if response.status().is_redirection() {
    return Err("Local API redirects are forbidden".into());
  }
  let content_type = response
    .headers()
    .get(reqwest::header::CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .map(str::to_owned);
  enqueue(
    outgoing,
    Frame::Response {
      request_id,
      status: response.status().as_u16(),
      content_type,
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
      // Consume one credit before emitting each chunk. The Hub replenishes it
      // only as its HTTP consumer reads, bounding memory independently per stream.
      window
        .acquire()
        .await
        .map_err(|_| "Response stream cancelled")?
        .forget();
      enqueue(
        outgoing,
        Frame::Chunk {
          request_id,
          data: protocol::encode(part),
        },
      )
      .await?;
    }
  }
  enqueue(outgoing, Frame::End { request_id }).await
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
      secure: None,
      paired: None,
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
