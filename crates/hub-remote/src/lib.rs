//! Direct native app connections: the app owns keys and decrypts host content.
mod direct;
mod directory;
pub use direct::{TransportKind, TransportState};
mod exchange;
mod stream;

use exchange::{Exchange, Socket, connect, receive, send};
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
pub use tokn_hub_client_core::address::ResolvedMachine;
use tokn_hub_client_core::{
  pairing::ClientPairing,
  protocol,
  secure::{HostAuthOperation, InnerMessage, NoiseIdentity, NoiseInitiator, SecureChannel},
};
use tokn_session_hub::onboarding::ClientStore;
pub use tokn_session_hub::onboarding::SavedHost;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

/// Callback receives event name and an envelope scoped to one connection.
pub type EventSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

#[derive(Serialize)]
pub struct ClientStatus {
  pub hub_url: String,
  pub device_public_key: String,
  pub hosts: Vec<SavedHost>,
  pub selected_host_id: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct ConnectionInfo {
  pub connection_id: String,
  pub endpoint: String,
  pub host_id: String,
  pub host_public_key: String,
  pub transport: TransportState,
}

#[derive(Serialize)]
pub struct AuthOptions {
  pub auth_id: String,
  pub options: Value,
}

struct PendingAuth {
  socket: Socket,
  channel: SecureChannel,
  store: ClientStore,
  host: SavedHost,
  register: bool,
}

struct Authentication {
  pending: Mutex<Option<PendingAuth>>,
  cancellation: CancellationToken,
  // Cancellation and the atomic filesystem commit have one ordering. A cancel
  // before this gate prevents saving; an already-started commit finishes first.
  persistence: Mutex<()>,
  expires: tokio::time::Instant,
}

struct Connection {
  info: ConnectionInfo,
  endpoint: Url,
  identity: Arc<NoiseIdentity>,
  cancellation: CancellationToken,
  requests: Arc<Semaphore>,
  listening: std::sync::atomic::AtomicBool,
  sink: EventSink,
  direct: direct::DirectRoute,
}

#[derive(Clone)]
pub struct RemoteManager {
  state_dir: PathBuf,
  connections: Arc<Mutex<HashMap<String, Arc<Connection>>>>,
  authentications: Arc<Mutex<HashMap<String, Arc<Authentication>>>>,
}

impl RemoteManager {
  pub fn new(state_dir: PathBuf) -> Self {
    Self {
      state_dir,
      connections: Arc::new(Mutex::new(HashMap::new())),
      authentications: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  async fn stored(&self, hub_url: &str) -> Result<(Url, ClientStore, Arc<NoiseIdentity>), String> {
    let hub = canonical_hub(hub_url)?;
    let state_dir = self.state_dir.clone();
    let hub_copy = hub.clone();
    let (store, identity) = tokio::task::spawn_blocking(move || {
      let store = ClientStore::load_or_create(&state_dir, &hub_copy)?;
      let identity = Arc::new(NoiseIdentity::load_or_create(&store.identity_file())?);
      Ok::<_, String>((store, identity))
    })
    .await
    .map_err(|error| format!("Load remembered device: {error}"))??;
    Ok((hub, store, identity))
  }

  pub async fn status(&self, hub_url: &str) -> Result<ClientStatus, String> {
    let (hub, store, identity) = self.stored(hub_url).await?;
    tokio::task::spawn_blocking(move || {
      Ok(ClientStatus {
        hub_url: hub.origin().ascii_serialization(),
        device_public_key: identity.public_key(),
        hosts: store.hosts()?,
        selected_host_id: store.selected_host()?,
      })
    })
    .await
    .map_err(|error| error.to_string())?
  }

  pub async fn resolve(&self, hub_url: &str, machine_address: &str) -> Result<ResolvedMachine, String> {
    directory::resolve(hub_url, machine_address).await
  }

  pub async fn remember_metadata(
    &self,
    hub_url: &str,
    host_id: &str,
    machine_address: &str,
    name: &str,
  ) -> Result<SavedHost, String> {
    validate_host(host_id)?;
    tokn_hub_client_core::address::parse_machine_address(machine_address)?;
    tokn_hub_client_core::address::validate_machine_name(name)?;
    let (_, store, _) = self.stored(hub_url).await?;
    if !store.hosts()?.iter().any(|host| host.host_id == host_id) {
      return Err("Pair this machine before remembering its address".into());
    }
    let resolved = self.resolve(hub_url, machine_address).await?;
    if resolved.host_id != host_id {
      return Err(
        "Machine address does not resolve to the remembered UUID; its original identity was preserved".into(),
      );
    }
    let host_id = host_id.to_owned();
    let machine_address = resolved.machine_address;
    let name = resolved.name;
    tokio::task::spawn_blocking(move || store.remember_metadata(&host_id, &machine_address, &name))
      .await
      .map_err(|error| error.to_string())?
  }

  pub async fn pair(
    &self,
    hub_url: &str,
    host_id: &str,
    code: String,
    expected_host_public_key: Option<&str>,
  ) -> Result<SavedHost, String> {
    self
      .pair_with_address(hub_url, host_id, code, expected_host_public_key, None)
      .await
  }

  pub async fn pair_with_address(
    &self,
    hub_url: &str,
    host_id: &str,
    code: String,
    expected_host_public_key: Option<&str>,
    machine_address: Option<&str>,
  ) -> Result<SavedHost, String> {
    let code = Zeroizing::new(code);
    let (hub, store, identity) = self.stored(hub_url).await?;
    validate_host(host_id)?;
    if store.hosts()?.iter().any(|host| host.host_id == host_id) {
      return Err("This machine is already paired; open it from remembered machines".into());
    }
    if let Some(address) = machine_address {
      if self.resolve(hub_url, address).await?.host_id != host_id {
        return Err("Machine address does not resolve to the requested UUID; resolve its address again".into());
      }
    }
    let mut socket = connect(&secure_endpoint(&hub, host_id)?).await?;
    let (pairing, first) = ClientPairing::start(host_id, &identity, &code, unix_time()?)?;
    send(&mut socket, first).await?;
    let (pairing, confirmation) = pairing.confirm(&receive(&mut socket).await?)?;
    send(&mut socket, confirmation).await?;
    let verified = pairing.finish(&receive(&mut socket).await?)?;
    if expected_host_public_key.is_some_and(|key| key != verified.host_public_key) {
      return Err("Authenticated machine key does not match the supplied reference".into());
    }
    let metadata = if let Some(address) = machine_address {
      let resolved = self.resolve(hub_url, address).await?;
      if resolved.host_id != verified.host_id {
        return Err("Machine address changed UUID during pairing; its original saved identity was preserved".into());
      }
      Some(resolved)
    } else {
      None
    };
    let host = SavedHost {
      host_id: verified.host_id,
      host_public_key: verified.host_public_key,
      machine_address: metadata.as_ref().map(|metadata| metadata.machine_address.clone()),
      name: metadata.map(|metadata| metadata.name),
    };
    let saved = host.clone();
    tokio::task::spawn_blocking(move || store.save_host(saved))
      .await
      .map_err(|error| error.to_string())??;
    Ok(host)
  }

  pub async fn open(
    &self,
    hub_url: &str,
    host_id: &str,
    host_public_key: Option<&str>,
    sink: EventSink,
  ) -> Result<ConnectionInfo, String> {
    let (hub, store, identity) = self.stored(hub_url).await?;
    validate_host(host_id)?;
    let host = store
      .hosts()?
      .into_iter()
      .find(|host| host.host_id == host_id)
      .ok_or("Pair this machine before opening it")?;
    if host_public_key.is_some_and(|key| key != host.host_public_key) {
      return Err("Machine key does not match its remembered identity".into());
    }
    let info = ConnectionInfo {
      connection_id: Uuid::new_v4().to_string(),
      endpoint: format!("{}/machines/{}", hub.origin().ascii_serialization(), host_id),
      host_id: host.host_id,
      host_public_key: host.host_public_key,
      transport: TransportState::default(),
    };
    let connection = Arc::new(Connection {
      endpoint: secure_endpoint(&hub, host_id)?,
      info: info.clone(),
      identity,
      cancellation: CancellationToken::new(),
      requests: Arc::new(Semaphore::new(protocol::MAX_REQUESTS)),
      listening: std::sync::atomic::AtomicBool::new(false),
      sink,
      direct: direct::DirectRoute::default(),
    });
    let health = connection.request("health", json!({})).await?;
    if health.get("version").and_then(Value::as_u64) != Some(1) {
      return Err("Machine uses an unsupported viewer API version".into());
    }
    store.select_host(host_id)?;
    let mut connections = self.connections.lock().await;
    if connections.len() >= 16 {
      return Err("Too many open machine connections".into());
    }
    connections.insert(info.connection_id.clone(), connection.clone());
    drop(connections);
    tokio::spawn(direct::upgrade(connection));
    Ok(info)
  }

  async fn connection(&self, connection_id: &str) -> Result<Arc<Connection>, String> {
    self
      .connections
      .lock()
      .await
      .get(connection_id)
      .cloned()
      .ok_or("Machine connection is closed".into())
  }

  pub async fn request(&self, connection_id: &str, command: &str, payload: Value) -> Result<Value, String> {
    self.connection(connection_id).await?.request(command, payload).await
  }

  pub async fn listen(&self, connection_id: &str) -> Result<(), String> {
    let connection = self.connection(connection_id).await?;
    connection.transport_event(connection.direct.changes.borrow().clone());
    if connection.listening.swap(true, std::sync::atomic::Ordering::AcqRel) {
      return Ok(());
    }
    let (ready, result) = tokio::sync::oneshot::channel();
    tokio::spawn(stream::pump(connection, ready));
    result.await.map_err(|_| "Live connection stopped")?
  }

  pub async fn close(&self, connection_id: &str) {
    let connection = { self.connections.lock().await.remove(connection_id) };
    if let Some(connection) = connection {
      connection.cancellation.cancel();
      connection.direct.close().await;
    }
  }

  pub async fn close_all(&self) {
    let connections: Vec<_> = self
      .connections
      .lock()
      .await
      .drain()
      .map(|(_, connection)| connection)
      .collect();
    for connection in connections {
      connection.cancellation.cancel();
      connection.direct.close().await;
    }
    let authentications: Vec<_> = self
      .authentications
      .lock()
      .await
      .drain()
      .map(|(_, auth)| auth)
      .collect();
    for authentication in authentications {
      let _commit = authentication.persistence.lock().await;
      authentication.cancellation.cancel();
    }
  }

  pub async fn forget(&self, hub_url: &str, host_id: &str) -> Result<(), String> {
    let (hub, store, _) = self.stored(hub_url).await?;
    let prefix = format!("{}/machines/", hub.origin().ascii_serialization());
    let ids: Vec<_> = self
      .connections
      .lock()
      .await
      .values()
      .filter(|connection| connection.info.host_id == host_id && connection.info.endpoint.starts_with(&prefix))
      .map(|connection| connection.info.connection_id.clone())
      .collect();
    for id in ids {
      self.close(&id).await;
    }
    let host_id = host_id.to_owned();
    tokio::task::spawn_blocking(move || store.forget_host(&host_id))
      .await
      .map_err(|error| error.to_string())?
  }

  pub async fn auth_start(
    &self,
    hub_url: &str,
    host_id: &str,
    host_public_key: &str,
    register: bool,
  ) -> Result<AuthOptions, String> {
    let (hub, store, identity) = self.stored(hub_url).await?;
    validate_host(host_id)?;
    tokn_hub_client_core::secure::decode_public_key(host_public_key)?;
    if let Some(saved) = store.hosts()?.iter().find(|host| host.host_id == host_id) {
      if saved.host_public_key != host_public_key {
        return Err("Machine key does not match its remembered identity".into());
      }
    } else if register {
      return Err("Pair this machine before enrolling a passkey".into());
    }
    let mut socket = connect(&secure_endpoint(&hub, host_id)?).await?;
    let mut initiator = NoiseInitiator::new(&identity, host_public_key)?;
    send(&mut socket, initiator.start()?).await?;
    let mut channel = initiator.finish(&receive(&mut socket).await?)?;
    send(
      &mut socket,
      channel.encrypt(&InnerMessage::AuthRequest {
        operation: if register {
          HostAuthOperation::RegisterStart
        } else {
          HostAuthOperation::LoginStart
        },
        payload: json!({}),
      })?,
    )
    .await?;
    let response = channel.decrypt(&receive(&mut socket).await?)?;
    let options = match response {
      InnerMessage::AuthResponse { payload } => {
        payload.get("options").cloned().ok_or("Host omitted passkey options")?
      }
      InnerMessage::Error { message } => return Err(message),
      _ => return Err("Expected encrypted passkey options".into()),
    };
    let auth_id = Uuid::new_v4().to_string();
    let mut pending = self.authentications.lock().await;
    pending.retain(|_, auth| {
      if auth.expires > tokio::time::Instant::now() {
        return true;
      }
      auth.cancellation.cancel();
      false
    });
    if pending.len() >= 4 {
      return Err("Too many pending passkey ceremonies".into());
    }
    pending.insert(
      auth_id.clone(),
      Arc::new(Authentication {
        pending: Mutex::new(Some(PendingAuth {
          socket,
          channel,
          store,
          host: SavedHost {
            host_id: host_id.into(),
            host_public_key: host_public_key.into(),
            machine_address: None,
            name: None,
          },
          register,
        })),
        cancellation: CancellationToken::new(),
        persistence: Mutex::new(()),
        expires: tokio::time::Instant::now() + Duration::from_secs(120),
      }),
    );
    Ok(AuthOptions { auth_id, options })
  }

  pub async fn auth_finish(&self, auth_id: &str, credential: Value) -> Result<SavedHost, String> {
    let authentication = self
      .authentications
      .lock()
      .await
      .get(auth_id)
      .cloned()
      .ok_or("Passkey ceremony is missing or already used")?;
    let pending = authentication
      .pending
      .lock()
      .await
      .take()
      .ok_or("Passkey ceremony is missing or already used")?;
    let result = authentication.finish(pending, credential).await;
    self.authentications.lock().await.remove(auth_id);
    result
  }

  pub async fn auth_cancel(&self, auth_id: &str) {
    let authentication = self.authentications.lock().await.remove(auth_id);
    if let Some(authentication) = authentication {
      let _commit = authentication.persistence.lock().await;
      authentication.cancellation.cancel();
    }
  }
}

impl Authentication {
  async fn finish(&self, mut pending: PendingAuth, credential: Value) -> Result<SavedHost, String> {
    if self.expires <= tokio::time::Instant::now() {
      return Err("Passkey ceremony expired".into());
    }
    tokio::select! {
      biased;
      _ = self.cancellation.cancelled() => return Err("Passkey ceremony cancelled".into()),
      result = tokio::time::timeout_at(self.expires, async {
        send(&mut pending.socket, pending.channel.encrypt(&InnerMessage::AuthRequest {
          operation: if pending.register { HostAuthOperation::RegisterFinish } else { HostAuthOperation::LoginFinish },
          payload: json!({"credential":credential}),
        })?).await?;
        match pending.channel.decrypt(&receive(&mut pending.socket).await?)? {
          InnerMessage::AuthResponse {payload} if payload.get("authorized") == Some(&Value::Bool(true)) => Ok(()),
          InnerMessage::Error {message} => Err(message),
          _ => Err("Host did not authorize this device".into()),
        }
      }) => result.map_err(|_| "Passkey ceremony expired")??,
    }
    let _commit = self.persistence.lock().await;
    if self.cancellation.is_cancelled() {
      return Err("Passkey ceremony cancelled".into());
    }
    if self.expires <= tokio::time::Instant::now() {
      return Err("Passkey ceremony expired".into());
    }
    let host = pending.host.clone();
    // Authentication remembers a pin; only opening a machine changes selection.
    tokio::task::spawn_blocking(move || pending.store.save_host(pending.host))
      .await
      .map_err(|error| error.to_string())??;
    Ok(host)
  }
}

impl Connection {
  fn transport_event(&self, transport: TransportState) {
    (self.sink)(
      "hub-client-transport",
      json!({"connection_id":self.info.connection_id,"transport":transport}),
    );
  }

  async fn exchange(&self, method: &str, path: &str, body: &[u8]) -> Result<Exchange, String> {
    let transport = self.direct.open(self).await?;
    Exchange::open(
      transport,
      &self.identity,
      &self.info.host_public_key,
      method,
      path,
      body,
    )
    .await
  }

  fn state(&self, state: &str) {
    (self.sink)(
      "hub-client-state",
      json!({"connection_id":self.info.connection_id,"state":state}),
    );
  }
  fn event(&self, event: &str, payload: Value) {
    (self.sink)(
      "hub-client-event",
      json!({"connection_id":self.info.connection_id,"event":event,"payload":payload}),
    );
  }

  async fn request(&self, command: &str, payload: Value) -> Result<Value, String> {
    if command.is_empty() || command.len() > 64 || !command.bytes().all(|ch| ch.is_ascii_lowercase() || ch == b'_') {
      return Err("Invalid viewer command".into());
    }
    let method = if command == "health" { "GET" } else { "POST" };
    let path = format!("/api/v1/{command}");
    if !protocol::allowed_route(method, &path, true) {
      return Err("Viewer route is unavailable".into());
    }
    let body = if method == "GET" {
      Vec::new()
    } else {
      serde_json::to_vec(&payload).map_err(|error| error.to_string())?
    };
    if body.len() > protocol::MAX_BODY {
      return Err("Viewer request exceeds size limit".into());
    }
    let _permit = self
      .requests
      .clone()
      .try_acquire_owned()
      .map_err(|_| "Too many concurrent viewer requests")?;
    let response = tokio::select! {
      _ = self.cancellation.cancelled() => return Err("Machine disconnected".into()),
      response = tokio::time::timeout(Duration::from_secs(120), async {
        let exchange = self.exchange(method, &path, &body).await?;
        exchange.json().await
      }) => response.map_err(|_| "Machine request timed out; delivery may be uncertain and requests are not retried")?,
    }?;
    Ok(response)
  }
}

pub fn canonical_hub(value: &str) -> Result<Url, String> {
  let url = Url::parse(value.trim()).map_err(|_| "Enter a valid Hub URL")?;
  if url.path() != "/"
    || !url.username().is_empty()
    || url.password().is_some()
    || url.query().is_some()
    || url.fragment().is_some()
  {
    return Err("Hub URL must be an origin without credentials, path, query or fragment".into());
  }
  let local = match url.host() {
    Some(url::Host::Domain("localhost")) => true,
    Some(url::Host::Ipv4(address)) => address.is_loopback(),
    Some(url::Host::Ipv6(address)) => address.is_loopback(),
    _ => false,
  };
  if url.scheme() != "https" && !(url.scheme() == "http" && local) {
    return Err("Hub requires HTTPS; HTTP is available only on loopback".into());
  }
  Ok(url)
}

fn secure_endpoint(hub: &Url, host_id: &str) -> Result<Url, String> {
  let mut endpoint = hub.clone();
  endpoint
    .set_scheme(if hub.scheme() == "https" { "wss" } else { "ws" })
    .map_err(|_| "Invalid Hub scheme")?;
  endpoint.set_path(&format!("/hub/v1/secure/{host_id}"));
  Ok(endpoint)
}

fn validate_host(value: &str) -> Result<(), String> {
  if !protocol::valid_host_uuid(value) {
    return Err("Enter a canonical machine UUID".into());
  }
  Ok(())
}

fn unix_time() -> Result<u64, String> {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|time| time.as_secs())
    .map_err(|_| "System clock is invalid".into())
}
