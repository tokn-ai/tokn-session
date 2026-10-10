//! Locally served authenticator onboarding and pinned, remembered host access.
//!
//! All executable UI assets come from the installed viewer. The Hub receives
//! only PAKE messages and authenticated ciphertext, never authenticator codes.
use crate::{
  onboarding::{ClientStore, SavedHost},
  pairing::ClientPairing,
  protocol,
  secure::{MAX_REQUEST_BODY, NoiseIdentity},
  secure_client::{self, LocalBoundary, PairedRequest, error},
  server,
};
use axum::{
  Json, Router,
  extract::{DefaultBodyLimit, Path, Request, State},
  http::{StatusCode, Uri},
  middleware,
  response::{IntoResponse, Response},
  routing::{any, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
  net::SocketAddr,
  path::PathBuf,
  sync::{Arc, Mutex},
  time::Duration,
};
use tokio::{sync::Semaphore, time::Instant};
use tokio_util::sync::CancellationToken;
use url::Url;
use zeroize::Zeroizing;

pub struct PairedClientConfig {
  pub hub_url: Url,
  pub state_dir: PathBuf,
  pub bind: SocketAddr,
  pub web_root: PathBuf,
  pub insecure_loopback: bool,
  pub host_id: Option<String>,
}

#[derive(Clone)]
struct ClientState {
  hub_url: Url,
  insecure_loopback: bool,
  store: Arc<ClientStore>,
  identity: Arc<NoiseIdentity>,
  boundary: LocalBoundary,
  active: Arc<Mutex<Option<ActiveHost>>>,
  requests: Arc<Semaphore>,
  changes: Arc<Semaphore>,
  shutdown: CancellationToken,
}

#[derive(Clone)]
struct ActiveHost {
  host: SavedHost,
  connection_id: String,
  endpoint: Url,
  cancelled: CancellationToken,
}

#[derive(Clone, Serialize)]
struct Selection {
  host_id: String,
  connection_id: String,
}

impl ActiveHost {
  fn selection(&self) -> Selection {
    Selection {
      host_id: self.host.host_id.clone(),
      connection_id: self.connection_id.clone(),
    }
  }
}

impl ClientState {
  fn select(&self, host_id: &str) -> Result<Selection, String> {
    let host = self
      .store
      .hosts()?
      .into_iter()
      .find(|host| host.host_id == host_id)
      .ok_or("This host has not been paired on this client")?;
    let endpoint = secure_client::secure_endpoint(&self.hub_url, host_id, self.insecure_loopback)?;
    let mut active = self.active.lock().map_err(|_| "Host selection is unavailable")?;
    self.store.select_host(host_id)?;
    let next = ActiveHost {
      host,
      connection_id: protocol::encode(&rand::random::<[u8; 24]>()),
      endpoint,
      cancelled: self.shutdown.child_token(),
    };
    let selection = next.selection();
    if let Some(previous) = active.replace(next) {
      previous.cancelled.cancel();
    }
    Ok(selection)
  }

  fn disconnect(&self) -> Result<(), String> {
    if let Some(previous) = self.active.lock().map_err(|_| "Host selection is unavailable")?.take() {
      previous.cancelled.cancel();
    }
    Ok(())
  }
}

pub async fn run_paired(config: PairedClientConfig, shutdown: CancellationToken) -> Result<(), String> {
  if !config.bind.ip().is_loopback() {
    return Err("The secure viewer client must bind to a numeric loopback address".into());
  }
  // Validate the Hub before creating any local state; routing IDs are public.
  secure_client::secure_endpoint(&config.hub_url, "validate", config.insecure_loopback)?;
  let store = Arc::new(ClientStore::load_or_create(&config.state_dir, &config.hub_url)?);
  let identity = Arc::new(NoiseIdentity::load_or_create(&store.identity_file())?);
  let listener = tokio::net::TcpListener::bind(config.bind)
    .await
    .map_err(|e| e.to_string())?;
  let boundary = LocalBoundary::new(listener.local_addr().map_err(|e| e.to_string())?);
  let state = ClientState {
    hub_url: config.hub_url,
    insecure_loopback: config.insecure_loopback,
    store,
    identity,
    boundary,
    active: Arc::new(Mutex::new(None)),
    requests: Arc::new(Semaphore::new(protocol::MAX_REQUESTS)),
    changes: Arc::new(Semaphore::new(1)),
    shutdown: shutdown.clone(),
  };
  if let Some(host_id) = config.host_id.or(state.store.selected_host()?) {
    state.select(&host_id)?;
  }
  let app = router(state.clone(), config.web_root)?;
  eprintln!(
    "Open your hosts: {}/connect#token={}",
    state.boundary.origin, state.boundary.token
  );
  axum::serve(listener, app)
    .with_graceful_shutdown(shutdown.cancelled_owned())
    .await
    .map_err(|e| e.to_string())
}

fn router(state: ClientState, web_root: PathBuf) -> Result<Router, String> {
  let app = Router::new()
    .route("/api/local/status", get(status))
    .route("/api/local/pair", post(pair))
    .route("/api/local/select", post(select))
    .route("/api/local/disconnect", post(disconnect))
    .route("/paired/{connection_id}/api/v1/{command}", any(proxy))
    .route("/api/{*path}", any(not_found))
    .with_state(state.clone())
    .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY));
  Ok(
    server::with_web_ui(app, web_root)?.layer(middleware::from_fn_with_state(
      state.boundary,
      secure_client::protect_origin,
    )),
  )
}

async fn not_found() -> Response {
  error(StatusCode::NOT_FOUND, "Unknown local client route")
}

async fn status(State(state): State<ClientState>) -> Response {
  let hosts = match state.store.hosts() {
    Ok(hosts) => hosts,
    Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, message),
  };
  let active = match state.active.lock() {
    Ok(active) => active.as_ref().map(ActiveHost::selection),
    Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Host selection is unavailable"),
  };
  Json(json!({ "hosts": hosts, "selected": active, "hub_url": state.hub_url.as_str() })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectRequest {
  host_id: String,
}

async fn select(State(state): State<ClientState>, Json(request): Json<SelectRequest>) -> Response {
  let Ok(_permit) = state.changes.try_acquire() else {
    return error(StatusCode::CONFLICT, "Another pairing or host change is in progress");
  };
  match state.select(&request.host_id) {
    Ok(selected) => Json(json!({ "selected": selected })).into_response(),
    Err(message) => error(StatusCode::BAD_REQUEST, message),
  }
}

async fn disconnect(State(state): State<ClientState>) -> Response {
  let Ok(_permit) = state.changes.try_acquire() else {
    return error(StatusCode::CONFLICT, "Another pairing or host change is in progress");
  };
  match state.disconnect() {
    Ok(()) => StatusCode::NO_CONTENT.into_response(),
    Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, message),
  }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairRequest {
  host_id: String,
  code: String,
}

async fn pair(State(state): State<ClientState>, Json(request): Json<PairRequest>) -> Response {
  let code = Zeroizing::new(request.code);
  let host_id = request.host_id;
  if uuid::Uuid::parse_str(&host_id).is_err() || code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
    return error(
      StatusCode::BAD_REQUEST,
      "Enter the host UUID and six-digit authenticator code",
    );
  }
  let Ok(_permit) = state.changes.try_acquire() else {
    return error(StatusCode::CONFLICT, "Another pairing or host change is in progress");
  };
  match state.store.hosts() {
    Ok(hosts) if hosts.iter().any(|host| host.host_id == host_id) => {
      return error(
        StatusCode::CONFLICT,
        "This host is already paired. Open it from your saved hosts",
      );
    }
    Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, message),
    _ => {}
  }
  let result = tokio::select! {
    _ = state.shutdown.cancelled() => Err("The local client stopped".into()),
    result = perform_pairing(&state, &host_id, &code) => result,
  };
  match result {
    Ok(host) => {
      // Save the authenticated pin only after the host's final confirmation.
      // Persistence refuses any replacement of an existing host's pin.
      if let Err(message) = state.store.save_host(host) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, message);
      }
      match state.select(&host_id) {
        Ok(selected) => Json(json!({ "selected": selected })).into_response(),
        Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, message),
      }
    }
    Err(message) => error(StatusCode::BAD_GATEWAY, message),
  }
}

async fn perform_pairing(state: &ClientState, host_id: &str, code: &str) -> Result<SavedHost, String> {
  let endpoint = secure_client::secure_endpoint(&state.hub_url, host_id, state.insecure_loopback)?;
  let mut socket = secure_client::connect_endpoint(&endpoint).await?;
  let (pairing, first) = ClientPairing::start(host_id, &state.identity, code, secure_client::unix_time()?)?;
  secure_client::send_record(&mut socket, first).await?;
  let reply = secure_client::receive_record(&mut socket, Instant::now() + Duration::from_secs(15))
    .await.map_err(|_| "Pairing did not complete. Check that the host is online, then wait for a fresh authenticator code before retrying")?;
  let (pairing, confirmation) = pairing.confirm(&reply).map_err(|_| {
    "Pairing was not authenticated. Check the host UUID and authenticator entry, wait for a fresh code, and check both devices' clocks"
  })?;
  secure_client::send_record(&mut socket, confirmation).await?;
  let ack = secure_client::receive_record(&mut socket, Instant::now() + Duration::from_secs(15))
    .await
    .map_err(|_| "The host did not confirm pairing. Wait for a fresh authenticator code before retrying")?;
  let paired = pairing.finish(&ack)?;
  Ok(SavedHost {
    host_id: paired.host_id,
    host_public_key: paired.host_public_key,
    machine_address: None,
    name: None,
  })
}

async fn proxy(
  State(state): State<ClientState>,
  Path((connection_id, command)): Path<(String, String)>,
  mut request: Request,
) -> Response {
  let target = match state.active.lock() {
    Ok(active) => match active.as_ref() {
      Some(active) if active.connection_id == connection_id => active.clone(),
      _ => {
        return error(
          StatusCode::CONFLICT,
          "This host connection changed. Open it again from your saved hosts",
        );
      }
    },
    Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Host selection is unavailable"),
  };
  // Every browser connection addresses one immutable selection. Requests from
  // old tabs never silently follow a new host, even if they arrive after switch.
  let path = match request.uri().query() {
    Some(query) => format!("/api/v1/{command}?{query}"),
    None => format!("/api/v1/{command}"),
  };
  let Ok(uri) = path.parse::<Uri>() else {
    return error(StatusCode::BAD_REQUEST, "Invalid viewer route");
  };
  *request.uri_mut() = uri;
  secure_client::proxy_paired(
    PairedRequest {
      endpoint: target.endpoint,
      host_public_key: target.host.host_public_key,
      identity: state.identity,
      requests: state.requests,
      shutdown: target.cancelled,
    },
    request,
  )
  .await
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    pairing::{HostPairing, TotpSecret, is_pairing_record},
    secure::{InnerMessage, NoiseResponder},
  };
  use axum::{
    body::Body,
    extract::{WebSocketUpgrade, ws::Message},
    http::header,
  };
  use http_body_util::BodyExt;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use tower::ServiceExt;

  const HOST_A: &str = "11111111-1111-4111-8111-111111111111";
  const HOST_B: &str = "22222222-2222-4222-8222-222222222222";

  fn state(directory: &std::path::Path, hub_url: Url) -> ClientState {
    let store = Arc::new(ClientStore::load_or_create(directory, &hub_url).unwrap());
    let identity = Arc::new(NoiseIdentity::load_or_create(&store.identity_file()).unwrap());
    ClientState {
      hub_url,
      insecure_loopback: true,
      store,
      identity,
      boundary: LocalBoundary {
        authority: "127.0.0.1:5555".into(),
        origin: "http://127.0.0.1:5555".into(),
        token: "local-token".into(),
      },
      active: Arc::new(Mutex::new(None)),
      requests: Arc::new(Semaphore::new(4)),
      changes: Arc::new(Semaphore::new(1)),
      shutdown: CancellationToken::new(),
    }
  }

  fn local_request(path: &str, host: &str, origin: Option<&str>, token: Option<&str>) -> Request {
    let mut request = Request::builder()
      .method("POST")
      .uri(path)
      .header(header::HOST, host)
      .header(header::CONTENT_TYPE, "application/json");
    if let Some(origin) = origin {
      request = request.header(header::ORIGIN, origin);
    }
    if let Some(token) = token {
      request = request.header(header::AUTHORIZATION, token);
    }
    request
      .body(Body::from(
        r#"{"host_id":"11111111-1111-4111-8111-111111111111","code":"123456"}"#,
      ))
      .unwrap()
  }

  #[tokio::test]
  async fn pairing_mutations_require_exact_local_origin_host_and_bearer() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("index.html"), "local viewer").unwrap();
    let state = state(directory.path(), Url::parse("http://127.0.0.1:9").unwrap());
    let app = router(state, directory.path().into()).unwrap();
    for (host, origin, token, expected) in [
      (
        "attacker.example",
        None,
        Some("Bearer local-token"),
        StatusCode::FORBIDDEN,
      ),
      (
        "127.0.0.1:5555",
        Some("https://attacker.example"),
        Some("Bearer local-token"),
        StatusCode::FORBIDDEN,
      ),
      (
        "127.0.0.1:5555",
        Some("null"),
        Some("Bearer local-token"),
        StatusCode::FORBIDDEN,
      ),
      ("127.0.0.1:5555", None, None, StatusCode::UNAUTHORIZED),
      ("127.0.0.1:5555", None, Some("Bearer wrong"), StatusCode::UNAUTHORIZED),
    ] {
      let response = app
        .clone()
        .oneshot(local_request("/api/local/pair", host, origin, token))
        .await
        .unwrap();
      assert_eq!(response.status(), expected);
    }
    let response = app
      .oneshot(local_request(
        "/paired/stale/api/v1/health",
        "127.0.0.1:5555",
        None,
        None,
      ))
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
  }

  #[tokio::test]
  async fn switching_cancels_previous_streams_and_old_routes_cannot_follow_the_new_host() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path(), Url::parse("http://127.0.0.1:9").unwrap());
    for host_id in [HOST_A, HOST_B] {
      state
        .store
        .save_host(SavedHost {
          host_id: host_id.into(),
          host_public_key: NoiseIdentity::generate().unwrap().public_key(),
          machine_address: None,
          name: None,
        })
        .unwrap();
    }
    let first = state.select(HOST_A).unwrap();
    let cancellation = state.active.lock().unwrap().as_ref().unwrap().cancelled.clone();
    let second = state.select(HOST_B).unwrap();
    assert!(cancellation.is_cancelled());
    assert_ne!(first.connection_id, second.connection_id);
    let response = proxy(
      State(state.clone()),
      Path((first.connection_id, "health".into())),
      Request::new(Body::empty()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(state.store.selected_host().unwrap().as_deref(), Some(HOST_B));
    let cancellation = state.active.lock().unwrap().as_ref().unwrap().cancelled.clone();
    state.disconnect().unwrap();
    assert!(cancellation.is_cancelled());
  }

  #[tokio::test]
  async fn successful_pairing_persists_pin_and_restart_uses_noise_without_another_code() {
    let directory = tempfile::tempdir().unwrap();
    let host = Arc::new(NoiseIdentity::generate().unwrap());
    let secret = Arc::new(TotpSecret::generate());
    let pairing_count = Arc::new(AtomicUsize::new(0));
    let noise_count = Arc::new(AtomicUsize::new(0));
    let authorized = Arc::new(Mutex::new(None::<String>));
    let app = Router::new().route(
      &format!("/hub/v1/secure/{HOST_A}"),
      get({
        let host = host.clone();
        let secret = secret.clone();
        let pairing_count = pairing_count.clone();
        let noise_count = noise_count.clone();
        let authorized = authorized.clone();
        move |upgrade: WebSocketUpgrade| {
          let host = host.clone();
          let secret = secret.clone();
          let pairing_count = pairing_count.clone();
          let noise_count = noise_count.clone();
          let authorized = authorized.clone();
          async move {
            upgrade.on_upgrade(move |mut socket| async move {
              let first = socket.recv().await.unwrap().unwrap().into_data();
              if is_pairing_record(&first) {
                pairing_count.fetch_add(1, Ordering::SeqCst);
                let (pending, reply) =
                  HostPairing::respond(&secret, HOST_A, &host, &first, secure_client::unix_time().unwrap()).unwrap();
                socket.send(Message::Binary(reply.into())).await.unwrap();
                // A wrong code is rejected by the client before it confirms.
                let Some(Ok(Message::Binary(confirmation))) = socket.recv().await else {
                  return;
                };
                let (paired, ack) = pending
                  .finish(&confirmation, secure_client::unix_time().unwrap())
                  .unwrap();
                *authorized.lock().unwrap() = Some(paired.client_public_key);
                socket.send(Message::Binary(ack.into())).await.unwrap();
                return;
              }
              noise_count.fetch_add(1, Ordering::SeqCst);
              let (reply, mut channel) = NoiseResponder::new(&host).unwrap().accept(&first).unwrap();
              assert_eq!(authorized.lock().unwrap().as_deref(), Some(channel.remote_public_key()));
              socket.send(Message::Binary(reply.into())).await.unwrap();
              let request = socket.recv().await.unwrap().unwrap().into_data();
              assert_eq!(
                channel.decrypt(&request).unwrap(),
                InnerMessage::DeviceRequest {
                  method: "GET".into(),
                  path: "/api/v1/health".into()
                }
              );
              let end = socket.recv().await.unwrap().unwrap().into_data();
              assert_eq!(channel.decrypt(&end).unwrap(), InnerMessage::RequestEnd {});
              for message in [
                InnerMessage::Response {
                  status: 200,
                  content_type: Some("application/json".into()),
                },
                InnerMessage::Chunk {
                  data: protocol::encode(br#"{"version":1}"#),
                },
                InnerMessage::End {},
              ] {
                socket
                  .send(Message::Binary(channel.encrypt(&message).unwrap().into()))
                  .await
                  .unwrap();
              }
              while let Some(Ok(_)) = socket.recv().await {}
            })
          }
        }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = state(directory.path(), hub_url.clone());
    let actual = secret.code_at(secure_client::unix_time().unwrap());
    let wrong = if actual == "000000" { "111111" } else { "000000" };
    let response = pair(
      State(state.clone()),
      Json(PairRequest {
        host_id: HOST_A.into(),
        code: wrong.into(),
      }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(state.store.hosts().unwrap().is_empty());
    let response = pair(
      State(state.clone()),
      Json(PairRequest {
        host_id: HOST_A.into(),
        code: secret.code_at(secure_client::unix_time().unwrap()),
      }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
      state.store.hosts().unwrap(),
      vec![SavedHost {
        host_id: HOST_A.into(),
        host_public_key: host.public_key(),
        machine_address: None,
        name: None,
      }]
    );
    assert!(
      state
        .store
        .save_host(SavedHost {
          host_id: HOST_A.into(),
          host_public_key: NoiseIdentity::generate().unwrap().public_key(),
          machine_address: None,
          name: None,
        })
        .is_err()
    );
    let restarted = self::state(directory.path(), hub_url);
    assert_eq!(restarted.identity.public_key(), state.identity.public_key());
    let selected = restarted
      .select(&restarted.store.selected_host().unwrap().unwrap())
      .unwrap();
    let request = Request::builder()
      .method("GET")
      .uri("/api/v1/health")
      .body(Body::empty())
      .unwrap();
    let response = proxy(
      State(restarted),
      Path((selected.connection_id, "health".into())),
      request,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, br#"{"version":1}"#.as_slice());
    assert_eq!(pairing_count.load(Ordering::SeqCst), 2);
    assert_eq!(noise_count.load(Ordering::SeqCst), 1);
    server.abort();
  }
}
