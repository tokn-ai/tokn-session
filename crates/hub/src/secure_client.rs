//! Independently installed E2EE endpoint serving the viewer only on loopback.
//!
//! The Hub never supplies executable code or a trusted public key to this client.
use crate::{
  protocol,
  secure::{
    InnerMessage, MAX_CHUNK, MAX_GRANT_BYTES, MAX_RECORD, MAX_REQUEST_BODY, NoiseIdentity, NoiseInitiator, SignedGrant,
  },
  server,
};
use axum::{
  Json, Router,
  body::{Body, Bytes},
  extract::{DefaultBodyLimit, Request, State},
  http::{HeaderValue, StatusCode, header},
  middleware::{self, Next},
  response::{IntoResponse, Response},
  routing::any,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::{
  collections::HashSet,
  io::Read,
  net::{IpAddr, SocketAddr},
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::{net::TcpStream, sync::Semaphore, time::Instant};
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream, connect_async_with_config,
  tungstenite::{Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const IO_TIMEOUT: Duration = Duration::from_secs(90);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub struct ClientConfig {
  pub hub_url: Url,
  pub grant_file: PathBuf,
  pub owner_public_key: String,
  pub identity_file: PathBuf,
  pub bind: SocketAddr,
  pub web_root: PathBuf,
  pub insecure_loopback: bool,
}

#[derive(Clone)]
struct ClientState {
  endpoint: Url,
  authorization: Authorization,
  identity: Arc<NoiseIdentity>,
  requests: Arc<Semaphore>,
  shutdown: CancellationToken,
}

#[derive(Clone)]
enum Authorization {
  Grant {
    grant: Arc<SignedGrant>,
    owner_public_key: String,
  },
  Device {
    host_public_key: String,
  },
}

#[derive(Clone)]
pub(crate) struct LocalBoundary {
  pub authority: String,
  pub origin: String,
  pub token: String,
}

impl LocalBoundary {
  pub fn new(address: SocketAddr) -> Self {
    Self {
      authority: address.to_string(),
      origin: format!("http://{address}"),
      token: protocol::encode(&rand::random::<[u8; 32]>()),
    }
  }
}

pub(crate) struct PairedRequest {
  pub endpoint: Url,
  pub host_public_key: String,
  pub identity: Arc<NoiseIdentity>,
  pub requests: Arc<Semaphore>,
  pub shutdown: CancellationToken,
}

pub(crate) async fn proxy_paired(target: PairedRequest, request: Request) -> Response {
  proxy(
    State(ClientState {
      endpoint: target.endpoint,
      authorization: Authorization::Device {
        host_public_key: target.host_public_key,
      },
      identity: target.identity,
      requests: target.requests,
      shutdown: target.shutdown,
    }),
    request,
  )
  .await
}

pub fn unix_time() -> Result<u64, String> {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|time| time.as_secs())
    .map_err(|_| "System clock is before the Unix epoch".into())
}

/// Local files pin identity before any communication with the untrusted Hub.
pub fn read_grant(path: &Path) -> Result<SignedGrant, String> {
  let metadata = std::fs::symlink_metadata(path).map_err(|e| format!("Could not inspect grant: {e}"))?;
  if !metadata.is_file() || metadata.len() > MAX_GRANT_BYTES as u64 {
    return Err("Grant must be a bounded regular file, not a symlink".into());
  }
  let mut file = std::fs::OpenOptions::new();
  file.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    file.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = file.open(path).map_err(|e| format!("Could not read grant: {e}"))?;
  if !file.metadata().map_err(|e| e.to_string())?.is_file() {
    return Err("Grant must be a regular file".into());
  }
  let mut bytes = Vec::new();
  file
    .take(MAX_GRANT_BYTES as u64 + 1)
    .read_to_end(&mut bytes)
    .map_err(|e| e.to_string())?;
  SignedGrant::from_json(&bytes)
}

pub(crate) fn secure_endpoint(hub: &Url, host_id: &str, insecure_loopback: bool) -> Result<Url, String> {
  if !hub.username().is_empty()
    || hub.password().is_some()
    || hub.query().is_some()
    || hub.fragment().is_some()
    || hub.path() != "/"
  {
    return Err("Hub URL must be an origin without credentials, paths, queries, or fragments".into());
  }
  if host_id.is_empty()
    || host_id.len() > 128
    || !host_id
      .bytes()
      .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-'))
  {
    return Err("Invalid host ID".into());
  }
  let loopback = matches!(hub.host(), Some(Host::Ipv4(ip)) if IpAddr::V4(ip).is_loopback())
    || matches!(hub.host(), Some(Host::Ipv6(ip)) if IpAddr::V6(ip).is_loopback())
    || hub.host_str() == Some("localhost");
  let mut endpoint = hub.clone();
  match hub.scheme() {
    "https" | "wss" => endpoint.set_scheme("wss").map_err(|_| "Invalid Hub URL")?,
    "http" | "ws" if insecure_loopback && loopback => endpoint.set_scheme("ws").map_err(|_| "Invalid Hub URL")?,
    _ => {
      return Err(
        "Hub requires HTTPS/WSS; insecure transport is allowed only for explicit loopback development".into(),
      );
    }
  }
  endpoint.set_path(&format!("/hub/v1/secure/{host_id}"));
  Ok(endpoint)
}

impl ClientState {
  fn verify_grant(&self) -> Result<(), String> {
    match &self.authorization {
      Authorization::Grant {
        grant,
        owner_public_key,
      } => grant.verify(
        owner_public_key,
        &grant.grant.host_id,
        &grant.grant.host_public_key,
        &self.identity.public_key(),
        unix_time()?,
        &HashSet::new(),
      ),
      Authorization::Device { .. } => Ok(()),
    }
  }

  fn host_public_key(&self) -> &str {
    match &self.authorization {
      Authorization::Grant { grant, .. } => &grant.grant.host_public_key,
      Authorization::Device { host_public_key } => host_public_key,
    }
  }

  fn allow_control(&self) -> bool {
    match &self.authorization {
      Authorization::Grant { grant, .. } => grant.grant.allow_control,
      // Paired devices trust the host to enforce its explicit control setting.
      Authorization::Device { .. } => true,
    }
  }

  fn expires_at(&self) -> Option<Instant> {
    match &self.authorization {
      Authorization::Grant { grant, .. } => {
        let seconds = grant.grant.expires_at.saturating_sub(unix_time().unwrap_or(u64::MAX));
        Some(
          Instant::now()
            .checked_add(Duration::from_secs(seconds))
            .unwrap_or_else(Instant::now),
        )
      }
      Authorization::Device { .. } => None,
    }
  }
}

pub async fn run(config: ClientConfig, shutdown: CancellationToken) -> Result<(), String> {
  if !config.bind.ip().is_loopback() {
    return Err("The secure viewer client must bind to a numeric loopback address".into());
  }
  let identity = Arc::new(NoiseIdentity::load_or_create(&config.identity_file)?);
  let grant = Arc::new(read_grant(&config.grant_file)?);
  let endpoint = secure_endpoint(&config.hub_url, &grant.grant.host_id, config.insecure_loopback)?;
  let listener = tokio::net::TcpListener::bind(config.bind)
    .await
    .map_err(|e| e.to_string())?;
  let address = listener.local_addr().map_err(|e| e.to_string())?;
  let boundary = LocalBoundary::new(address);
  let state = ClientState {
    endpoint,
    authorization: Authorization::Grant {
      grant: grant.clone(),
      owner_public_key: config.owner_public_key,
    },
    identity,
    requests: Arc::new(Semaphore::new(protocol::MAX_REQUESTS)),
    shutdown: shutdown.clone(),
  };
  state.verify_grant()?;
  let app = router(state, boundary.clone(), config.web_root)?;
  eprintln!("Encrypted host: {}", grant.grant.host_id);
  eprintln!(
    "Open the locally installed viewer: {}/#token={}",
    boundary.origin, boundary.token
  );
  axum::serve(listener, app)
    .with_graceful_shutdown(shutdown.cancelled_owned())
    .await
    .map_err(|e| e.to_string())
}

fn router(state: ClientState, boundary: LocalBoundary, web_root: PathBuf) -> Result<Router, String> {
  let app = Router::new()
    .route("/api/v1/{command}", any(proxy))
    .route("/api", any(not_found))
    .route("/api/{*path}", any(not_found))
    .with_state(state.clone())
    .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY));
  Ok(server::with_web_ui(app, web_root)?.layer(middleware::from_fn_with_state(boundary, protect_origin)))
}

pub(crate) fn error(status: StatusCode, message: impl Into<String>) -> Response {
  (status, Json(json!({ "error": message.into() }))).into_response()
}

async fn not_found() -> Response {
  error(StatusCode::NOT_FOUND, "Unknown local viewer route")
}

pub(crate) async fn protect_origin(State(state): State<LocalBoundary>, request: Request, next: Next) -> Response {
  // Exact numeric Host binding rejects DNS rebinding, aliases, and forwarded-host tricks.
  let headers = request.headers();
  let single = |name| {
    let mut values = headers.get_all(name).iter();
    let first = values.next().and_then(|value| value.to_str().ok());
    if values.next().is_some() { None } else { first }
  };
  if single(header::HOST) != Some(state.authority.as_str()) {
    return error(StatusCode::FORBIDDEN, "Invalid local viewer Host");
  }
  if headers.contains_key(header::ORIGIN) && single(header::ORIGIN) != Some(state.origin.as_str()) {
    return error(StatusCode::FORBIDDEN, "Cross-origin access is not permitted");
  }
  if request.uri().path().starts_with("/api/") || request.uri().path().starts_with("/paired/") {
    let expected = format!("Bearer {}", state.token);
    let supplied = single(header::AUTHORIZATION).unwrap_or_default();
    if !bool::from(expected.as_bytes().ct_eq(supplied.as_bytes())) {
      return error(StatusCode::UNAUTHORIZED, "A local viewer bearer token is required");
    }
  }
  let mut response = next.run(request).await;
  response.headers_mut().insert(
    header::CONTENT_SECURITY_POLICY,
    HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"),
  );
  response
    .headers_mut()
    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
  response
    .headers_mut()
    .insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
  response
    .headers_mut()
    .insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
  response
    .headers_mut()
    .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
  response
}

async fn proxy(State(state): State<ClientState>, request: Request) -> Response {
  let method = request.method().as_str().to_owned();
  let path = request
    .uri()
    .path_and_query()
    .map(|path| path.as_str())
    .unwrap_or_default()
    .to_owned();
  let runtime_route =
    matches!(&state.authorization, Authorization::Device { .. }) && protocol::runtime_management_route(&method, &path);
  if !protocol::allowed_route(&method, &path, state.allow_control()) && !runtime_route {
    return error(StatusCode::FORBIDDEN, "This viewer route is unavailable");
  }
  if let Err(message) = state.verify_grant() {
    return error(StatusCode::FORBIDDEN, message);
  }
  let permit = match state.requests.clone().try_acquire_owned() {
    Ok(permit) => permit,
    Err(_) => return error(StatusCode::TOO_MANY_REQUESTS, "Local viewer request capacity reached"),
  };
  let body = match axum::body::to_bytes(request.into_body(), MAX_REQUEST_BODY).await {
    Ok(body) => body,
    Err(_) => {
      return error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Secure request body exceeds its size limit",
      );
    }
  };
  let result = tokio::select! {
    _ = state.shutdown.cancelled() => return error(StatusCode::SERVICE_UNAVAILABLE, "Secure client stopped"),
    result = open_request(&state, method, path.clone(), &body) => result,
  };
  let (mut socket, mut channel, status, content_type) = match result {
    Ok(result) => result,
    Err(message) => return error(StatusCode::BAD_GATEWAY, message),
  };
  let shutdown = state.shutdown.clone();
  let expires = state.expires_at();
  let request_deadline = if path == "/api/v1/events" {
    None
  } else {
    Some(Instant::now() + REQUEST_TIMEOUT)
  };
  let stream: futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> =
    Box::pin(async_stream::try_stream! {
      let _permit = permit;
      loop {
        let deadline = [request_deadline, expires, Some(Instant::now() + IO_TIMEOUT)]
          .into_iter().flatten().min().expect("I/O timeout is always present");
        let record = tokio::select! {
          _ = shutdown.cancelled() => Err("Secure client stopped".to_owned()),
          record = receive_record(&mut socket, deadline) => record,
        }.map_err(std::io::Error::other)?;
        let message = channel.decrypt(&record).map_err(std::io::Error::other)?;
        match message {
          InnerMessage::Chunk { data } => {
            let bytes = protocol::decode(&data, MAX_CHUNK).map_err(std::io::Error::other)?;
            yield Bytes::from(bytes);
            // This executes only once the HTTP consumer polls again after the
            // preceding chunk, propagating its backpressure to the host.
            let credit = channel.encrypt(&InnerMessage::Window { credits: 1 }).map_err(std::io::Error::other)?;
            send_record(&mut socket, credit).await.map_err(std::io::Error::other)?;
          }
          InnerMessage::End {} => break,
          InnerMessage::Error { message } => Err(std::io::Error::other(message))?,
          _ => Err(std::io::Error::other("Unexpected encrypted response message"))?,
        }
      }
    });
  let mut response = Response::new(Body::from_stream(stream));
  *response.status_mut() = status;
  if let Some(content_type) = content_type {
    response.headers_mut().insert(header::CONTENT_TYPE, content_type);
  }
  response
}

async fn open_request(
  state: &ClientState,
  method: String,
  path: String,
  body: &[u8],
) -> Result<(Socket, crate::secure::SecureChannel, StatusCode, Option<HeaderValue>), String> {
  let mut socket = connect_endpoint(&state.endpoint).await?;
  let mut initiator = NoiseInitiator::new(&state.identity, state.host_public_key())?;
  send_record(&mut socket, initiator.start()?).await?;
  let reply = receive_record(&mut socket, Instant::now() + Duration::from_secs(10)).await?;
  let mut channel = initiator.finish(&reply)?;
  let request = match &state.authorization {
    Authorization::Grant { grant, .. } => InnerMessage::Request {
      method,
      path,
      grant: (**grant).clone(),
    },
    Authorization::Device { .. } => InnerMessage::DeviceRequest { method, path },
  };
  send_record(&mut socket, channel.encrypt(&request)?).await?;
  for chunk in body.chunks(MAX_CHUNK) {
    send_record(
      &mut socket,
      channel.encrypt(&InnerMessage::RequestBody {
        data: protocol::encode(chunk),
      })?,
    )
    .await?;
  }
  send_record(&mut socket, channel.encrypt(&InnerMessage::RequestEnd {})?).await?;
  let response = receive_record(&mut socket, Instant::now() + REQUEST_TIMEOUT).await?;
  match channel.decrypt(&response)? {
    InnerMessage::Response { status, content_type } => {
      if !(200..=599).contains(&status) {
        return Err("Host returned an invalid HTTP status".into());
      }
      let content_type = match content_type {
        Some(value)
          if matches!(
            value.split(';').next().map(str::trim),
            Some("application/json" | "text/event-stream")
          ) =>
        {
          Some(HeaderValue::from_str(&value).map_err(|_| "Invalid host response type")?)
        }
        None if status == 204 => None,
        _ => return Err("Host returned an unsupported response type".into()),
      };
      Ok((
        socket,
        channel,
        StatusCode::from_u16(status).map_err(|_| "Invalid host status")?,
        content_type,
      ))
    }
    InnerMessage::Error { message } => Err(message),
    _ => Err("Expected an authenticated response from the host".into()),
  }
}

pub(crate) async fn connect_endpoint(endpoint: &Url) -> Result<Socket, String> {
  let ws_config = WebSocketConfig::default()
    .max_message_size(Some(MAX_RECORD))
    .max_frame_size(Some(MAX_RECORD));
  let (socket, _) = tokio::time::timeout(
    Duration::from_secs(15),
    connect_async_with_config(endpoint.as_str(), Some(ws_config), false),
  )
  .await
  .map_err(|_| "Hub connection timed out; check the Hub address and host connection")?
  .map_err(|_| "Could not reach this host through the Hub; check its UUID and that its connector is online")?;
  Ok(socket)
}

pub(crate) async fn send_record(socket: &mut Socket, record: Vec<u8>) -> Result<(), String> {
  tokio::time::timeout(Duration::from_secs(10), socket.send(Message::Binary(record.into())))
    .await
    .map_err(|_| "Encrypted connection write timed out; request delivery may be uncertain")?
    .map_err(|_| "Encrypted connection closed; request delivery may be uncertain".into())
}

pub(crate) async fn receive_record(socket: &mut Socket, deadline: Instant) -> Result<Vec<u8>, String> {
  // Relay-controlled Ping/Pong frames never extend the authenticated data deadline.
  tokio::time::timeout_at(deadline, async {
    loop {
      match socket.next().await {
        Some(Ok(Message::Binary(bytes))) if bytes.len() <= MAX_RECORD => return Ok(bytes.to_vec()),
        Some(Ok(Message::Ping(bytes))) => socket
          .send(Message::Pong(bytes))
          .await
          .map_err(|_| "Relay disconnected")?,
        Some(Ok(Message::Pong(_))) => {}
        _ => return Err("Encrypted connection ended unexpectedly".into()),
      }
    }
  })
  .await
  .map_err(|_| "Encrypted host response timed out; requests are not retried")?
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::secure::{Grant, GrantScope, NoiseResponder, OwnerIdentity};
  use axum::extract::WebSocketUpgrade;
  use http_body_util::BodyExt;
  use tower::ServiceExt;

  fn state(host: &NoiseIdentity) -> ClientState {
    let identity = Arc::new(NoiseIdentity::generate().unwrap());
    let owner = OwnerIdentity::generate();
    let grant = owner
      .sign_grant(Grant {
        version: 1,
        grant_id: "grant_test".into(),
        host_id: "host_test".into(),
        host_public_key: host.public_key(),
        recipient_public_key: identity.public_key(),
        scope: GrantScope::All {},
        allow_control: false,
        expires_at: unix_time().unwrap() + 3600,
      })
      .unwrap();
    ClientState {
      endpoint: Url::parse("ws://127.0.0.1:9/hub/v1/secure/host_test").unwrap(),
      authorization: Authorization::Grant {
        grant: Arc::new(grant),
        owner_public_key: owner.public_key(),
      },
      identity,
      requests: Arc::new(Semaphore::new(2)),
      shutdown: CancellationToken::new(),
    }
  }

  fn request(path: &str, host: &str, origin: Option<&str>, token: Option<&str>) -> Request {
    let mut request = Request::builder().uri(path).header(header::HOST, host);
    if let Some(origin) = origin {
      request = request.header(header::ORIGIN, origin);
    }
    if let Some(token) = token {
      request = request.header(header::AUTHORIZATION, token);
    }
    request.body(Body::empty()).unwrap()
  }

  #[tokio::test]
  async fn local_browser_boundary_rejects_rebinding_cross_origin_and_missing_bearers() {
    let directory = tempfile::tempdir().unwrap();
    fs_write(directory.path().join("index.html"), b"<p>local trusted viewer</p>");
    let state = state(&NoiseIdentity::generate().unwrap());
    let boundary = LocalBoundary {
      authority: "127.0.0.1:5555".into(),
      origin: "http://127.0.0.1:5555".into(),
      token: "local-token".into(),
    };
    let app = router(state, boundary, directory.path().to_owned()).unwrap();
    for (request, expected) in [
      (request("/", "evil.example", None, None), StatusCode::FORBIDDEN),
      (
        request(
          "/api/v1/health",
          "127.0.0.1:5555",
          Some("https://evil.example"),
          Some("Bearer local-token"),
        ),
        StatusCode::FORBIDDEN,
      ),
      (
        request(
          "/api/v1/health",
          "127.0.0.1:5555",
          Some("null"),
          Some("Bearer local-token"),
        ),
        StatusCode::FORBIDDEN,
      ),
      (
        request("/api/v1/health", "127.0.0.1:5555", None, None),
        StatusCode::UNAUTHORIZED,
      ),
      (
        request("/api/v1/health", "127.0.0.1:5555", None, Some("Bearer incorrect")),
        StatusCode::UNAUTHORIZED,
      ),
      (
        request(
          "/api/v1/not_real",
          "127.0.0.1:5555",
          Some("http://127.0.0.1:5555"),
          Some("Bearer local-token"),
        ),
        StatusCode::FORBIDDEN,
      ),
    ] {
      assert_eq!(app.clone().oneshot(request).await.unwrap().status(), expected);
    }
    let response = app.oneshot(request("/", "127.0.0.1:5555", None, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
      response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .contains("connect-src 'self'")
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
      response.into_body().collect().await.unwrap().to_bytes(),
      "<p>local trusted viewer</p>"
    );
  }

  #[test]
  fn only_explicit_loopback_development_can_use_plain_websockets() {
    for url in [
      "http://example.com",
      "ws://192.168.1.2",
      "https://example.com/path",
      "https://user@example.com",
      "https://example.com/?token=secret",
    ] {
      assert!(
        secure_endpoint(&Url::parse(url).unwrap(), "host_test", true).is_err(),
        "{url}"
      );
    }
    assert!(secure_endpoint(&Url::parse("http://127.0.0.1:5559").unwrap(), "host_test", false).is_err());
    assert_eq!(
      secure_endpoint(&Url::parse("https://example.com").unwrap(), "host_test", false)
        .unwrap()
        .as_str(),
      "wss://example.com/hub/v1/secure/host_test"
    );
    assert!(secure_endpoint(&Url::parse("https://example.com").unwrap(), "../../evil", false).is_err());
  }

  #[test]
  fn grant_must_match_independently_pinned_owner_and_local_recipient() {
    let mut state = state(&NoiseIdentity::generate().unwrap());
    assert!(state.verify_grant().is_ok());
    if let Authorization::Grant { owner_public_key, .. } = &mut state.authorization {
      *owner_public_key = OwnerIdentity::generate().public_key();
    }
    assert!(state.verify_grant().is_err());
    let mut state = self::state(&NoiseIdentity::generate().unwrap());
    state.identity = Arc::new(NoiseIdentity::generate().unwrap());
    assert!(state.verify_grant().is_err());
  }

  #[tokio::test]
  async fn native_client_authenticates_host_fragments_body_and_returns_response_credits() {
    let host = NoiseIdentity::generate().unwrap();
    let mut state = state(&host);
    let expected_recipient = state.identity.public_key();
    let Authorization::Grant {
      owner_public_key: expected_owner,
      ..
    } = state.authorization.clone()
    else {
      panic!("Expected legacy grant authorization")
    };
    let expected_host = host.public_key();
    let expected_body = vec![b's'; MAX_CHUNK * 3 + 27];
    let expected = expected_body.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = Arc::new(std::sync::Mutex::new(Some(sender)));
    let app = Router::new().route(
      "/hub/v1/secure/host_test",
      axum::routing::get(move |upgrade: WebSocketUpgrade| {
        let host = host.clone();
        let sender = sender.clone();
        let expected_recipient = expected_recipient.clone();
        let expected_owner = expected_owner.clone();
        let expected_host = expected_host.clone();
        let expected = expected.clone();
        async move {
          upgrade.on_upgrade(move |mut socket| async move {
            let first = socket.recv().await.unwrap().unwrap().into_data();
            let (reply, mut channel) = NoiseResponder::new(&host).unwrap().accept(&first).unwrap();
            assert_eq!(channel.remote_public_key(), expected_recipient);
            socket
              .send(axum::extract::ws::Message::Binary(reply.into()))
              .await
              .unwrap();
            let record = socket.recv().await.unwrap().unwrap().into_data();
            assert!(
              !record
                .windows(b"grant_test".len())
                .any(|window| window == b"grant_test")
            );
            let InnerMessage::Request { method, path, grant } = channel.decrypt(&record).unwrap() else {
              panic!("Expected request")
            };
            assert_eq!(method, "POST");
            assert_eq!(path, "/api/v1/list_sessions");
            grant
              .verify(
                &expected_owner,
                "host_test",
                &expected_host,
                &expected_recipient,
                unix_time().unwrap(),
                &HashSet::new(),
              )
              .unwrap();
            let mut body = Vec::new();
            loop {
              let record = socket.recv().await.unwrap().unwrap().into_data();
              match channel.decrypt(&record).unwrap() {
                InnerMessage::RequestBody { data } => body.extend(protocol::decode(&data, MAX_CHUNK).unwrap()),
                InnerMessage::RequestEnd {} => break,
                _ => panic!("Unexpected body message"),
              }
            }
            assert_eq!(body, expected);
            let header = InnerMessage::Response {
              status: 200,
              content_type: Some("application/json".into()),
            };
            socket
              .send(axum::extract::ws::Message::Binary(
                channel.encrypt(&header).unwrap().into(),
              ))
              .await
              .unwrap();
            let mut credits = protocol::RESPONSE_WINDOW;
            for _ in 0..20 {
              if credits == 0 {
                let record = socket.recv().await.unwrap().unwrap().into_data();
                let InnerMessage::Window { credits: returned } = channel.decrypt(&record).unwrap() else {
                  panic!("Expected encrypted response credits")
                };
                credits += returned;
              }
              credits -= 1;
              let chunk = InnerMessage::Chunk {
                data: protocol::encode(b"{\"version\":1}"),
              };
              socket
                .send(axum::extract::ws::Message::Binary(
                  channel.encrypt(&chunk).unwrap().into(),
                ))
                .await
                .unwrap();
            }
            socket
              .send(axum::extract::ws::Message::Binary(
                channel.encrypt(&InnerMessage::End {}).unwrap().into(),
              ))
              .await
              .unwrap();
            // Keep the channel alive until the client consumes End and drops it,
            // so its final backpressure acknowledgements cannot race closure.
            while let Some(Ok(_)) = socket.recv().await {}
            sender.lock().unwrap().take().unwrap().send(()).unwrap();
          })
        }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    state.endpoint = Url::parse(&format!(
      "ws://{}/hub/v1/secure/host_test",
      listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let request = Request::builder()
      .method("POST")
      .uri("/api/v1/list_sessions")
      .body(Body::from(expected_body))
      .unwrap();
    let response = proxy(State(state.clone()), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
      .await
      .unwrap()
      .unwrap()
      .to_bytes();
    assert_eq!(bytes, b"{\"version\":1}".repeat(20));
    assert_eq!(state.requests.available_permits(), 2);
    receiver.await.unwrap();
    server.abort();
  }

  fn fs_write(path: PathBuf, value: &[u8]) {
    std::fs::write(path, value).unwrap();
  }
}
