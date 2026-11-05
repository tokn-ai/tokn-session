use crate::{
  bridge, emit, host,
  storage::{self, Store},
};
use futures_util::future::{Either, select};
use serde_json::{Value, json};
use std::{
  cell::{Cell, RefCell},
  collections::{BTreeMap, BTreeSet},
  rc::Rc,
};
use tokn_hub_client_core::{
  exchange::{RequestFrames, ResponseState},
  protocol,
  secure::{NoiseInitiator, SecureChannel},
};

const MAX_CONNECTIONS: usize = 8;
const MAX_REQUESTS: usize = 12;
thread_local! {static CONNECTIONS:RefCell<BTreeMap<String,Rc<Connection>>>=RefCell::new(BTreeMap::new());}
struct Connection {
  id: String,
  store: Rc<Store>,
  host_id: String,
  host_key: String,
  endpoint: String,
  closed: Cell<bool>,
  requests: Cell<usize>,
  sockets: RefCell<BTreeSet<String>>,
  stream_epoch: Cell<u64>,
  stream_socket: RefCell<Option<String>>,
  stream_channel: RefCell<Option<String>>,
}
impl Connection {
  fn descriptor(&self) -> Value {
    json!({"connection_id":self.id,"endpoint":format!("{}/machines/{}",self.store.hub.origin().ascii_serialization(),self.host_id),"host_id":self.host_id,"host_public_key":self.host_key,"transport":{"kind":"relay"}})
  }
  fn state(&self, state: &str) {
    if !self.closed.get() {
      emit("hub-client-state", json!({"connection_id":self.id,"state":state}));
    }
  }
  async fn exchange(
    self: &Rc<Self>,
    method: &str,
    path: &str,
    body: &[u8],
    limit: Option<usize>,
  ) -> Result<Exchange, String> {
    if self.closed.get() {
      return Err("Machine disconnected".into());
    }
    let frames = RequestFrames::new(method, path, body)?;
    let socket = host::Socket::open(&self.endpoint).await?;
    self.sockets.borrow_mut().insert(socket.handle.clone());
    let mut exchange = Exchange {
      socket,
      channel: None,
      response: None,
      owner: self.clone(),
    };
    if self.closed.get() {
      return Err("Machine disconnected".into());
    }
    let mut initiator = NoiseInitiator::new_with_entropy(&self.store.identity, &self.host_key, host::entropy().await?)?;
    exchange.socket.send(initiator.start()?).await?;
    let mut channel = initiator.finish(&exchange.socket.receive().await?)?;
    for frame in frames {
      exchange.socket.send(channel.encrypt(&frame)?).await?;
    }
    let header = channel.decrypt(&exchange.socket.receive().await?)?;
    exchange.response = Some(ResponseState::from_header(header, limit)?);
    exchange.channel = Some(channel);
    Ok(exchange)
  }
  async fn health(self: &Rc<Self>) -> Result<Value, String> {
    let mut exchange = self.exchange("GET", "/api/v1/health", &[], Some(64 * 1024)).await?;
    if exchange.response.as_ref().unwrap().status != 200 {
      return Err("Machine refused this device".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = exchange.next().await? {
      body.extend_from_slice(&chunk);
      exchange.ack().await?;
    }
    serde_json::from_slice(&body).map_err(|_| "Invalid machine health response".into())
  }
  async fn stop_stream(&self) {
    self.stream_epoch.set(self.stream_epoch.get().wrapping_add(1));
    let channel = self.stream_channel.borrow_mut().take();
    if let Some(channel) = channel {
      bridge::cancel(&channel);
    }
    let handle = self.stream_socket.borrow_mut().take();
    if let Some(handle) = handle {
      let _ = host::call::<Value>("websocket.close", json!({"socket_handle":handle})).await;
    }
  }
}
struct Exchange {
  socket: host::Socket,
  channel: Option<SecureChannel>,
  response: Option<ResponseState>,
  owner: Rc<Connection>,
}
impl Exchange {
  async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
    if self.owner.closed.get() {
      return Err("Machine disconnected".into());
    }
    let message = self.channel.as_mut().unwrap().decrypt(&self.socket.receive().await?)?;
    self.response.as_mut().unwrap().next(message)
  }
  async fn ack(&mut self) -> Result<(), String> {
    let frame = self.response.as_mut().unwrap().acknowledge()?;
    self.socket.send(self.channel.as_mut().unwrap().encrypt(&frame)?).await
  }
}
impl Drop for Exchange {
  fn drop(&mut self) {
    self.owner.sockets.borrow_mut().remove(&self.socket.handle);
  }
}
struct Permit(Rc<Connection>);
impl Permit {
  fn take(connection: Rc<Connection>) -> Result<Self, String> {
    if connection.requests.get() >= MAX_REQUESTS {
      return Err("Too many concurrent applet requests".into());
    }
    connection.requests.set(connection.requests.get() + 1);
    Ok(Self(connection))
  }
}
impl Drop for Permit {
  fn drop(&mut self) {
    self.0.requests.set(self.0.requests.get().saturating_sub(1));
  }
}
fn get(id: &str) -> Result<Rc<Connection>, String> {
  CONNECTIONS
    .with(|connections| connections.borrow().get(id).cloned())
    .filter(|connection| !connection.closed.get())
    .ok_or("Machine connection is closed".into())
}
pub fn inventory() -> Vec<Value> {
  CONNECTIONS.with(|connections| {
    connections
      .borrow()
      .values()
      .map(|connection| connection.descriptor())
      .collect()
  })
}
pub async fn open(params: Value) -> Result<Value, String> {
  let store = storage::load(host::string(&params, "hub_url")?).await?;
  let host_id = host::string(&params, "host_id")?;
  let saved = store
    .hosts()
    .into_iter()
    .find(|host| host.host_id == host_id)
    .ok_or("Pair this machine before opening it")?;
  if params
    .get("host_public_key")
    .and_then(Value::as_str)
    .is_some_and(|key| key != saved.host_public_key)
  {
    return Err("Machine key differs from its remembered identity".into());
  }
  let existing = CONNECTIONS.with(|connections| {
    connections
      .borrow()
      .values()
      .find(|connection| {
        !connection.closed.get()
          && connection.store.hub == store.hub
          && connection.host_id == host_id
          && connection.host_key == saved.host_public_key
      })
      .cloned()
  });
  if let Some(connection) = existing {
    // A reattached view keeps the run's connection and trust. Verify that the
    // host still admits this device before handing the existing descriptor out.
    let result = verify_health(&connection).await;
    if let Err(error) = result {
      close(&connection.id).await;
      return Err(error);
    }
    if connection.closed.get() {
      return Err("Machine disconnected".into());
    }
    store.select(host_id).await?;
    return Ok(connection.descriptor());
  }
  if CONNECTIONS.with(|connections| connections.borrow().len()) >= MAX_CONNECTIONS {
    return Err("Too many connected machines".into());
  }
  let connection = Rc::new(Connection {
    id: apell_guest::create_id(),
    endpoint: storage::endpoint(&store.hub, host_id)?,
    store: store.clone(),
    host_id: host_id.into(),
    host_key: saved.host_public_key,
    closed: Cell::new(false),
    requests: Cell::new(0),
    sockets: RefCell::new(BTreeSet::new()),
    stream_epoch: Cell::new(0),
    stream_socket: RefCell::new(None),
    stream_channel: RefCell::new(None),
  });
  verify_health(&connection).await?;
  store.select(host_id).await?;
  CONNECTIONS.with(|connections| {
    let mut connections = connections.borrow_mut();
    // Another view can finish opening the same machine while this health check
    // is pending. Reuse its verified live connection rather than leaking a run.
    if let Some(existing) = connections.values().find(|existing| {
      !existing.closed.get()
        && existing.store.hub == connection.store.hub
        && existing.host_id == connection.host_id
        && existing.host_key == connection.host_key
    }) {
      return Ok(existing.descriptor());
    }
    // Startup yielded to health and persistence. Other opens may have filled
    // the bounded registry meanwhile; never admit a ninth live connection.
    if connections.len() >= MAX_CONNECTIONS {
      return Err("Too many connected machines".to_owned());
    }
    let descriptor = connection.descriptor();
    connections.insert(connection.id.clone(), connection);
    Ok(descriptor)
  })
}

async fn verify_health(connection: &Rc<Connection>) -> Result<(), String> {
  let watchdog = bridge::Channel::new(format!("health-{}", apell_guest::create_id()))?;
  watchdog.timeout(15)?;
  match select(Box::pin(connection.health()), Box::pin(watchdog.cancellation())).await {
    Either::Left((result, _)) => {
      if result?.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("Machine uses an unsupported viewer API".into());
      }
      Ok(())
    }
    Either::Right(_) => Err("Machine health check timed out".into()),
  }
}
pub async fn request(params: Value) -> Result<Value, String> {
  let connection = get(host::string(&params, "connection_id")?)?;
  let _permit = Permit::take(connection.clone())?;
  let command = host::string(&params, "command")?;
  let response_id = host::string(&params, "response_id")?;
  validate_response_id(response_id)?;
  let payload = params.get("payload").cloned().unwrap_or(json!({}));
  let (method, path, body) = prepare_request(command, &payload)?;
  let mut bridge = bridge::Channel::new(response_id.into())?;
  let cancelled = Box::pin(bridge.cancellation());
  let operation = Box::pin(async {
    let mut exchange = connection
      .exchange(method, &path, &body, Some(128 * 1024 * 1024))
      .await?;
    let response = exchange.response.as_ref().unwrap();
    if response
      .content_type
      .as_deref()
      .is_some_and(|value| !value.starts_with("application/json"))
    {
      return Err("Expected a JSON machine response".to_owned());
    }
    let result = json!({"response_id":response_id,"status":response.status,"content_type":response.content_type});
    while let Some(chunk) = exchange.next().await? {
      bridge
        .chunk(
          "hub-client-response",
          json!({"response_id":response_id,"kind":"chunk"}),
          &chunk,
        )
        .await?;
      exchange.ack().await?;
    }
    if connection.closed.get() {
      return Err("Machine disconnected".into());
    }
    Ok(result)
  });
  match select(operation, cancelled).await {
    Either::Left((result, _)) => {
      result.map_err(|error| format!("{error}; delivery may be uncertain and input is not replayed"))
    }
    Either::Right(_) => Err("Machine request timed out; delivery may be uncertain and input is not replayed".into()),
  }
}
fn validate_response_id(value: &str) -> Result<(), String> {
  if value.is_empty()
    || value.len() > 128
    || !value
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
  {
    return Err("Invalid applet response identity".into());
  }
  Ok(())
}
fn prepare_request(command: &str, payload: &Value) -> Result<(&'static str, String, Vec<u8>), String> {
  if command.is_empty() || command.len() > 64 || !command.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_')
  {
    return Err("Invalid viewer command".into());
  }
  let method = if command == "health" { "GET" } else { "POST" };
  let path = format!("/api/v1/{command}");
  if !protocol::allowed_route(method, &path, true) && !protocol::runtime_management_route(method, &path) {
    return Err("Viewer route is unavailable".into());
  }
  let body = if method == "GET" {
    Vec::new()
  } else {
    serde_json::to_vec(payload).map_err(|_| "Invalid machine request")?
  };
  if body.len() > protocol::MAX_BODY {
    return Err("Viewer request exceeds size limit".into());
  }
  Ok((method, path, body))
}
pub async fn listen(id: &str) -> Result<Value, String> {
  let connection = get(id)?;
  connection.stop_stream().await;
  let epoch = connection.stream_epoch.get();
  emit(
    "hub-client-transport",
    json!({"connection_id":id,"transport":{"kind":"relay"}}),
  );
  apell_guest::spawn(async move {
    pump(connection, epoch).await;
  })
  .map_err(|error| error.message)?;
  Ok(Value::Null)
}
async fn pump(connection: Rc<Connection>, epoch: u64) {
  let mut attempt = 0u64;
  while !connection.closed.get() && connection.stream_epoch.get() == epoch {
    connection.state(if attempt == 0 { "connecting" } else { "reconnecting" });
    let generation = format!("{epoch}-{attempt}");
    let channel_id = format!("{}:{generation}", connection.id);
    let mut bridge = match bridge::Channel::new(channel_id.clone()) {
      Ok(channel) => channel,
      Err(_) => return,
    };
    bridge.live();
    *connection.stream_channel.borrow_mut() = Some(channel_id.clone());
    emit(
      "hub-client-stream-reset",
      json!({"connection_id":connection.id,"generation":generation,"channel_id":channel_id}),
    );
    let result = async {
      let mut exchange = connection.exchange("GET", "/api/v1/events", &[], None).await?;
      // A replacement listener can retire this generation while its socket or
      // handshake is opening. Drop that exact exchange before touching the new
      // generation's socket handle or waiting for its first response chunk.
      if connection.closed.get() || connection.stream_epoch.get() != epoch {
        return Ok(());
      }
      *connection.stream_socket.borrow_mut() = Some(exchange.socket.handle.clone());
      let response = exchange.response.as_ref().unwrap();
      if response.status != 200
        || !response
          .content_type
          .as_deref()
          .is_some_and(|value| value.starts_with("text/event-stream"))
      {
        return Err("Host refused live updates".to_owned());
      }
      while let Some(chunk) = exchange.next().await? {
        if connection.stream_epoch.get() != epoch {
          return Ok(());
        }
        bridge
          .chunk(
            "hub-client-stream-chunk",
            json!({"connection_id":connection.id,"generation":generation}),
            &chunk,
          )
          .await?;
        exchange.ack().await?;
      }
      Err::<(), String>("Machine closed live updates".into())
    }
    .await;
    drop(bridge);
    if connection.closed.get() || connection.stream_epoch.get() != epoch {
      return;
    }
    if let Err(reason) = result {
      emit(
        "hub-client-stream-error",
        json!({"connection_id":connection.id,"generation":generation,"reason":reason}),
      );
    }
    attempt = attempt.wrapping_add(1);
    if host::delay(1000).await.is_err() {
      return;
    }
  }
}
pub async fn close(id: &str) {
  let connection = CONNECTIONS.with(|connections| connections.borrow_mut().remove(id));
  if let Some(connection) = connection {
    connection.closed.set(true);
    connection.stop_stream().await;
    let sockets = connection.sockets.borrow().iter().cloned().collect::<Vec<_>>();
    for socket in sockets {
      let _ = host::call::<Value>("websocket.close", json!({"socket_handle":socket})).await;
    }
  }
}
pub async fn close_host(hub: &str, id: &str) {
  let ids = CONNECTIONS.with(|connections| {
    connections
      .borrow()
      .values()
      .filter(|connection| connection.store.hub.as_str() == hub && connection.host_id == id)
      .map(|connection| connection.id.clone())
      .collect::<Vec<_>>()
  });
  for id in ids {
    close(&id).await;
  }
}
#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn only_existing_authorized_viewer_routes_can_be_tunneled() {
    assert!(prepare_request("list_sessions", &json!({})).is_ok());
    for invalid in ["../secrets", "initialize_local_viewer", "list/Sessions", ""] {
      assert!(prepare_request(invalid, &json!({})).is_err());
    }
    assert!(
      prepare_request(
        "submit_session_input",
        &json!({"text":"x".repeat(protocol::MAX_BODY+1)})
      )
      .is_err()
    );
  }
}
