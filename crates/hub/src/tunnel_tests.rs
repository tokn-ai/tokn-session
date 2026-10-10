use super::*;
use crate::connector::{self, ConnectorConfig};
use axum::{
  Router,
  extract::{Path, State, WebSocketUpgrade},
  routing::{get, post},
};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};

async fn serve(app: Router) -> (url::Url, JoinHandle<()>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let url = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
  let task = tokio::spawn(async move {
    axum::serve(listener, app).await.unwrap();
  });
  (url, task)
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
  tokio::time::timeout(Duration::from_secs(5), async {
    while !predicate() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("condition timed out");
}

async fn hub(tunnels: HubTunnels) -> (url::Url, JoinHandle<()>) {
  serve(
    Router::new()
      .route(
        protocol::TUNNEL_PATH,
        get(|State(tunnels): State<HubTunnels>, ws: WebSocketUpgrade| async move {
          ws.max_message_size(protocol::MAX_FRAME)
            .on_upgrade(move |socket| async move {
              tunnels.handle_socket(socket).await;
            })
        }),
      )
      .route(
        "/hub/v1/secure/{host_id}",
        get(
          |State(tunnels): State<HubTunnels>, Path(host_id): Path<String>, ws: WebSocketUpgrade| async move {
            let Ok(permit) = tunnels.reserve_secure_channel() else {
              return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::empty())
                .unwrap();
            };
            ws.max_message_size(protocol::MAX_SECURE_RECORD)
              .on_upgrade(move |socket| async move {
                tunnels.handle_secure_socket(&host_id, socket, permit).await;
              })
          },
        ),
      )
      .with_state(tunnels),
  )
  .await
}

struct StreamGuard(Arc<AtomicBool>);
impl Drop for StreamGuard {
  fn drop(&mut self) {
    self.0.store(false, Ordering::SeqCst);
  }
}

#[tokio::test]
async fn enrollment_streaming_cancellation_reconnect_and_revocation() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store.clone());
  let (hub_url, hub_task) = hub(tunnels.clone()).await;
  let streaming = Arc::new(AtomicBool::new(false));
  let stream_state = streaming.clone();
  let local = Router::new()
    .route(
      "/api/v1/health",
      get(|| async { ([("content-type", "text/html")], "<script>bad()</script>") }),
    )
    .route("/api/v1/list_sessions", post(|| async { "x".repeat(2 * 1024 * 1024) }))
    .route(
      "/api/v1/events",
      get(move || {
        let streaming = stream_state.clone();
        async move {
          let stream = async_stream::stream! {
            let _guard = StreamGuard(streaming.clone());
            streaming.store(true, Ordering::SeqCst);
            yield Ok::<_, io::Error>(Bytes::from_static(b"event: ready\ndata: {}\n\n"));
            std::future::pending::<()>().await;
          };
          Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
        }
      }),
    );
  let (local_url, local_task) = serve(local).await;
  let mut config = ConnectorConfig {
    hub_url,
    local_url,
    key_file: directory.path().join("host.json"),
    name: "test host".into(),
    local_token: None,
    allow_control: false,
    insecure_loopback: true,
    secure: None,
    paired: None,
  };
  let stop = CancellationToken::new();
  let task = tokio::spawn(connector::run(config.clone(), stop.clone()));
  wait_for(|| !tunnels.pending().is_empty()).await;
  assert!(store.hosts().unwrap().is_empty());
  let pending = tunnels.pending().remove(0);
  let record = tunnels.approve(&pending.pairing_code).unwrap();
  assert!(
    tunnels.approve(&pending.pairing_code).is_err(),
    "pairing codes are single use"
  );
  wait_for(|| tunnels.online(&record.host_id)).await;

  let response = tunnels
    .proxy(&record.host_id, Method::GET, "/api/v1/health", Bytes::new())
    .await
    .ok()
    .unwrap();
  assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
  assert_eq!(response.headers()["x-content-type-options"], "nosniff");
  let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
  assert_eq!(&body[..], b"<script>bad()</script>");
  let response = tunnels
    .proxy(
      &record.host_id,
      Method::POST,
      "/api/v1/list_sessions",
      Bytes::from_static(b"{}"),
    )
    .await
    .ok()
    .unwrap();
  // More than one flow-control window must arrive intact, without spooling the
  // entire body in either process or starving heartbeat/cancellation traffic.
  let body = axum::body::to_bytes(response.into_body(), 3 * 1024 * 1024)
    .await
    .unwrap();
  assert_eq!(body.len(), 2 * 1024 * 1024);
  // A client that stops reading must not block requests on other streams.
  let stalled = tunnels
    .proxy(
      &record.host_id,
      Method::POST,
      "/api/v1/list_sessions",
      Bytes::from_static(b"{}"),
    )
    .await
    .ok()
    .unwrap();
  let other = tokio::time::timeout(
    Duration::from_secs(2),
    tunnels.proxy(&record.host_id, Method::GET, "/api/v1/health", Bytes::new()),
  )
  .await
  .unwrap()
  .ok()
  .unwrap();
  axum::body::to_bytes(other.into_body(), 1024).await.unwrap();
  drop(stalled);
  let denied = tunnels
    .proxy(
      &record.host_id,
      Method::POST,
      "/api/v1/submit_session_input",
      Bytes::new(),
    )
    .await
    .err()
    .unwrap();
  assert_eq!(denied.status, StatusCode::FORBIDDEN);

  let response = tunnels
    .proxy(&record.host_id, Method::GET, "/api/v1/events", Bytes::new())
    .await
    .ok()
    .unwrap();
  let mut body = response.into_body();
  let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
  assert!(String::from_utf8_lossy(&frame).contains("ready"));
  assert!(streaming.load(Ordering::SeqCst));
  drop(body);
  wait_for(|| !streaming.load(Ordering::SeqCst)).await;

  stop.cancel();
  task.await.unwrap().unwrap();
  wait_for(|| !tunnels.online(&record.host_id)).await;
  // The same host key reconnects without pairing, but changing its local flag
  // cannot exceed the access approved during the original enrollment.
  config.allow_control = true;
  let stop = CancellationToken::new();
  let task = tokio::spawn(connector::run(config, stop.clone()));
  wait_for(|| tunnels.online(&record.host_id)).await;
  assert!(tunnels.pending().is_empty());
  assert_eq!(tunnels.access(&record.host_id).as_deref(), Some("view"));
  let response = tunnels
    .proxy(&record.host_id, Method::GET, "/api/v1/events", Bytes::new())
    .await
    .ok()
    .unwrap();
  let mut body = response.into_body();
  body.frame().await.unwrap().unwrap();
  tunnels.revoke(&record.host_id).unwrap();
  assert!(!tunnels.online(&record.host_id));
  assert!(store.host(&record.host_id).unwrap().is_none());
  assert!(
    body.frame().await.unwrap().is_err(),
    "revocation terminates an established stream"
  );
  wait_for(|| !streaming.load(Ordering::SeqCst)).await;
  wait_for(|| !tunnels.pending().is_empty()).await;
  assert!(
    !tunnels.online(&record.host_id),
    "revocation requires a fresh owner approval"
  );
  stop.cancel();
  task.await.unwrap().unwrap();
  tunnels.shutdown();
  hub_task.abort();
  local_task.abort();
}

#[tokio::test]
async fn signatures_bind_fresh_challenge_identity_name_and_permission() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store);
  let (mut endpoint, task) = hub(tunnels.clone()).await;
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(protocol::TUNNEL_PATH);
  let key = SigningKey::generate(&mut OsRng);
  let public_key = protocol::encode(key.verifying_key().as_bytes());
  let mut previous_nonce = None;
  for attempt in 0..4 {
    let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
    let ClientMessage::Text(text) = socket.next().await.unwrap().unwrap() else {
      panic!("expected challenge")
    };
    let Frame::Challenge { nonce, .. } = serde_json::from_str(&text).unwrap() else {
      panic!("expected challenge")
    };
    let signed_nonce = if attempt == 1 {
      previous_nonce.as_ref().unwrap()
    } else {
      &nonce
    };
    let signing_key = if attempt == 0 {
      SigningKey::generate(&mut OsRng)
    } else {
      key.clone()
    };
    let signature = signing_key.sign(&protocol::proof(signed_nonce, &public_key, "Host", false));
    previous_nonce = Some(nonce);
    let frame = Frame::Authenticate {
      version: protocol::VERSION,
      public_key: public_key.clone(),
      name: "Host".into(),
      allow_control: attempt == 2,
      signature: protocol::encode(&signature.to_bytes()),
    };
    socket
      .send(ClientMessage::Text(serde_json::to_string(&frame).unwrap().into()))
      .await
      .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(2), socket.next())
      .await
      .unwrap();
    if attempt < 3 {
      assert!(
        !matches!(reply, Some(Ok(ClientMessage::Text(_)))),
        "invalid proof must not enter enrollment"
      );
      assert!(tunnels.pending().is_empty());
    } else {
      let Some(Ok(ClientMessage::Text(text))) = reply else {
        panic!("expected pairing")
      };
      assert!(matches!(
        serde_json::from_str::<Frame>(&text).unwrap(),
        Frame::Pending { .. }
      ));
      assert_eq!(tunnels.pending().len(), 1);
    }
  }
  tunnels.shutdown();
  task.abort();
}

#[tokio::test]
async fn encrypted_registration_proof_binds_uuid_and_reports_replacement_or_revocation() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store.clone());
  let (mut endpoint, task) = hub(tunnels.clone()).await;
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(protocol::TUNNEL_PATH);
  let host_id = uuid::Uuid::new_v4().to_string();
  let key = SigningKey::generate(&mut OsRng);
  for attempt in 0..5 {
    let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
    let Some(Frame::Challenge { nonce, .. }) = next_frame(&mut socket).await else {
      panic!("challenge missing")
    };
    let signing_key = if attempt == 3 {
      SigningKey::generate(&mut OsRng)
    } else {
      key.clone()
    };
    let public_key = protocol::encode(signing_key.verifying_key().as_bytes());
    let proof = if attempt == 0 {
      protocol::proof(&nonce, &public_key, "Host", false)
    } else {
      protocol::registration_proof(&nonce, &host_id, &public_key, "Host", false)
    };
    let frame = Frame::Register {
      version: protocol::VERSION,
      host_id: if attempt == 1 {
        uuid::Uuid::new_v4().to_string()
      } else {
        host_id.clone()
      },
      public_key,
      name: "Host".into(),
      allow_control: false,
      signature: protocol::encode(&signing_key.sign(&proof).to_bytes()),
    };
    socket
      .send(ClientMessage::Text(serde_json::to_string(&frame).unwrap().into()))
      .await
      .unwrap();
    let reply = next_frame(&mut socket).await;
    if attempt == 2 {
      assert!(matches!(reply, Some(Frame::Ready { host_id: registered, .. }) if registered == host_id));
      assert!(
        tunnels.secure_only(&host_id),
        "secure-only is effective before any host advisory"
      );
    } else if attempt >= 3 {
      assert!(
        matches!(reply, Some(Frame::RegistrationRejected { message }) if message.contains("revoked or belongs to a different identity")),
        "proved registration receives a recovery diagnostic without authorizing the rejected identity"
      );
    } else {
      assert!(
        reply.is_none(),
        "legacy proof and UUID substitution must fail before registration"
      );
    }
    if attempt == 3 {
      tunnels.revoke(&host_id).unwrap();
    }
    assert!(tunnels.pending().is_empty());
  }
  assert!(store.hosts().unwrap().is_empty());
  tunnels.shutdown();
  task.abort();
}

type TestSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[test]
fn registration_diagnostics_never_expose_database_details() {
  let diagnostic = registration_diagnostic("Hub database: failed to open /private/example.sqlite");
  assert_eq!(
    diagnostic,
    "Hub could not register this host; contact its administrator"
  );
  assert_eq!(
    registration_diagnostic("New host registration rate limit reached; retry in one minute"),
    "New host registration rate limit reached; retry in one minute"
  );
}

async fn signed_socket(endpoint: &url::Url, key: &SigningKey) -> TestSocket {
  let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
  let Some(Frame::Challenge { nonce, .. }) = next_frame(&mut socket).await else {
    panic!("expected challenge")
  };
  let public_key = protocol::encode(key.verifying_key().as_bytes());
  let signature = key.sign(&protocol::proof(&nonce, &public_key, "Host", false));
  let frame = Frame::Authenticate {
    version: protocol::VERSION,
    public_key,
    name: "Host".into(),
    allow_control: false,
    signature: protocol::encode(&signature.to_bytes()),
  };
  socket
    .send(ClientMessage::Text(serde_json::to_string(&frame).unwrap().into()))
    .await
    .unwrap();
  socket
}

async fn next_frame(socket: &mut TestSocket) -> Option<Frame> {
  tokio::time::timeout(Duration::from_secs(2), async {
    loop {
      match socket.next().await {
        Some(Ok(ClientMessage::Text(text))) => return Some(serde_json::from_str(&text).unwrap()),
        Some(Ok(ClientMessage::Ping(data))) => {
          let _ = socket.send(ClientMessage::Pong(data)).await;
        }
        Some(Ok(ClientMessage::Pong(_))) => {}
        _ => return None,
      }
    }
  })
  .await
  .expect("tunnel frame timed out")
}

#[tokio::test]
async fn pending_enrollments_cannot_exhaust_approved_reconnection_capacity() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let approved_key = SigningKey::generate(&mut OsRng);
  let host_id = protocol::host_id(approved_key.verifying_key().as_bytes());
  store
    .approve_host(HostRecord {
      host_id: host_id.clone(),
      name: "Approved".into(),
      public_key: protocol::encode(approved_key.verifying_key().as_bytes()),
      access: "view".into(),
    })
    .unwrap();
  let tunnels = HubTunnels::with_limits(store, 1, 2, 1);
  let (mut endpoint, task) = hub(tunnels.clone()).await;
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(protocol::TUNNEL_PATH);

  let mut pending_sockets = Vec::new();
  for _ in 0..2 {
    let mut socket = signed_socket(&endpoint, &SigningKey::generate(&mut OsRng)).await;
    assert!(matches!(next_frame(&mut socket).await, Some(Frame::Pending { .. })));
    pending_sockets.push(socket);
  }
  assert_eq!(tunnels.pending().len(), 2);
  assert_eq!(tunnels.enrollments.available_permits(), 0);
  assert_eq!(tunnels.handshakes.available_permits(), 1);
  assert_eq!(tunnels.approved.available_permits(), 1);
  let mut overflow = signed_socket(&endpoint, &SigningKey::generate(&mut OsRng)).await;
  assert!(
    next_frame(&mut overflow).await.is_none(),
    "unknown hosts cannot exceed the pending budget"
  );

  let mut approved = signed_socket(&endpoint, &approved_key).await;
  assert!(matches!(next_frame(&mut approved).await, Some(Frame::Ready { .. })));
  assert!(tunnels.online(&host_id));
  assert_eq!(tunnels.approved.available_permits(), 0);
  // All long-lived budgets are full. An approved identity can still replace
  // its own connection, and closing the old socket must not release its slot.
  let mut replacement = signed_socket(&endpoint, &approved_key).await;
  assert!(matches!(next_frame(&mut replacement).await, Some(Frame::Ready { .. })));
  assert!(next_frame(&mut approved).await.is_none());
  assert!(tunnels.online(&host_id));
  assert_eq!(tunnels.approved.available_permits(), 0);
  assert_eq!(tunnels.pending().len(), 2);
  drop(replacement);
  wait_for(|| !tunnels.online(&host_id) && tunnels.approved.available_permits() == 1).await;
  drop(pending_sockets);
  wait_for(|| tunnels.enrollments.available_permits() == 2).await;
  tunnels.shutdown();
  task.abort();
}

#[tokio::test]
async fn accepted_agent_input_is_not_replayed_after_tunnel_reconnect() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store);
  let (hub_url, hub_task) = hub(tunnels.clone()).await;
  let accepted_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let count = accepted_count.clone();
  let (accepted, mut acceptances) = mpsc::channel(8);
  let local = Router::new().route("/api/v1/health", get(|| async { "{}" })).route(
    "/api/v1/submit_session_input",
    post(move || {
      let count = count.clone();
      let accepted = accepted.clone();
      async move {
        count.fetch_add(1, Ordering::SeqCst);
        let _ = accepted.send(()).await;
        // The agent has accepted the input, but the caller never receives a
        // response establishing whether it was delivered.
        std::future::pending::<()>().await;
        "{}"
      }
    }),
  );
  let (local_url, local_task) = serve(local).await;
  let config = ConnectorConfig {
    hub_url,
    local_url,
    key_file: directory.path().join("host.json"),
    name: "Input host".into(),
    local_token: None,
    allow_control: true,
    insecure_loopback: true,
    secure: None,
    paired: None,
  };
  let stop = CancellationToken::new();
  let connector_task = tokio::spawn(connector::run(config, stop.clone()));
  wait_for(|| !tunnels.pending().is_empty()).await;
  let pending = tunnels.pending().remove(0);
  let record = tunnels.approve(&pending.pairing_code).unwrap();
  wait_for(|| tunnels.online(&record.host_id)).await;
  let old_connection = tunnels.inner.lock().unwrap().online[&record.host_id].clone();
  let proxy = tunnels.clone();
  let host_id = record.host_id.clone();
  let request = tokio::spawn(async move {
    proxy
      .proxy(
        &host_id,
        Method::POST,
        "/api/v1/submit_session_input",
        Bytes::from_static(br#"{"request":{"text":"execute once"}}"#),
      )
      .await
  });
  tokio::time::timeout(Duration::from_secs(2), acceptances.recv())
    .await
    .unwrap()
    .unwrap();
  assert_eq!(accepted_count.load(Ordering::SeqCst), 1);
  old_connection.shutdown.cancel();
  let failure = tokio::time::timeout(Duration::from_secs(2), request)
    .await
    .unwrap()
    .unwrap()
    .err()
    .unwrap();
  assert_eq!(failure.status, StatusCode::BAD_GATEWAY);
  assert!(failure.message.contains("never replayed"));

  // Exercise the production reconnect loop with the same approved identity.
  // A fresh successful request proves its new tunnel is usable.
  wait_for(|| tunnels.online(&record.host_id)).await;
  assert!(tunnels.pending().is_empty());
  let response = tunnels
    .proxy(&record.host_id, Method::GET, "/api/v1/health", Bytes::new())
    .await
    .ok()
    .unwrap();
  assert_eq!(axum::body::to_bytes(response.into_body(), 1024).await.unwrap(), "{}");
  assert!(
    tokio::time::timeout(Duration::from_millis(200), acceptances.recv())
      .await
      .is_err(),
    "uncertain input must not be retried by HTTP or by tunnel reconnect"
  );
  assert_eq!(accepted_count.load(Ordering::SeqCst), 1);
  stop.cancel();
  connector_task.await.unwrap().unwrap();
  tunnels.shutdown();
  hub_task.abort();
  local_task.abort();
}

async fn next_binary(socket: &mut TestSocket) -> Option<Vec<u8>> {
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      match socket.next().await {
        Some(Ok(ClientMessage::Binary(data))) => return Some(data.to_vec()),
        Some(Ok(ClientMessage::Ping(data))) => {
          let _ = socket.send(ClientMessage::Pong(data)).await;
        }
        Some(Ok(ClientMessage::Pong(_))) => {}
        _ => return None,
      }
    }
  })
  .await
  .expect("encrypted record timed out")
}

async fn encrypted_socket(
  endpoint: &url::Url,
  identity: &crate::secure::NoiseIdentity,
  host_public_key: &str,
) -> (TestSocket, crate::secure::SecureChannel) {
  let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
  let mut handshake = crate::secure::NoiseInitiator::new(identity, host_public_key).unwrap();
  socket
    .send(ClientMessage::Binary(handshake.start().unwrap().into()))
    .await
    .unwrap();
  let reply = next_binary(&mut socket).await.unwrap();
  (socket, handshake.finish(&reply).unwrap())
}

async fn send_inner(
  socket: &mut TestSocket,
  channel: &mut crate::secure::SecureChannel,
  message: &crate::secure::InnerMessage,
) {
  socket
    .send(ClientMessage::Binary(channel.encrypt(message).unwrap().into()))
    .await
    .unwrap();
}

async fn secure_request(
  endpoint: &url::Url,
  identity: &crate::secure::NoiseIdentity,
  grant: &crate::secure::SignedGrant,
  method: &str,
  path: &str,
  body: &[u8],
) -> (TestSocket, crate::secure::SecureChannel) {
  use crate::secure::InnerMessage;
  let (mut socket, mut channel) = encrypted_socket(endpoint, identity, &grant.grant.host_public_key).await;
  send_inner(
    &mut socket,
    &mut channel,
    &InnerMessage::Request {
      method: method.into(),
      path: path.into(),
      grant: grant.clone(),
    },
  )
  .await;
  for part in body.chunks(protocol::CHUNK_SIZE) {
    send_inner(
      &mut socket,
      &mut channel,
      &InnerMessage::RequestBody {
        data: protocol::encode(part),
      },
    )
    .await;
  }
  send_inner(&mut socket, &mut channel, &InnerMessage::RequestEnd {}).await;
  (socket, channel)
}

async fn device_request(
  endpoint: &url::Url,
  identity: &crate::secure::NoiseIdentity,
  host_public_key: &str,
  method: &str,
  path: &str,
  body: &[u8],
) -> (TestSocket, crate::secure::SecureChannel) {
  use crate::secure::InnerMessage;
  let (mut socket, mut channel) = encrypted_socket(endpoint, identity, host_public_key).await;
  send_inner(
    &mut socket,
    &mut channel,
    &InnerMessage::DeviceRequest {
      method: method.into(),
      path: path.into(),
    },
  )
  .await;
  for part in body.chunks(protocol::CHUNK_SIZE) {
    send_inner(
      &mut socket,
      &mut channel,
      &InnerMessage::RequestBody {
        data: protocol::encode(part),
      },
    )
    .await;
  }
  send_inner(&mut socket, &mut channel, &InnerMessage::RequestEnd {}).await;
  (socket, channel)
}

#[tokio::test]
async fn authenticator_pairing_registers_only_encrypted_hosts_and_persists_trust() {
  use crate::{
    connector::PairedHostConfig,
    pairing::{ClientPairing, TotpSecret},
    secure::{InnerMessage, NoiseIdentity},
  };
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store.clone());
  let (hub_url, hub_task) = hub(tunnels.clone()).await;
  let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let health_calls = calls.clone();
  let updates_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
  let forwarded_updates = updates_requests.clone();
  let updates_response = serde_json::json!({
    "subscription_id": "paired-updates",
    "session_key": "paired-session",
    "level": "all",
    "generation": "generation-one",
    "base_revision": null,
    "revision": "revision-one",
    "snapshot": true,
    "items": [{ "key": "reply-one", "type": "assistant_message", "text": "Updated reply" }],
    "groups": [],
    "semantic_order": ["reply-one"],
    "event_order": [],
    "removed_items": [],
    "item_order": ["reply-one"],
    "state": {}
  });
  let local_updates_response = updates_response.clone();
  let streaming = Arc::new(AtomicBool::new(false));
  let stream_state = streaming.clone();
  let local = Router::new()
    .route(
      "/api/v1/health",
      get(move || {
        health_calls.fetch_add(1, Ordering::SeqCst);
        async { "{}" }
      }),
    )
    .route(
      "/api/v1/load_session_updates",
      post(move |axum::Json(payload): axum::Json<serde_json::Value>| {
        forwarded_updates.lock().unwrap().push(payload);
        let response = local_updates_response.clone();
        async move { axum::Json(response) }
      }),
    )
    .route(
      "/api/v1/events",
      get(move || {
        let streaming = stream_state.clone();
        async move {
          let stream = async_stream::stream! {
            let _guard = StreamGuard(streaming.clone());
            streaming.store(true, Ordering::SeqCst);
            yield Ok::<_, io::Error>(Bytes::from_static(b"event: ready\ndata: {}\n\n"));
            std::future::pending::<()>().await;
          };
          Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
        }
      }),
    );
  let (local_url, local_task) = serve(local).await;
  let host_id = uuid::Uuid::new_v4().to_string();
  let noise_file = directory.path().join("host-noise.json");
  let host_identity = NoiseIdentity::load_or_create(&noise_file).unwrap();
  let state_file = directory.path().join("access.json");
  let secret = TotpSecret::generate();
  crate::onboarding::initialize_host_access(&state_file, &secret).unwrap();
  let config = ConnectorConfig {
    hub_url: hub_url.clone(),
    local_url,
    key_file: directory.path().join("enrollment.json"),
    name: "Paired host".into(),
    local_token: None,
    allow_control: false,
    insecure_loopback: true,
    secure: None,
    paired: Some(PairedHostConfig {
      host_id: host_id.clone(),
      noise_key_file: noise_file,
      state_file: state_file.clone(),
    }),
  };
  let stop = CancellationToken::new();
  let connector_task = tokio::spawn(connector::run(config.clone(), stop.clone()));
  wait_for(|| tunnels.online(&host_id)).await;
  assert!(tunnels.pending().is_empty());
  assert!(tunnels.secure_only(&host_id));
  assert_eq!(store.hosts().unwrap().len(), 1);
  assert_eq!(
    tunnels
      .proxy(&host_id, Method::GET, "/api/v1/health", Bytes::new())
      .await
      .err()
      .unwrap()
      .status,
    StatusCode::FORBIDDEN
  );
  let mut endpoint = hub_url;
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(&format!("/hub/v1/secure/{host_id}"));
  let recipient = NoiseIdentity::generate().unwrap();
  let (mut socket, mut channel) = encrypted_socket(&endpoint, &recipient, &host_identity.public_key()).await;
  send_inner(
    &mut socket,
    &mut channel,
    &InnerMessage::DeviceRequest {
      method: "GET".into(),
      path: "/api/v1/health".into(),
    },
  )
  .await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Error { .. }
  ));
  assert!(
    next_binary(&mut socket).await.is_none(),
    "knowing a host's public key does not authorize an unpaired device"
  );
  assert_eq!(calls.load(Ordering::SeqCst), 0);

  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  let code = secret.code_at(now);
  let (pending, hello) = ClientPairing::start(&host_id, &recipient, &code, now).unwrap();
  let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
  socket.send(ClientMessage::Binary(hello.into())).await.unwrap();
  let (waiting, confirm) = pending.confirm(&next_binary(&mut socket).await.unwrap()).unwrap();
  socket.send(ClientMessage::Binary(confirm.into())).await.unwrap();
  let paired = waiting.finish(&next_binary(&mut socket).await.unwrap()).unwrap();
  assert_eq!(paired.host_public_key, host_identity.public_key());
  assert_eq!(paired.client_public_key, recipient.public_key());
  assert!(crate::onboarding::is_authorized(&state_file, &recipient.public_key()).unwrap());
  drop(socket);

  // A fresh exchange with the accepted time step must fail, even with a new
  // client key and PAKE ephemeral. Authorization consumes the step locally.
  let other = NoiseIdentity::generate().unwrap();
  let (_, replay) = ClientPairing::start(&host_id, &other, &code, now).unwrap();
  let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
  socket.send(ClientMessage::Binary(replay.into())).await.unwrap();
  assert!(next_binary(&mut socket).await.is_none());
  assert!(!crate::onboarding::is_authorized(&state_file, &other.public_key()).unwrap());
  drop(socket);

  stop.cancel();
  connector_task.await.unwrap().unwrap();
  wait_for(|| !tunnels.online(&host_id)).await;
  let stop = CancellationToken::new();
  let connector_task = tokio::spawn(connector::run(config, stop.clone()));
  wait_for(|| tunnels.online(&host_id)).await;
  let (mut socket, mut channel) = device_request(
    &endpoint,
    &recipient,
    &paired.host_public_key,
    "GET",
    "/api/v1/health",
    &[],
  )
  .await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { status: 200, .. }
  ));
  assert_eq!(
    calls.load(Ordering::SeqCst),
    1,
    "saved device authorization survives connector restart"
  );
  drop(socket);

  let updates_request = serde_json::json!({
    "request": {
      "subscription_id": "paired-updates",
      "session_key": "paired-session",
      "level": "all",
      "cursor": null,
      "detail_keys": [],
      "unsubscribe": false
    }
  });
  let (mut socket, mut channel) = device_request(
    &endpoint,
    &recipient,
    &paired.host_public_key,
    "POST",
    "/api/v1/load_session_updates",
    &serde_json::to_vec(&updates_request).unwrap(),
  )
  .await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { status: 200, .. }
  ));
  let mut response = Vec::new();
  loop {
    let record = next_binary(&mut socket).await.unwrap();
    assert!(
      !record
        .windows(b"Updated reply".len())
        .any(|part| part == b"Updated reply"),
      "session updates remain encrypted through the Hub"
    );
    match channel.decrypt(&record).unwrap() {
      InnerMessage::Chunk { data } => response.extend(protocol::decode(&data, protocol::CHUNK_SIZE).unwrap()),
      InnerMessage::End {} => break,
      message => panic!("expected encrypted session update response, got {message:?}"),
    }
  }
  assert_eq!(
    serde_json::from_slice::<serde_json::Value>(&response).unwrap(),
    updates_response
  );
  assert_eq!(*updates_requests.lock().unwrap(), vec![updates_request]);
  drop(socket);

  let (mut socket, mut channel) = device_request(
    &endpoint,
    &recipient,
    &paired.host_public_key,
    "POST",
    "/api/v1/submit_session_input",
    &[],
  )
  .await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Error { .. }
  ));
  drop(socket);
  let (mut socket, mut channel) = device_request(
    &endpoint,
    &recipient,
    &paired.host_public_key,
    "GET",
    "/api/v1/events",
    &[],
  )
  .await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { .. }
  ));
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Chunk { .. }
  ));
  std::fs::write(&state_file, "{}").unwrap();
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Error { .. }
  ));
  wait_for(|| !streaming.load(Ordering::SeqCst)).await;
  assert_eq!(calls.load(Ordering::SeqCst), 1);
  stop.cancel();
  connector_task.await.unwrap().unwrap();
  hub_task.abort();
  local_task.abort();
}

#[tokio::test]
async fn encrypted_transport_authenticates_grants_bounds_streams_and_rejects_plaintext() {
  use crate::{
    connector::SecureHostConfig,
    secure::{GRANT_VERSION, Grant, GrantScope, InnerMessage, NoiseIdentity, OwnerIdentity},
  };
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store);
  let (hub_url, hub_task) = hub(tunnels.clone()).await;
  let owner = OwnerIdentity::generate();
  let recipient = NoiseIdentity::generate().unwrap();
  let noise_file = directory.path().join("noise.json");
  let host_identity = NoiseIdentity::load_or_create(&noise_file).unwrap();
  let revoked_file = directory.path().join("revoked.json");
  std::fs::write(&revoked_file, "[]").unwrap();
  let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
  let health_calls = calls.clone();
  let streaming = Arc::new(AtomicBool::new(false));
  let stream_state = streaming.clone();
  let local = Router::new()
    .route(
      "/api/v1/health",
      get(move || {
        health_calls.fetch_add(1, Ordering::SeqCst);
        async { "{}" }
      }),
    )
    .route("/api/v1/list_sessions", post(|body: Bytes| async { body }))
    .route(
      "/api/v1/shared",
      post(|axum::Json(envelope): axum::Json<serde_json::Value>| async { axum::Json(envelope) }),
    )
    .route(
      "/api/v1/events",
      get(move || {
        let streaming = stream_state.clone();
        async move {
          let stream = async_stream::stream! {
            let _guard = StreamGuard(streaming.clone());
            streaming.store(true, Ordering::SeqCst);
            yield Ok::<_, io::Error>(Bytes::from_static(b"event: ready\ndata: {}\n\n"));
            std::future::pending::<()>().await;
          };
          Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
        }
      }),
    );
  let (local_url, local_task) = serve(local).await;
  let config = ConnectorConfig {
    hub_url: hub_url.clone(),
    local_url,
    key_file: directory.path().join("host.json"),
    name: "Encrypted host".into(),
    local_token: None,
    allow_control: false,
    insecure_loopback: true,
    secure: Some(SecureHostConfig {
      noise_key_file: noise_file,
      owner_public_key: owner.public_key(),
      revocations_file: Some(revoked_file.clone()),
    }),
    paired: None,
  };
  let stop = CancellationToken::new();
  let connector = tokio::spawn(connector::run(config, stop.clone()));
  wait_for(|| !tunnels.pending().is_empty()).await;
  let record = tunnels.approve(&tunnels.pending()[0].pairing_code).unwrap();
  wait_for(|| tunnels.online(&record.host_id)).await;
  wait_for(|| tunnels.secure_only(&record.host_id)).await;
  let grant = owner
    .sign_grant(Grant {
      version: GRANT_VERSION,
      grant_id: "integration_grant".into(),
      host_id: record.host_id.clone(),
      host_public_key: host_identity.public_key(),
      recipient_public_key: recipient.public_key(),
      scope: GrantScope::All {},
      allow_control: false,
      expires_at: std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 300,
    })
    .unwrap();
  let mut endpoint = hub_url;
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(&format!("/hub/v1/secure/{}", record.host_id));

  let error = tunnels
    .proxy(&record.host_id, Method::GET, "/api/v1/health", Bytes::new())
    .await
    .err()
    .unwrap();
  assert_eq!(error.status, StatusCode::FORBIDDEN);
  assert!(error.message.contains("encrypted client"));
  assert_eq!(
    calls.load(Ordering::SeqCst),
    0,
    "a malicious Hub cannot downgrade this host to plaintext"
  );
  // Bypass the Hub's advisory secure-only check: the host must still reject a
  // plaintext request injected directly onto its authenticated tunnel.
  let connection = tunnels.inner.lock().unwrap().online[&record.host_id].clone();
  let (headers, ready) = oneshot::channel();
  let (chunks, _) = mpsc::channel(1);
  connection.requests.lock().unwrap().insert(
    900,
    InFlight {
      headers: Some(headers),
      chunks,
      failure: Arc::new(Mutex::new(None)),
    },
  );
  connection
    .outgoing
    .send(Frame::Request {
      request_id: 900,
      method: "GET".into(),
      path: "/api/v1/health".into(),
      body: String::new(),
    })
    .await
    .unwrap();
  let rejected = tokio::time::timeout(Duration::from_secs(2), ready)
    .await
    .unwrap()
    .unwrap();
  assert!(rejected.unwrap_err().contains("end-to-end"));
  assert_eq!(calls.load(Ordering::SeqCst), 0);

  let attacker = NoiseIdentity::generate().unwrap();
  let (mut socket, mut channel) = encrypted_socket(&endpoint, &attacker, &host_identity.public_key()).await;
  send_inner(
    &mut socket,
    &mut channel,
    &InnerMessage::Request {
      method: "GET".into(),
      path: "/api/v1/health".into(),
      grant: grant.clone(),
    },
  )
  .await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Error { .. }
  ));
  assert!(
    next_binary(&mut socket).await.is_none(),
    "a stolen grant needs its recipient's private key"
  );
  assert_eq!(calls.load(Ordering::SeqCst), 0);

  let data = vec![b'x'; protocol::MAX_BODY];
  let (mut socket, mut channel) =
    secure_request(&endpoint, &recipient, &grant, "POST", "/api/v1/list_sessions", &data).await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { status: 200, .. }
  ));
  let mut response = Vec::new();
  for _ in 0..protocol::RESPONSE_WINDOW {
    let record = next_binary(&mut socket).await.unwrap();
    assert!(
      !record.windows(32).any(|chunk| chunk == &[b'x'; 32]),
      "Hub records never contain response plaintext"
    );
    let InnerMessage::Chunk { data } = channel.decrypt(&record).unwrap() else {
      panic!("expected encrypted chunk")
    };
    response.extend(protocol::decode(&data, protocol::CHUNK_SIZE).unwrap());
  }
  assert!(
    tokio::time::timeout(Duration::from_millis(100), socket.next())
      .await
      .is_err(),
    "host stops when the encrypted downstream window is exhausted"
  );
  send_inner(
    &mut socket,
    &mut channel,
    &InnerMessage::Window {
      credits: protocol::RESPONSE_WINDOW,
    },
  )
  .await;
  loop {
    match channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap() {
      InnerMessage::Chunk { data } => {
        response.extend(protocol::decode(&data, protocol::CHUNK_SIZE).unwrap());
        send_inner(&mut socket, &mut channel, &InnerMessage::Window { credits: 1 }).await;
      }
      InnerMessage::End {} => break,
      message => panic!("unexpected {message:?}"),
    }
  }
  assert_eq!(
    response, data,
    "a full request and response span multiple Noise records intact"
  );
  drop(socket);

  let mut selected = grant.grant.clone();
  selected.grant_id = "selected_grant".into();
  selected.scope = GrantScope::Sessions {
    session_keys: vec!["approved_session".into()],
  };
  let selected = owner.sign_grant(selected).unwrap();
  let (mut socket, mut channel) = secure_request(
    &endpoint,
    &recipient,
    &selected,
    "POST",
    "/api/v1/list_sessions",
    br#"{"session_keys":["private_session"],"principal":"attacker"}"#,
  )
  .await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { status: 200, .. }
  ));
  let mut body = Vec::new();
  loop {
    match channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap() {
      InnerMessage::Chunk { data } => {
        body.extend(protocol::decode(&data, protocol::CHUNK_SIZE).unwrap());
        send_inner(&mut socket, &mut channel, &InnerMessage::Window { credits: 1 }).await;
      }
      InnerMessage::End {} => break,
      message => panic!("unexpected {message:?}"),
    }
  }
  let envelope: serde_json::Value = serde_json::from_slice(&body).unwrap();
  assert_eq!(envelope["session_keys"], serde_json::json!(["approved_session"]));
  assert_eq!(envelope["principal"], recipient.public_key());
  assert_eq!(envelope["command"], "list_sessions");
  drop(socket);
  let (mut socket, mut channel) = secure_request(
    &endpoint,
    &recipient,
    &selected,
    "POST",
    "/api/v1/submit_session_input",
    b"{}",
  )
  .await;
  assert!(
    matches!(
      channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
      InnerMessage::Error { .. }
    ),
    "selected-session sharing never authorizes input"
  );
  drop(socket);

  let mut expiring = grant.grant.clone();
  expiring.grant_id = "expiring_grant".into();
  expiring.expires_at = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs()
    + 3;
  let expiring = owner.sign_grant(expiring).unwrap();
  let (mut socket, mut channel) = secure_request(&endpoint, &recipient, &expiring, "GET", "/api/v1/events", &[]).await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { .. }
  ));
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Chunk { .. }
  ));
  let message = channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap();
  assert!(matches!(message, InnerMessage::Error { message } if message.contains("expired")));
  wait_for(|| !streaming.load(Ordering::SeqCst)).await;
  drop(socket);

  let (mut socket, mut channel) = secure_request(&endpoint, &recipient, &grant, "GET", "/api/v1/events", &[]).await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { .. }
  ));
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Chunk { .. }
  ));
  assert!(streaming.load(Ordering::SeqCst));
  drop(socket);
  wait_for(|| !streaming.load(Ordering::SeqCst)).await;

  let (mut socket, mut channel) = secure_request(&endpoint, &recipient, &grant, "GET", "/api/v1/events", &[]).await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { .. }
  ));
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Chunk { .. }
  ));
  std::fs::write(&revoked_file, serde_json::to_vec(&vec![&grant.grant.grant_id]).unwrap()).unwrap();
  let message = channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap();
  assert!(matches!(message, InnerMessage::Error { message } if message.contains("revoked")));
  wait_for(|| !streaming.load(Ordering::SeqCst)).await;

  let (mut socket, mut channel) = encrypted_socket(&endpoint, &recipient, &host_identity.public_key()).await;
  send_inner(
    &mut socket,
    &mut channel,
    &InnerMessage::Request {
      method: "GET".into(),
      path: "/api/v1/health".into(),
      grant,
    },
  )
  .await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Error { message } if message.contains("revoked")
  ));
  assert!(next_binary(&mut socket).await.is_none());
  assert_eq!(calls.load(Ordering::SeqCst), 0);

  let mut waiting = Vec::new();
  for _ in 0..protocol::MAX_REQUESTS {
    waiting.push(connect_async(endpoint.as_str()).await.unwrap().0);
  }
  let connection = tunnels.inner.lock().unwrap().online[&record.host_id].clone();
  wait_for(|| connection.secure_channels.lock().unwrap().len() == protocol::MAX_REQUESTS).await;
  let mut overflow = connect_async(endpoint.as_str()).await.unwrap().0;
  assert!(
    next_binary(&mut overflow).await.is_none(),
    "anonymous channels cannot exceed the host capacity"
  );
  drop(waiting);
  wait_for(|| connection.secure_channels.lock().unwrap().is_empty()).await;
  stop.cancel();
  connector.await.unwrap().unwrap();
  tunnels.shutdown();
  hub_task.abort();
  local_task.abort();
}

#[tokio::test]
async fn blind_channels_accept_opaque_bytes_and_cancel_only_the_saturated_channel() {
  let (outgoing, mut commands) = mpsc::channel(8);
  let connection = Arc::new(Connection {
    capacity: Weak::new(),
    outgoing,
    requests: Mutex::new(HashMap::new()),
    secure_channels: Mutex::new(HashMap::new()),
    next_id: std::sync::atomic::AtomicU64::new(1),
    allow_control: false,
    secure_only: AtomicBool::new(false),
    shutdown: CancellationToken::new(),
  });
  let (sender, mut received) = mpsc::channel(1);
  connection.secure_channels.lock().unwrap().insert(7, sender);
  let arbitrary = b"\xff\0opaque ciphertext";
  connection
    .receive(Frame::SecureData {
      channel_id: 7,
      data: protocol::encode(arbitrary),
    })
    .unwrap();
  assert_eq!(&received.recv().await.unwrap()[..], arbitrary);
  for _ in 0..2 {
    connection
      .receive(Frame::SecureData {
        channel_id: 7,
        data: protocol::encode(arbitrary),
      })
      .unwrap();
  }
  assert!(connection.secure_channels.lock().unwrap().is_empty());
  assert!(matches!(
    commands.recv().await,
    Some(Frame::SecureClose { channel_id: 7 })
  ));
  assert!(!connection.shutdown.is_cancelled());
  let (sender, _) = mpsc::channel(1);
  connection.secure_channels.lock().unwrap().insert(8, sender);
  drop(SecureGuard {
    connection: connection.clone(),
    channel_id: 8,
  });
  assert!(matches!(
    commands.recv().await,
    Some(Frame::SecureClose { channel_id: 8 })
  ));
  assert!(connection.secure_channels.lock().unwrap().is_empty());
}

#[test]
fn anonymous_secure_channels_have_a_global_budget_separate_from_host_enrollment() {
  let directory = tempfile::tempdir().unwrap();
  let tunnels = HubTunnels::new(Store::open(directory.path().join("hub.sqlite")).unwrap());
  let mut permits = Vec::new();
  for _ in 0..128 {
    permits.push(tunnels.reserve_secure_channel().unwrap());
  }
  assert!(tunnels.reserve_secure_channel().is_err());
  assert_eq!(tunnels.handshakes.available_permits(), 32);
  assert_eq!(tunnels.approved.available_permits(), 64);
  permits.pop();
  assert!(tunnels.reserve_secure_channel().is_ok());
  drop(permits);
  assert_eq!(tunnels.secure_capacity.available_permits(), 128);
}
