use axum::{
  Json, Router,
  extract::{
    State, WebSocketUpgrade,
    ws::{Message, WebSocket},
  },
  http::StatusCode,
  response::{IntoResponse, Response, Sse, sse::Event},
  routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
  convert::Infallible,
  sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
  },
  time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::{
  pairing::TotpSecret,
  secure::{HostAuthOperation, InnerMessage, NoiseIdentity, NoiseResponder},
};
use tokn_hub_remote::{RemoteManager, ResolvedMachine, SavedHost};
use tokn_session_hub::{
  connector::{self, ConnectorConfig, PairedHostConfig},
  onboarding::{self, HostProfile},
  server::{self, HubState},
  store::Store,
};
use url::Url;
use webauthn_authenticator_rs::{WebauthnAuthenticator, softpasskey::SoftPasskey};

struct Harness {
  directory: tempfile::TempDir,
  hub_url: String,
  host_id: String,
  host_key: String,
  secret: TotpSecret,
  stop: CancellationToken,
  tasks: Vec<tokio::task::JoinHandle<()>>,
  event_attempts: Arc<AtomicUsize>,
  pairing_directory: PairingDirectory,
  hub_state: HubState,
}

#[derive(Clone)]
struct PairingDirectory {
  current: Arc<Mutex<ResolvedMachine>>,
  followup: Arc<Mutex<Option<ResolvedMachine>>>,
  lookups: Arc<AtomicUsize>,
}

impl PairingDirectory {
  fn resolve(&self) -> ResolvedMachine {
    if self.lookups.fetch_add(1, Ordering::SeqCst) > 0 {
      if let Some(resolved) = self.followup.lock().unwrap().clone() {
        return resolved;
      }
    }
    self.current.lock().unwrap().clone()
  }
}

impl Drop for Harness {
  fn drop(&mut self) {
    self.stop.cancel();
    for task in &self.tasks {
      task.abort();
    }
  }
}

impl Harness {
  async fn start() -> Self {
    Self::start_with_event_failures(0).await
  }
  async fn start_with_event_failures(event_failures: usize) -> Self {
    let directory = tempfile::tempdir().unwrap();
    let event_attempts = Arc::new(AtomicUsize::new(0));
    let attempts = event_attempts.clone();
    let viewer = Router::new()
      .route("/api/v1/health", get(|| async { Json(json!({"version":1})) }))
      .route(
        "/api/v1/list_sessions",
        post(|Json(payload): Json<serde_json::Value>| async move {
          Json(json!({"sessions":[{"text":"private native history"}],"received":payload}))
        }),
      )
      .route(
        "/api/v1/events",
        get(move || {
          let attempts = attempts.clone();
          async move {
            if attempts.fetch_add(1, Ordering::SeqCst) < event_failures {
              return (StatusCode::SERVICE_UNAVAILABLE, "Temporary stream failure").into_response();
            }
            Sse::new(
              futures_util::stream::iter([Ok::<_, Infallible>(Event::default().event("ready").data("{}"))])
                .chain(futures_util::stream::pending()),
            )
            .into_response()
          }
        }),
      );
    let viewer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let viewer_url = Url::parse(&format!("http://{}", viewer_listener.local_addr().unwrap())).unwrap();
    let viewer_task = tokio::spawn(async move {
      axum::serve(viewer_listener, viewer).await.unwrap();
    });
    let hub_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_url = format!("http://localhost:{}", hub_listener.local_addr().unwrap().port());
    let host_id = uuid::Uuid::new_v4().to_string();
    let pairing_directory = PairingDirectory {
      current: Arc::new(Mutex::new(ResolvedMachine {
        host_id: host_id.clone(),
        machine_address: "alice:workstation".into(),
        name: "Native smoke host".into(),
        online: true,
      })),
      followup: Arc::new(Mutex::new(None)),
      lookups: Arc::new(AtomicUsize::new(0)),
    };
    let directory_lookup = pairing_directory.clone();
    let state = HubState::new(Store::open(directory.path().join("hub.sqlite")).unwrap(), &hub_url).unwrap();
    let hub = server::router(state.clone()).route(
      "/hub/v1/resolve/alice/workstation",
      get(move || {
        let directory_lookup = directory_lookup.clone();
        async move { Json(directory_lookup.resolve()) }
      }),
    );
    let hub_task = tokio::spawn(async move {
      axum::serve(hub_listener, hub).await.unwrap();
    });
    let host_dir = directory.path().join("host");
    std::fs::create_dir(&host_dir).unwrap();
    let secret = TotpSecret::generate();
    let noise_key = host_dir.join("host-noise.key");
    let host_key = NoiseIdentity::load_or_create(&noise_key).unwrap().public_key();
    onboarding::initialize_host_access(&host_dir.join("host-access.json"), &secret).unwrap();
    HostProfile {
      version: 1,
      host_id: host_id.clone(),
      hub_url: hub_url.clone(),
      name: "Native smoke host".into(),
      viewer_url: viewer_url.to_string(),
      allow_control: false,
      insecure_loopback: true,
      ice_servers: Vec::new(),
      passkey_origin: Some(hub_url.clone()),
    }
    .save(&host_dir.join("host.json"))
    .unwrap();
    let config = ConnectorConfig {
      hub_url: Url::parse(&hub_url).unwrap(),
      local_url: viewer_url,
      key_file: host_dir.join("host-enrollment.key"),
      name: "Native smoke host".into(),
      local_token: None,
      allow_control: false,
      insecure_loopback: true,
      ice_servers: Vec::new(),
      secure: None,
      paired: Some(PairedHostConfig {
        host_id: host_id.clone(),
        noise_key_file: noise_key,
        state_file: host_dir.join("host-access.json"),
      }),
    };
    let stop = CancellationToken::new();
    let connector_stop = stop.clone();
    let connector_task = tokio::spawn(async move {
      connector::run(config, connector_stop).await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), async {
      while !state.tunnels.online(&host_id) {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .unwrap();
    Self {
      directory,
      hub_url,
      host_id,
      host_key,
      secret,
      stop,
      tasks: vec![viewer_task, hub_task, connector_task],
      event_attempts,
      pairing_directory,
      hub_state: state,
    }
  }
  fn manager(&self, name: &str) -> RemoteManager {
    RemoteManager::new(self.directory.path().join(name))
  }
}

#[tokio::test]
async fn native_pairs_reconnects_reads_streams_and_obeys_revocation() {
  let harness = Harness::start().await;
  let manager = harness.manager("app");
  assert!(manager.status(&harness.hub_url).await.unwrap().hosts.is_empty());
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  let host = manager
    .pair(
      &harness.hub_url,
      &harness.host_id,
      harness.secret.code_at(now),
      Some(&harness.host_key),
    )
    .await
    .unwrap();
  assert_eq!(host.host_public_key, harness.host_key);
  assert_eq!(manager.status(&harness.hub_url).await.unwrap().selected_host_id, None);
  let sink = Arc::new(|_: &str, _: serde_json::Value| {});
  let first = manager
    .open(&harness.hub_url, &harness.host_id, None, sink.clone())
    .await
    .unwrap();
  assert_eq!(
    manager
      .request(&first.connection_id, "list_sessions", json!({}))
      .await
      .unwrap()["sessions"][0]["text"],
    "private native history"
  );
  manager.listen(&first.connection_id).await.unwrap();
  assert!(
    manager
      .request(
        &first.connection_id,
        "submit_session_input",
        json!({"request":{"text":"deny"}})
      )
      .await
      .unwrap_err()
      .contains("control")
  );
  manager.close(&first.connection_id).await;
  assert!(
    manager
      .request(&first.connection_id, "list_sessions", json!({}))
      .await
      .is_err()
  );
  // A fresh app instance reconnects solely by its remembered key and host pin.
  let remembered = harness.manager("app");
  let next = remembered
    .open(&harness.hub_url, &harness.host_id, None, sink)
    .await
    .unwrap();
  let public_key = remembered.status(&harness.hub_url).await.unwrap().device_public_key;
  onboarding::remove_device(&harness.directory.path().join("host/host-access.json"), &public_key).unwrap();
  assert!(
    remembered
      .request(&next.connection_id, "list_sessions", json!({}))
      .await
      .unwrap_err()
      .contains("not paired")
  );
  remembered.forget(&harness.hub_url, &harness.host_id).await.unwrap();
  assert!(remembered.status(&harness.hub_url).await.unwrap().hosts.is_empty());
  assert!(
    remembered
      .request(&next.connection_id, "list_sessions", json!({}))
      .await
      .is_err()
  );
}

#[tokio::test]
async fn named_native_pairing_saves_verified_current_metadata_and_selects_only_on_open() {
  let harness = Harness::start().await;
  let manager = harness.manager("app");
  let mut latest = harness.pairing_directory.current.lock().unwrap().clone();
  latest.name = "Renamed native host".into();
  *harness.pairing_directory.followup.lock().unwrap() = Some(latest.clone());
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  let host = manager
    .pair_with_address(
      &harness.hub_url,
      &harness.host_id,
      harness.secret.code_at(now),
      Some(&harness.host_key),
      Some("alice:workstation"),
    )
    .await
    .unwrap();
  assert_eq!(host.machine_address.as_deref(), Some("alice:workstation"));
  assert_eq!(host.name.as_deref(), Some("Renamed native host"));
  assert_eq!(host.host_public_key, harness.host_key);
  assert_eq!(harness.pairing_directory.lookups.load(Ordering::SeqCst), 2);
  let restarted = harness.manager("app");
  let status = restarted.status(&harness.hub_url).await.unwrap();
  assert_eq!(status.hosts, vec![host]);
  assert_eq!(status.selected_host_id, None);
  latest.host_id = uuid::Uuid::new_v4().to_string();
  *harness.pairing_directory.followup.lock().unwrap() = Some(latest);
  restarted
    .open(&harness.hub_url, &harness.host_id, None, Arc::new(|_, _| {}))
    .await
    .unwrap();
  assert_eq!(
    restarted
      .status(&harness.hub_url)
      .await
      .unwrap()
      .selected_host_id
      .as_deref(),
    Some(harness.host_id.as_str())
  );
  assert_eq!(
    harness.pairing_directory.lookups.load(Ordering::SeqCst),
    2,
    "remembered open uses its UUID and pin directly"
  );
}

#[tokio::test]
async fn named_native_pairing_rejects_uuid_remapping_before_or_after_pake_without_saving_a_pin() {
  let harness = Harness::start().await;
  let manager = harness.manager("app");
  let mut replacement = harness.pairing_directory.current.lock().unwrap().clone();
  replacement.host_id = uuid::Uuid::new_v4().to_string();
  *harness.pairing_directory.current.lock().unwrap() = replacement.clone();
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  let error = manager
    .pair_with_address(
      &harness.hub_url,
      &harness.host_id,
      harness.secret.code_at(now),
      None,
      Some("alice:workstation"),
    )
    .await
    .unwrap_err();
  assert!(error.contains("requested UUID"), "{error}");
  assert_eq!(harness.pairing_directory.lookups.load(Ordering::SeqCst), 1);
  harness.pairing_directory.current.lock().unwrap().host_id = harness.host_id.clone();
  *harness.pairing_directory.followup.lock().unwrap() = Some(replacement);
  harness.pairing_directory.lookups.store(0, Ordering::SeqCst);
  let error = manager
    .pair_with_address(
      &harness.hub_url,
      &harness.host_id,
      harness.secret.code_at(now),
      None,
      Some("alice:workstation"),
    )
    .await
    .unwrap_err();
  assert!(error.contains("changed UUID during pairing"), "{error}");
  let status = harness.manager("app").status(&harness.hub_url).await.unwrap();
  assert!(status.hosts.is_empty());
  assert_eq!(status.selected_host_id, None);
  assert_eq!(harness.pairing_directory.lookups.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn named_native_pairing_alias_collision_preserves_the_previous_pin_and_selection() {
  let harness = Harness::start().await;
  let manager = harness.manager("app");
  let store = onboarding::ClientStore::load_or_create(
    &harness.directory.path().join("app"),
    &Url::parse(&harness.hub_url).unwrap(),
  )
  .unwrap();
  let original = SavedHost {
    host_id: uuid::Uuid::new_v4().to_string(),
    host_public_key: NoiseIdentity::generate().unwrap().public_key(),
    machine_address: Some("alice:workstation".into()),
    name: Some("Original remembered host".into()),
  };
  store.save_host(original.clone()).unwrap();
  store.select_host(&original.host_id).unwrap();
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  let error = manager
    .pair_with_address(
      &harness.hub_url,
      &harness.host_id,
      harness.secret.code_at(now),
      Some(&harness.host_key),
      Some("alice:workstation"),
    )
    .await
    .unwrap_err();
  assert!(error.contains("Duplicate saved machine address"), "{error}");
  let status = harness.manager("app").status(&harness.hub_url).await.unwrap();
  assert_eq!(status.hosts, vec![original.clone()]);
  assert_eq!(status.selected_host_id.as_deref(), Some(original.host_id.as_str()));
  assert_eq!(harness.pairing_directory.lookups.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn native_passkey_ceremony_authorizes_a_new_device_on_its_original_channel() {
  let harness = Harness::start().await;
  let owner = harness.manager("owner");
  // Authorize the enrolling device as if it had completed the tested OTP flow.
  let owner_key = owner.status(&harness.hub_url).await.unwrap().device_public_key;
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  onboarding::authorize_device(
    &harness.directory.path().join("host/host-access.json"),
    &owner_key,
    now / 30,
    now,
  )
  .unwrap();
  let owner_store = onboarding::ClientStore::load_or_create(
    &harness.directory.path().join("owner"),
    &Url::parse(&harness.hub_url).unwrap(),
  )
  .unwrap();
  owner_store
    .save_host(tokn_hub_remote::SavedHost {
      host_id: harness.host_id.clone(),
      host_public_key: harness.host_key.clone(),
      machine_address: None,
      name: None,
    })
    .unwrap();
  let mut authenticator = WebauthnAuthenticator::new(SoftPasskey::new(true));
  let enrollment = owner
    .auth_start(&harness.hub_url, &harness.host_id, &harness.host_key, true)
    .await
    .unwrap();
  let credential = authenticator
    .do_registration(
      Url::parse(&harness.hub_url).unwrap(),
      serde_json::from_value(enrollment.options).unwrap(),
    )
    .unwrap();
  owner
    .auth_finish(&enrollment.auth_id, serde_json::to_value(credential).unwrap())
    .await
    .unwrap();
  let app = harness.manager("new-app");
  assert!(app.status(&harness.hub_url).await.unwrap().hosts.is_empty());
  let login = app
    .auth_start(&harness.hub_url, &harness.host_id, &harness.host_key, false)
    .await
    .unwrap();
  let credential = authenticator
    .do_authentication(
      Url::parse(&harness.hub_url).unwrap(),
      serde_json::from_value(login.options).unwrap(),
    )
    .unwrap();
  let credential_json = serde_json::to_value(credential).unwrap();
  app.auth_finish(&login.auth_id, credential_json.clone()).await.unwrap();
  assert_eq!(
    app.status(&harness.hub_url).await.unwrap().selected_host_id,
    None,
    "authentication must not select a machine before it is opened"
  );
  assert!(
    app.auth_finish(&login.auth_id, credential_json).await.is_err(),
    "a completed ceremony must not be replayed"
  );
  let info = app
    .open(&harness.hub_url, &harness.host_id, None, Arc::new(|_, _| {}))
    .await
    .unwrap();
  assert!(
    app
      .request(&info.connection_id, "list_sessions", json!({}))
      .await
      .is_ok()
  );
  app.close_all().await;
  let cancelled = app
    .auth_start(&harness.hub_url, &harness.host_id, &harness.host_key, false)
    .await
    .unwrap();
  app.auth_cancel(&cancelled.auth_id).await;
  assert!(app.auth_finish(&cancelled.auth_id, json!({})).await.is_err());
}

#[tokio::test]
async fn native_stream_retries_transient_failures_before_its_first_ready_event() {
  let harness = Harness::start_with_event_failures(2).await;
  let manager = harness.manager("stream-retry");
  let device_key = manager.status(&harness.hub_url).await.unwrap().device_public_key;
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  onboarding::authorize_device(
    &harness.directory.path().join("host/host-access.json"),
    &device_key,
    now / 30,
    now,
  )
  .unwrap();
  let store = onboarding::ClientStore::load_or_create(
    &harness.directory.path().join("stream-retry"),
    &Url::parse(&harness.hub_url).unwrap(),
  )
  .unwrap();
  store
    .save_host(tokn_hub_remote::SavedHost {
      host_id: harness.host_id.clone(),
      host_public_key: harness.host_key.clone(),
      machine_address: None,
      name: None,
    })
    .unwrap();
  let info = manager
    .open(&harness.hub_url, &harness.host_id, None, Arc::new(|_, _| {}))
    .await
    .unwrap();
  tokio::time::timeout(Duration::from_secs(6), manager.listen(&info.connection_id))
    .await
    .unwrap()
    .unwrap();
  assert_eq!(harness.event_attempts.load(Ordering::SeqCst), 3);
  manager.close_all().await;
}

#[derive(Clone)]
struct DelayedAuthHost {
  identity: Arc<NoiseIdentity>,
  finish_received: Arc<Notify>,
  reply: Arc<Notify>,
}

async fn delayed_auth_upgrade(State(host): State<DelayedAuthHost>, upgrade: WebSocketUpgrade) -> Response {
  upgrade.on_upgrade(move |socket| delayed_auth_channel(host, socket))
}

async fn delayed_auth_channel(host: DelayedAuthHost, mut socket: WebSocket) {
  let Message::Binary(first) = socket.recv().await.unwrap().unwrap() else {
    panic!("expected Noise initiator");
  };
  let (response, mut channel) = NoiseResponder::new(&host.identity).unwrap().accept(&first).unwrap();
  socket.send(Message::Binary(response.into())).await.unwrap();
  let Message::Binary(start) = socket.recv().await.unwrap().unwrap() else {
    panic!("expected auth start");
  };
  assert!(matches!(
    channel.decrypt(&start).unwrap(),
    InnerMessage::AuthRequest {
      operation: HostAuthOperation::LoginStart,
      ..
    }
  ));
  socket
    .send(Message::Binary(
      channel
        .encrypt(&InnerMessage::AuthResponse {
          payload: json!({"options":{"publicKey":{"challenge":"test"}}}),
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
  let Message::Binary(finish) = socket.recv().await.unwrap().unwrap() else {
    panic!("expected auth finish");
  };
  assert!(matches!(
    channel.decrypt(&finish).unwrap(),
    InnerMessage::AuthRequest {
      operation: HostAuthOperation::LoginFinish,
      ..
    }
  ));
  host.finish_received.notify_one();
  host.reply.notified().await;
  let _ = socket
    .send(Message::Binary(
      channel
        .encrypt(&InnerMessage::AuthResponse {
          payload: json!({"authorized":true}),
        })
        .unwrap()
        .into(),
    ))
    .await;
}

#[tokio::test]
async fn cancelling_in_flight_native_auth_finish_preserves_device_pins_and_selection() {
  let directory = tempfile::tempdir().unwrap();
  let host = DelayedAuthHost {
    identity: Arc::new(NoiseIdentity::generate().unwrap()),
    finish_received: Arc::new(Notify::new()),
    reply: Arc::new(Notify::new()),
  };
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let hub_url = format!("http://localhost:{}", listener.local_addr().unwrap().port());
  let router = Router::new()
    .route("/hub/v1/secure/{host_id}", get(delayed_auth_upgrade))
    .with_state(host.clone());
  let server = tokio::spawn(async move {
    axum::serve(listener, router).await.unwrap();
  });
  let manager = RemoteManager::new(directory.path().join("app"));
  let host_id = uuid::Uuid::new_v4().to_string();
  let start = manager
    .auth_start(&hub_url, &host_id, &host.identity.public_key(), false)
    .await
    .unwrap();
  let finishing = manager.clone();
  let auth_id = start.auth_id.clone();
  let finish = tokio::spawn(async move { finishing.auth_finish(&auth_id, json!({})).await });
  tokio::time::timeout(Duration::from_secs(2), host.finish_received.notified())
    .await
    .unwrap();
  manager.auth_cancel(&start.auth_id).await;
  let failure = tokio::time::timeout(Duration::from_secs(1), finish)
    .await
    .unwrap()
    .unwrap()
    .unwrap_err();
  assert!(failure.contains("cancelled"));
  host.reply.notify_one();
  let status = manager.status(&hub_url).await.unwrap();
  assert!(status.hosts.is_empty(), "cancelled finish must not persist a host pin");
  assert_eq!(status.selected_host_id, None);
  assert!(manager.auth_finish(&start.auth_id, json!({})).await.is_err());
  server.abort();
}

#[test]
fn remote_origins_require_tls_and_reject_credentials_paths_and_queries() {
  for invalid in [
    "http://host.example",
    "http://[2001:db8::1]:8080",
    "http://[::]:8080",
    "http://[::1:8080",
    "https://user:secret@hub.example",
    "https://hub.example/path",
    "https://hub.example?token=x",
    "https://hub.example#x",
  ] {
    assert!(tokn_hub_remote::canonical_hub(invalid).is_err());
  }
  assert!(tokn_hub_remote::canonical_hub("https://hub.example").is_ok());
  assert!(tokn_hub_remote::canonical_hub("http://127.0.0.1:8080").is_ok());
  assert!(tokn_hub_remote::canonical_hub("http://[::1]:8080").is_ok());
  assert!(tokn_hub_remote::canonical_hub("http://[0:0:0:0:0:0:0:1]:8080").is_ok());
}

async fn authorize_native(harness: &Harness, manager: &RemoteManager, name: &str) {
  let key = manager.status(&harness.hub_url).await.unwrap().device_public_key;
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
  onboarding::authorize_device(
    &harness.directory.path().join("host/host-access.json"),
    &key,
    now / 30,
    now,
  )
  .unwrap();
  onboarding::ClientStore::load_or_create(
    &harness.directory.path().join(name),
    &Url::parse(&harness.hub_url).unwrap(),
  )
  .unwrap()
  .save_host(SavedHost {
    host_id: harness.host_id.clone(),
    host_public_key: harness.host_key.clone(),
    machine_address: None,
    name: None,
  })
  .unwrap();
}

#[tokio::test]
async fn native_direct_records_survive_hub_shutdown_and_keep_host_authorization() {
  let harness = Harness::start().await;
  let manager = harness.manager("direct");
  authorize_native(&harness, &manager, "direct").await;
  let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
  let sink = Arc::new(move |event: &str, value: serde_json::Value| {
    let _ = sender.send((event.to_owned(), value));
  });
  let info = manager
    .open(&harness.hub_url, &harness.host_id, None, sink)
    .await
    .unwrap();
  tokio::time::timeout(Duration::from_secs(25), async {
    loop {
      let (event, payload) = received.recv().await.unwrap();
      if event == "hub-client-transport" && payload["transport"]["kind"] == "direct" {
        break;
      }
      if event == "hub-client-transport" && payload["transport"]["reason"].is_string() {
        panic!("Direct upgrade failed: {payload}");
      }
    }
  })
  .await
  .unwrap();
  manager.listen(&info.connection_id).await.unwrap();
  // A complete request larger than a single RTC message exercises bounded
  // chunking and independent channel ordering, not just ICE connectivity.
  let payload = json!({"large":"x".repeat(900_000)});
  let response = manager
    .request(&info.connection_id, "list_sessions", payload.clone())
    .await
    .unwrap();
  assert_eq!(response["received"], payload);
  assert!(
    manager
      .request(&info.connection_id, "submit_session_input", json!({"text":"deny"}))
      .await
      .unwrap_err()
      .contains("control")
  );
  harness.hub_state.tunnels.shutdown();
  harness.tasks[1].abort();
  let response = manager
    .request(&info.connection_id, "list_sessions", json!({}))
    .await
    .unwrap();
  assert_eq!(response["sessions"][0]["text"], "private native history");
  let key = manager.status(&harness.hub_url).await.unwrap().device_public_key;
  onboarding::remove_device(&harness.directory.path().join("host/host-access.json"), &key).unwrap();
  assert!(
    manager
      .request(&info.connection_id, "list_sessions", json!({}))
      .await
      .is_err()
  );
  manager.close_all().await;
}
