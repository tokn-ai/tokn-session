mod session;

use self::session::{Session, Stream};
use crate::{
  bridge, emit, host,
  storage::{self, Store},
};
use futures_util::{
  future::{AbortHandle, Abortable, Either, select},
  lock::Mutex,
};
use serde_json::{Value, json};
use std::{
  cell::{Cell, RefCell},
  collections::BTreeMap,
  rc::Rc,
};
use tokn_hub_client_core::{
  discovery::DiscoveryGroup,
  exchange::{RequestFrames, ResponseState},
  protocol,
  secure::InnerMessage,
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
  ready: Cell<bool>,
  opening: Mutex<()>,
  session: Mutex<Option<Rc<Session>>>,
  connecting: RefCell<Option<AbortHandle>>,
  stream_epoch: Cell<u64>,
  stream_channel: RefCell<Option<String>>,
}
impl Connection {
  fn descriptor(&self) -> Value {
    let discovery_group = self
      .store
      .hosts()
      .into_iter()
      .find(|host| host.host_id == self.host_id)
      .and_then(|host| host.discovery_group);
    let mut descriptor = json!({"connection_id":self.id,"endpoint":format!("{}/machines/{}",self.store.hub.origin().ascii_serialization(),self.host_id),"host_id":self.host_id,"host_public_key":self.host_key,"transport":{"kind":"relay"}});
    if let Some(group) = discovery_group {
      descriptor["discovery_group"] = json!(group);
    }
    descriptor
  }
  fn state(&self, state: &str) {
    if !self.closed.get() {
      emit("hub-client-state", json!({"connection_id":self.id,"state":state}));
    }
  }
  async fn session(&self) -> Result<Rc<Session>, String> {
    let mut active = self.session.lock().await;
    if self.closed.get() {
      return Err("Machine disconnected".into());
    }
    if let Some(session) = active.as_ref().filter(|session| session.is_live()) {
      return Ok(session.clone());
    }
    let (dial, cancellation) = AbortHandle::new_pair();
    *self.connecting.borrow_mut() = Some(dial);
    let session = Abortable::new(
      Session::connect(&self.endpoint, &self.store.identity, &self.host_key),
      cancellation,
    )
    .await;
    self.connecting.borrow_mut().take();
    let session = session.map_err(|_| "Machine disconnected")??;
    if self.closed.get() {
      return Err("Machine disconnected".into());
    }
    *active = Some(session.clone());
    Ok(session)
  }
  async fn exchange(&self, method: &str, path: &str, body: &[u8], limit: Option<usize>) -> Result<Exchange, String> {
    let mut frames = RequestFrames::new(method, path, body)?;
    let mut stream = self
      .session()
      .await?
      .open(frames.next().ok_or("Missing machine request header")?)
      .await?;
    for frame in frames {
      stream.send(frame).await?;
    }
    let response = ResponseState::from_header(stream.receive().await?, limit)?;
    Ok(Exchange { stream, response })
  }
  async fn health(&self) -> Result<(u16, Value), String> {
    let mut exchange = self.exchange("GET", "/api/v1/health", &[], Some(64 * 1024)).await?;
    if exchange.response.status != 200 {
      return Ok((exchange.response.status, Value::Null));
    }
    let mut body = Vec::new();
    while let Some(chunk) = exchange.next().await? {
      body.extend_from_slice(&chunk);
      exchange.ack().await?;
    }
    Ok((
      200,
      serde_json::from_slice(&body).map_err(|_| "Invalid machine health response")?,
    ))
  }
  async fn discovery_group(self: &Rc<Self>) -> Result<Option<DiscoveryGroup>, String> {
    let mut stream = self.session().await?.open(InnerMessage::DiscoveryRequest {}).await?;
    match stream.receive().await? {
      InnerMessage::DiscoveryResponse { group } => Ok(group),
      InnerMessage::Error { message } => Err(message),
      _ => Err("Machine returned invalid discovery metadata".into()),
    }
  }
  fn stop_stream(&self) {
    self.stream_epoch.set(self.stream_epoch.get().wrapping_add(1));
    let channel = self.stream_channel.borrow_mut().take();
    if let Some(channel) = channel {
      bridge::cancel(&channel);
    }
  }
}
struct Exchange {
  stream: Stream,
  response: ResponseState,
}
impl Exchange {
  async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
    self.response.next(self.stream.receive().await?)
  }
  async fn ack(&mut self) -> Result<(), String> {
    self.stream.send(self.response.acknowledge()?).await
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
    .filter(|connection| connection.ready.get() && !connection.closed.get())
    .ok_or("Machine connection is closed".into())
}
pub fn inventory() -> Vec<Value> {
  CONNECTIONS.with(|connections| {
    connections
      .borrow()
      .values()
      .filter(|connection| connection.ready.get())
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
  // Publish the pending connection internally before the first network await.
  // Concurrent views share one handshake, but only verified connections appear
  // in inventory or accept commands.
  let connection = CONNECTIONS.with(|connections| -> Result<Rc<Connection>, String> {
    let mut connections = connections.borrow_mut();
    if let Some(existing) = connections.values().find(|connection| {
      !connection.closed.get()
        && connection.store.hub == store.hub
        && connection.host_id == host_id
        && connection.host_key == saved.host_public_key
    }) {
      return Ok(existing.clone());
    }
    if connections.len() >= MAX_CONNECTIONS {
      return Err("Too many connected machines".into());
    }
    let connection = Rc::new(Connection {
      id: apell_guest::create_id(),
      endpoint: storage::endpoint(&store.hub, host_id)?,
      store: store.clone(),
      host_id: host_id.into(),
      host_key: saved.host_public_key,
      closed: Cell::new(false),
      ready: Cell::new(false),
      opening: Mutex::new(()),
      requests: Cell::new(0),
      session: Mutex::new(None),
      connecting: RefCell::new(None),
      stream_epoch: Cell::new(0),
      stream_channel: RefCell::new(None),
    });
    connections.insert(connection.id.clone(), connection.clone());
    Ok(connection)
  })?;
  let reattach = connection.ready.get();
  let _opening = connection.opening.lock().await;
  if connection.closed.get() {
    return Err("Machine disconnected".into());
  }
  if !reattach && connection.ready.get() {
    return Ok(connection.descriptor());
  }
  if let Err(error) = verify_health(&connection).await {
    if !connection.ready.get()
      || matches!(error, HealthFailure::Rejected(_))
      || connection
        .session
        .lock()
        .await
        .as_ref()
        .is_none_or(|session| !session.is_live())
    {
      close(&connection.id).await;
    }
    return Err(match error {
      HealthFailure::Rejected(reason) | HealthFailure::Temporary(reason) => reason,
    });
  }
  let result = async {
    refresh_discovery(&connection).await?;
    if connection.closed.get() {
      return Err("Machine disconnected".into());
    }
    store.select(host_id).await?;
    if connection.closed.get() {
      return Err("Machine disconnected".into());
    }
    connection.ready.set(true);
    Ok(connection.descriptor())
  }
  .await;
  if result.is_err() && !connection.ready.get() {
    close(&connection.id).await;
  }
  result
}

enum HealthFailure {
  Rejected(String),
  Temporary(String),
}

async fn verify_health(connection: &Connection) -> Result<(), HealthFailure> {
  let watchdog =
    bridge::Channel::new(format!("health-{}", apell_guest::create_id())).map_err(HealthFailure::Temporary)?;
  watchdog.timeout(15).map_err(HealthFailure::Temporary)?;
  match select(Box::pin(connection.health()), Box::pin(watchdog.cancellation())).await {
    Either::Left((result, _)) => {
      let (status, body) = result.map_err(HealthFailure::Temporary)?;
      if matches!(status, 401 | 403) {
        return Err(HealthFailure::Rejected("Machine refused this device".into()));
      }
      if status != 200 {
        return Err(HealthFailure::Temporary(format!(
          "Machine health check returned HTTP {status}"
        )));
      }
      if body.get("version").and_then(Value::as_u64) != Some(protocol::VIEWER_API_VERSION) {
        return Err(HealthFailure::Rejected(
          "Machine uses an unsupported viewer API version. Update the machine's viewer.".into(),
        ));
      }
      Ok(())
    }
    Either::Right(_) => Err(HealthFailure::Temporary("Machine health check timed out".into())),
  }
}

async fn refresh_discovery(connection: &Rc<Connection>) -> Result<(), String> {
  let watchdog = bridge::Channel::new(format!("discovery-{}", apell_guest::create_id()))?;
  watchdog.timeout(5)?;
  let result = select(
    Box::pin(connection.discovery_group()),
    Box::pin(watchdog.cancellation()),
  )
  .await;
  if let Either::Left((Ok(group), _)) = result {
    if !connection.closed.get() {
      connection
        .store
        .remember_discovery_group(&connection.host_id, &connection.host_key, group)
        .await?;
    }
  }
  Ok(())
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
    let response = &exchange.response;
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
  if command == "events" {
    return Err("Use the live event listener for event streams".into());
  }
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
  connection.stop_stream();
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
  let subscription = json!({"subscription_prefix":connection.id}).to_string();
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
    let cancelled = bridge.cancellation();
    let operation = async {
      let mut exchange = connection
        .exchange("POST", "/api/v1/events", subscription.as_bytes(), None)
        .await?;
      if connection.closed.get() || connection.stream_epoch.get() != epoch {
        return Ok(());
      }
      let response = &exchange.response;
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
    };
    let result = match select(Box::pin(operation), Box::pin(cancelled)).await {
      Either::Left((result, _)) => result,
      Either::Right(_) => Err("Live updates detached".into()),
    };
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
    if let Some(dial) = connection.connecting.borrow_mut().take() {
      dial.abort();
    }
    connection.stop_stream();
    if let Some(session) = connection.session.lock().await.take() {
      session.stop("Machine disconnected".into());
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
