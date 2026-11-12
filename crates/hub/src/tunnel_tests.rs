use super::*;
use crate::connector::{self, ConnectorConfig};
use axum::{
  Router,
  body::Body,
  extract::{Path, State, WebSocketUpgrade},
  http::StatusCode,
  response::Response,
  routing::{get, post},
};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use std::io;
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

fn claim_fixture_connector(store: &Store, config: &ConnectorConfig) {
  let paired = config.paired.as_ref().unwrap();
  let key = connector::load_key(&config.key_file).unwrap();
  let claim = store.claim_fixture_host(HostRecord {
    host_id: paired.host_id.clone(),
    name: config.name.clone(),
    public_key: protocol::encode(key.verifying_key().as_bytes()),
    access: if config.allow_control { "control" } else { "view" }.into(),
  });
  let hub_origin = crate::onboarding::canonical_hub(&config.hub_url).unwrap();
  let receipt = paired.state_file.with_file_name("host-account.json");
  crate::onboarding::save_configuration(&receipt, &serde_json::json!({"hub_origin": hub_origin, "claim": claim}))
    .unwrap();
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
async fn signatures_bind_fresh_challenge_identity_name_and_permission() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store);
  let (mut endpoint, task) = hub(tunnels.clone()).await;
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(protocol::TUNNEL_PATH);
  let key = SigningKey::generate(&mut OsRng);
  let public_key = protocol::encode(key.verifying_key().as_bytes());
  let host_id = uuid::Uuid::new_v4().to_string();
  let mut previous_nonce = None;
  for attempt in 0..5 {
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
    let signature = signing_key.sign(&protocol::registration_proof(
      signed_nonce,
      &host_id,
      &public_key,
      "Host",
      false,
    ));
    previous_nonce = Some(nonce);
    let frame = Frame::Register {
      version: protocol::VERSION,
      host_id: host_id.clone(),
      public_key: public_key.clone(),
      name: if attempt == 3 { "Different name" } else { "Host" }.into(),
      allow_control: attempt == 2,
      discovery_group: None,
      signature: protocol::encode(&signature.to_bytes()),
    };
    socket
      .send(ClientMessage::Text(serde_json::to_string(&frame).unwrap().into()))
      .await
      .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(2), socket.next())
      .await
      .unwrap();
    if attempt < 4 {
      assert!(
        !matches!(reply, Some(Ok(ClientMessage::Text(_)))),
        "invalid proof must not reach account policy"
      );
    } else {
      let Some(Ok(ClientMessage::Text(text))) = reply else {
        panic!("expected account registration rejection")
      };
      assert!(matches!(
        serde_json::from_str::<Frame>(&text).unwrap(),
        Frame::RegistrationRejected { message } if message == crate::store::ACCOUNT_REQUIRED
      ));
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
  let registered_public_key = protocol::encode(key.verifying_key().as_bytes());
  for attempt in 0..7 {
    let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
    let Some(Frame::Challenge { nonce, .. }) = next_frame(&mut socket).await else {
      panic!("challenge missing")
    };
    let signing_key = if attempt == 5 {
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
      discovery_group: None,
      signature: protocol::encode(&signing_key.sign(&proof).to_bytes()),
    };
    socket
      .send(ClientMessage::Text(serde_json::to_string(&frame).unwrap().into()))
      .await
      .unwrap();
    let reply = next_frame(&mut socket).await;
    if attempt == 2 {
      assert!(
        matches!(reply, Some(Frame::RegistrationRejected { message }) if message == crate::store::ACCOUNT_REQUIRED)
      );
      store
        .register_encrypted_host(HostRecord {
          host_id: host_id.clone(),
          name: "Host".into(),
          public_key: registered_public_key.clone(),
          access: "view".into(),
        })
        .unwrap();
    } else if attempt == 3 {
      assert!(
        matches!(reply, Some(Frame::RegistrationRejected { message }) if message == crate::store::ACCOUNT_REQUIRED)
      );
      assert!(
        !tunnels.online(&host_id),
        "a preexisting registration does not grant access"
      );
      store.claim_fixture_host(HostRecord {
        host_id: host_id.clone(),
        name: "Host".into(),
        public_key: registered_public_key.clone(),
        access: "view".into(),
      });
    } else if attempt == 4 {
      assert!(matches!(reply, Some(Frame::Ready { host_id: registered, .. }) if registered == host_id));
    } else if attempt >= 5 {
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
    if attempt == 5 {
      tunnels.revoke(&host_id).unwrap();
    }
  }
  assert!(store.hosts().unwrap().is_empty());
  tunnels.shutdown();
  task.abort();
}

#[tokio::test]
async fn encrypted_registration_proof_binds_discovery_group_and_cannot_be_stripped() {
  use tokn_hub_client_core::discovery::DiscoveryGroup;
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store.clone());
  let (mut endpoint, task) = hub(tunnels.clone()).await;
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(protocol::TUNNEL_PATH);
  let host_id = uuid::Uuid::new_v4().to_string();
  let key = SigningKey::generate(&mut OsRng);
  let public_key = protocol::encode(key.verifying_key().as_bytes());
  let group = store
    .claim_fixture_host(HostRecord {
      host_id: host_id.clone(),
      name: "Host".into(),
      public_key: public_key.clone(),
      access: "view".into(),
    })
    .group;
  let other = DiscoveryGroup {
    namespace: group.namespace.clone(),
    secret_digest: protocol::encode(&[2; 32]),
  };
  for sent_group in [Some(other), None, Some(group.clone())] {
    let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
    let Some(Frame::Challenge { nonce, .. }) = next_frame(&mut socket).await else {
      panic!("challenge missing")
    };
    let signature = key.sign(&protocol::discovery_registration_proof(
      &nonce,
      &host_id,
      &public_key,
      "Host",
      false,
      Some(&group),
    ));
    let expected = sent_group.as_ref() == Some(&group);
    let frame = Frame::Register {
      version: protocol::VERSION,
      host_id: host_id.clone(),
      public_key: public_key.clone(),
      name: "Host".into(),
      allow_control: false,
      discovery_group: sent_group,
      signature: protocol::encode(&signature.to_bytes()),
    };
    socket
      .send(ClientMessage::Text(serde_json::to_string(&frame).unwrap().into()))
      .await
      .unwrap();
    assert_eq!(
      matches!(next_frame(&mut socket).await, Some(Frame::Ready { .. })),
      expected
    );
  }
  assert_eq!(store.discover_machines(&group).unwrap().len(), 1);
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
    registration_diagnostic(crate::store::ACCOUNT_REQUIRED),
    crate::store::ACCOUNT_REQUIRED
  );
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

async fn signed_socket(endpoint: &url::Url, key: &SigningKey, host_id: Option<&str>) -> TestSocket {
  let (mut socket, _) = connect_async(endpoint.as_str()).await.unwrap();
  let Some(Frame::Challenge { nonce, .. }) = next_frame(&mut socket).await else {
    panic!("expected challenge");
  };
  let public_key = protocol::encode(key.verifying_key().as_bytes());
  let frame = if let Some(host_id) = host_id {
    let proof = protocol::registration_proof(&nonce, host_id, &public_key, "Host", false);
    Frame::Register {
      version: protocol::VERSION,
      host_id: host_id.into(),
      public_key,
      name: "Host".into(),
      allow_control: false,
      discovery_group: None,
      signature: protocol::encode(&key.sign(&proof).to_bytes()),
    }
  } else {
    let proof = protocol::proof(&nonce, &public_key, "Host", false);
    Frame::Authenticate {
      version: protocol::VERSION,
      public_key,
      name: "Host".into(),
      allow_control: false,
      signature: protocol::encode(&key.sign(&proof).to_bytes()),
    }
  };
  socket
    .send(ClientMessage::Text(serde_json::to_string(&frame).unwrap().into()))
    .await
    .unwrap();
  socket
}

#[tokio::test]
async fn legacy_authentication_is_rejected_without_taking_claimed_host_capacity() {
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let legacy_key = SigningKey::generate(&mut OsRng);
  let legacy_id = protocol::host_id(legacy_key.verifying_key().as_bytes());
  store
    .connection()
    .unwrap()
    .execute(
      "INSERT INTO hosts (host_id, name, public_key, access) VALUES (?1, 'Legacy', ?2, 'view')",
      (&legacy_id, protocol::encode(legacy_key.verifying_key().as_bytes())),
    )
    .unwrap();
  let claimed_key = SigningKey::generate(&mut OsRng);
  let host_id = uuid::Uuid::new_v4().to_string();
  store.claim_fixture_host(HostRecord {
    host_id: host_id.clone(),
    name: "Claimed".into(),
    public_key: protocol::encode(claimed_key.verifying_key().as_bytes()),
    access: "view".into(),
  });
  let tunnels = HubTunnels::with_limits(store, 1, 1);
  let (mut endpoint, task) = hub(tunnels.clone()).await;
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(protocol::TUNNEL_PATH);

  let mut legacy = signed_socket(&endpoint, &legacy_key, None).await;
  assert!(matches!(
    next_frame(&mut legacy).await,
    Some(Frame::RegistrationRejected { message }) if message == crate::store::ACCOUNT_REQUIRED
  ));
  assert!(!tunnels.online(&legacy_id));
  assert_eq!(tunnels.handshakes.available_permits(), 1);
  assert_eq!(tunnels.approved.available_permits(), 1);

  let mut claimed = signed_socket(&endpoint, &claimed_key, Some(&host_id)).await;
  assert!(matches!(next_frame(&mut claimed).await, Some(Frame::Ready { .. })));
  assert!(tunnels.online(&host_id));
  assert_eq!(tunnels.approved.available_permits(), 0);
  // The same account-owned identity can replace its connection while full.
  let mut replacement = signed_socket(&endpoint, &claimed_key, Some(&host_id)).await;
  assert!(matches!(next_frame(&mut replacement).await, Some(Frame::Ready { .. })));
  assert!(next_frame(&mut claimed).await.is_none());
  assert!(tunnels.online(&host_id));
  assert_eq!(tunnels.approved.available_permits(), 0);
  drop(replacement);
  wait_for(|| !tunnels.online(&host_id) && tunnels.approved.available_permits() == 1).await;
  tunnels.shutdown();
  task.abort();
}

#[tokio::test]
async fn accepted_agent_input_is_not_replayed_after_tunnel_reconnect() {
  use crate::{
    connector::PairedHostConfig,
    onboarding::{authorize_device, initialize_host_access},
    pairing::TotpSecret,
    secure::{InnerMessage, NoiseIdentity},
  };
  let directory = tempfile::tempdir().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let tunnels = HubTunnels::new(store.clone());
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
  let host_id = uuid::Uuid::new_v4().to_string();
  let noise_key_file = directory.path().join("noise.json");
  let host_key = NoiseIdentity::load_or_create(&noise_key_file).unwrap().public_key();
  let recipient = NoiseIdentity::generate().unwrap();
  let state_file = directory.path().join("access.json");
  initialize_host_access(&state_file, &TotpSecret::generate()).unwrap();
  let now = crate::secure_client::unix_time().unwrap();
  authorize_device(&state_file, &recipient.public_key(), now / 30, now).unwrap();
  let mut endpoint = hub_url.clone();
  endpoint.set_scheme("ws").unwrap();
  endpoint.set_path(&format!("/hub/v1/secure/{host_id}"));
  let config = ConnectorConfig {
    hub_url,
    local_url,
    key_file: directory.path().join("enrollment.json"),
    name: "Input host".into(),
    local_token: None,
    allow_control: true,
    insecure_loopback: true,
    ice_servers: Vec::new(),
    secure: None,
    paired: Some(PairedHostConfig {
      host_id: host_id.clone(),
      noise_key_file,
      state_file,
    }),
  };
  claim_fixture_connector(&store, &config);
  let stop = CancellationToken::new();
  let connector_task = tokio::spawn(connector::run(config, stop.clone()));
  wait_for(|| tunnels.online(&host_id)).await;
  let old_connection = tunnels.inner.lock().unwrap().online[&host_id].clone();
  let (mut request, _) = device_request(
    &endpoint,
    &recipient,
    &host_key,
    "POST",
    "/api/v1/submit_session_input",
    br#"{"request":{"text":"execute once"}}"#,
  )
  .await;
  tokio::time::timeout(Duration::from_secs(2), acceptances.recv())
    .await
    .unwrap()
    .unwrap();
  assert_eq!(accepted_count.load(Ordering::SeqCst), 1);
  old_connection.shutdown.cancel();
  assert!(next_binary(&mut request).await.is_none());

  // Exercise the production reconnect loop with the same approved identity.
  // A fresh successful request proves its new tunnel is usable.
  wait_for(|| tunnels.online(&host_id)).await;
  let (mut socket, mut channel) = device_request(&endpoint, &recipient, &host_key, "GET", "/api/v1/health", &[]).await;
  assert!(matches!(
    channel.decrypt(&next_binary(&mut socket).await.unwrap()).unwrap(),
    InnerMessage::Response { status: 200, .. }
  ));
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
    ice_servers: Vec::new(),
    secure: None,
    paired: Some(PairedHostConfig {
      host_id: host_id.clone(),
      noise_key_file: noise_file,
      state_file: state_file.clone(),
    }),
  };
  claim_fixture_connector(&store, &config);
  let stop = CancellationToken::new();
  let connector_task = tokio::spawn(connector::run(config.clone(), stop.clone()));
  wait_for(|| tunnels.online(&host_id)).await;
  assert_eq!(store.hosts().unwrap().len(), 1);
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
async fn blind_channels_accept_opaque_bytes_and_cancel_only_the_saturated_channel() {
  let (outgoing, mut commands) = mpsc::channel(8);
  let connection = Arc::new(Connection {
    capacity: Weak::new(),
    outgoing,
    secure_channels: Mutex::new(HashMap::new()),
    next_id: std::sync::atomic::AtomicU64::new(1),
    allow_control: false,
    connector_version: None,
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
