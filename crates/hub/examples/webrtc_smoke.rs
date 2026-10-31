//! Isolated loopback fixture for real browser/native WebRTC verification.
//! Contains synthetic data only. /smoke/authorize grants an ephemeral test key;
//! this deliberately bypasses pairing in this fixture, never in the product.
use axum::{
  Json, Router,
  extract::State,
  response::{Sse, sse::Event},
  routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
  convert::Infallible,
  path::PathBuf,
  time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::{pairing::TotpSecret, secure::NoiseIdentity};
use tokn_session_hub::{
  connector::{self, ConnectorConfig, PairedHostConfig},
  onboarding,
  server::{self, HubState},
  store::Store,
};
use tower_http::cors::CorsLayer;
use url::Url;

#[derive(Clone)]
struct Smoke {
  host_state: PathBuf,
  config: Value,
  hub: HubState,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let directory = tempfile::tempdir()?;
  let viewer = Router::new()
    .route("/api/v1/health", get(|| async { Json(json!({"version":1})) }))
    .route("/api/v1/list_sessions", post(|Json(payload): Json<Value>| async move {
      Json(json!({"sessions":[{"text":"Synthetic encrypted history"}],"received_bytes":payload["large"].as_str().map(str::len).unwrap_or(0)}))
    }))
    .route("/api/v1/events", get(|| async {
      Sse::new(futures_util::stream::iter([Ok::<_, Infallible>(Event::default().event("ready").data("{}"))])
        .chain(futures_util::stream::pending())).keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(10)))
    }));
  let viewer_listener = tokio::net::TcpListener::bind("127.0.0.1:15578").await?;
  tokio::spawn(async move {
    axum::serve(viewer_listener, viewer).await.unwrap();
  });
  let hub_listener = tokio::net::TcpListener::bind("127.0.0.1:15579").await?;
  let hub_url = "http://localhost:15579";
  // Vite is only a local source-code server for the synthetic smoke page.
  let hub = HubState::new(
    Store::open(directory.path().join("hub.sqlite"))?,
    "http://localhost:1447",
  )?;
  let host_id = uuid::Uuid::new_v4().to_string();
  let noise_key = directory.path().join("host-noise.key");
  let host_key = NoiseIdentity::load_or_create(&noise_key)?.public_key();
  let host_state = directory.path().join("host-access.json");
  onboarding::initialize_host_access(&host_state, &TotpSecret::generate())?;
  let smoke = Smoke {
    host_state: host_state.clone(),
    config: json!({"hub_url":hub_url,"host_id":host_id,"host_public_key":host_key}),
    hub: hub.clone(),
  };
  let fixture = Router::new()
    .route(
      "/smoke/config",
      get(|State(smoke): State<Smoke>| async move { Json(smoke.config) }),
    )
    .route(
      "/smoke/authorize",
      post(|State(smoke): State<Smoke>, Json(payload): Json<Value>| async move {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let key = payload["device_public_key"].as_str().unwrap_or_default();
        match onboarding::authorize_device(&smoke.host_state, key, now / 30, now) {
          Ok(()) => Json(json!({"ok":true})),
          Err(error) => Json(json!({"error":error})),
        }
      }),
    )
    .route(
      "/smoke/stop-hub",
      post(|State(smoke): State<Smoke>| async move {
        smoke.hub.tunnels.shutdown();
        Json(json!({"ok":true}))
      }),
    )
    .layer(
      CorsLayer::new()
        .allow_origin("http://localhost:1447".parse::<axum::http::HeaderValue>().unwrap())
        .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
        .allow_headers([axum::http::header::CONTENT_TYPE]),
    )
    .with_state(smoke);
  let router = server::router(hub.clone()).merge(fixture);
  tokio::spawn(async move {
    axum::serve(hub_listener, router).await.unwrap();
  });
  let shutdown = CancellationToken::new();
  let config = ConnectorConfig {
    hub_url: Url::parse(hub_url)?,
    local_url: Url::parse("http://127.0.0.1:15578")?,
    key_file: directory.path().join("host-enrollment.key"),
    name: "WebRTC smoke fixture".into(),
    local_token: None,
    allow_control: false,
    insecure_loopback: true,
    ice_servers: Vec::new(),
    secure: None,
    paired: Some(PairedHostConfig {
      host_id: host_id.clone(),
      noise_key_file: noise_key,
      state_file: host_state,
    }),
  };
  let stop = shutdown.clone();
  tokio::spawn(async move {
    connector::run(config, stop).await.unwrap();
  });
  tokio::time::timeout(Duration::from_secs(10), async {
    while !hub.tunnels.online(&host_id) {
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await?;
  println!("WebRTC synthetic fixture ready at {hub_url}/smoke/config");
  tokio::signal::ctrl_c().await?;
  shutdown.cancel();
  Ok(())
}
