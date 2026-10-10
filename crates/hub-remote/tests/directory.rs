use axum::{
  Json, Router,
  body::Body,
  extract::State,
  http::{StatusCode, header},
  response::{IntoResponse, Response},
  routing::get,
};
use serde_json::json;
use std::{
  convert::Infallible,
  sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
  },
};
use tokn_hub_client_core::secure::NoiseIdentity;
use tokn_hub_remote::{RemoteManager, ResolvedMachine, SavedHost};
use tokn_session_hub::onboarding::ClientStore;
use url::Url;

#[derive(Clone)]
struct Directory {
  resolved: Arc<Mutex<ResolvedMachine>>,
  mode: Arc<AtomicUsize>,
  lookups: Arc<AtomicUsize>,
  redirects: Arc<AtomicUsize>,
}

async fn lookup(State(state): State<Directory>) -> Response {
  state.lookups.fetch_add(1, Ordering::SeqCst);
  match state.mode.load(Ordering::SeqCst) {
    1 => (
      StatusCode::TEMPORARY_REDIRECT,
      [
        (header::LOCATION, "/redirected"),
        (header::CONTENT_TYPE, "application/json"),
      ],
      "{}",
    )
      .into_response(),
    2 => Json(json!({"padding":"x".repeat(5000)})).into_response(),
    3 => Response::builder()
      .header(header::CONTENT_TYPE, "application/json")
      .body(Body::from_stream(futures_util::stream::iter([
        Ok::<_, Infallible>("x".repeat(3000)),
        Ok("x".repeat(3000)),
      ])))
      .unwrap(),
    4 => "not JSON".into_response(),
    5 => (
      StatusCode::NOT_FOUND,
      Json(json!({"error":"Machine address is not registered"})),
    )
      .into_response(),
    6 => ([(header::CONTENT_TYPE, "application/json")], "{").into_response(),
    7 => {
      let mut value = serde_json::to_value(state.resolved.lock().unwrap().clone()).unwrap();
      value["host_public_key"] = "directory metadata must not supply a pin".into();
      Json(value).into_response()
    }
    _ => Json(state.resolved.lock().unwrap().clone()).into_response(),
  }
}

struct Harness {
  directory: tempfile::TempDir,
  hub_url: String,
  state: Directory,
  task: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
  fn drop(&mut self) {
    self.task.abort();
  }
}

impl Harness {
  async fn start() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let state = Directory {
      resolved: Arc::new(Mutex::new(ResolvedMachine {
        host_id: uuid::Uuid::new_v4().to_string(),
        machine_address: "alice:workstation".into(),
        name: "Workstation".into(),
        online: false,
      })),
      mode: Arc::new(AtomicUsize::new(0)),
      lookups: Arc::new(AtomicUsize::new(0)),
      redirects: Arc::new(AtomicUsize::new(0)),
    };
    let redirected = state.redirects.clone();
    let router = Router::new()
      .route("/hub/v1/resolve/alice/workstation", get(lookup))
      .route(
        "/redirected",
        get(move || {
          let redirected = redirected.clone();
          async move {
            redirected.fetch_add(1, Ordering::SeqCst);
            Json(json!({}))
          }
        }),
      )
      .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let task = tokio::spawn(async move {
      axum::serve(listener, router).await.unwrap();
    });
    Self {
      directory,
      hub_url,
      state,
      task,
    }
  }
  fn manager(&self) -> RemoteManager {
    RemoteManager::new(self.directory.path().join("app"))
  }
  fn store(&self) -> ClientStore {
    ClientStore::load_or_create(&self.directory.path().join("app"), &Url::parse(&self.hub_url).unwrap()).unwrap()
  }
}

#[tokio::test]
async fn offline_resolution_and_verified_metadata_preserve_pins_and_saved_selection_after_restart() {
  let harness = Harness::start().await;
  let manager = harness.manager();
  let resolved = manager.resolve(&harness.hub_url, "alice:workstation").await.unwrap();
  assert!(
    !resolved.online,
    "directory lookup does not depend on an online connector"
  );
  assert!(
    manager
      .remember_metadata(
        &harness.hub_url,
        &resolved.host_id,
        &resolved.machine_address,
        &resolved.name
      )
      .await
      .is_err()
  );
  assert_eq!(
    harness.state.lookups.load(Ordering::SeqCst),
    1,
    "unpaired metadata writes are rejected before lookup"
  );
  let host = SavedHost {
    host_id: resolved.host_id.clone(),
    host_public_key: NoiseIdentity::generate().unwrap().public_key(),
    machine_address: None,
    name: None,
  };
  let store = harness.store();
  store.save_host(host.clone()).unwrap();
  store.select_host(&host.host_id).unwrap();
  let remembered = manager
    .remember_metadata(&harness.hub_url, &host.host_id, "alice:workstation", "Workstation")
    .await
    .unwrap();
  assert_eq!(remembered.host_public_key, host.host_public_key);
  let restarted = harness.manager();
  let status = restarted.status(&harness.hub_url).await.unwrap();
  assert_eq!(status.hosts, vec![remembered.clone()]);
  assert_eq!(status.selected_host_id.as_deref(), Some(host.host_id.as_str()));
  harness.state.resolved.lock().unwrap().name = "Renamed workstation".into();
  let remembered = restarted
    .remember_metadata(&harness.hub_url, &host.host_id, "alice:workstation", "Workstation")
    .await
    .unwrap();
  assert_eq!(remembered.name.as_deref(), Some("Renamed workstation"));
  assert_eq!(remembered.host_public_key, host.host_public_key);
  let second = SavedHost {
    host_id: uuid::Uuid::new_v4().to_string(),
    host_public_key: NoiseIdentity::generate().unwrap().public_key(),
    machine_address: None,
    name: None,
  };
  store.save_host(second.clone()).unwrap();
  harness.state.resolved.lock().unwrap().host_id = second.host_id.clone();
  assert!(
    restarted
      .remember_metadata(&harness.hub_url, &host.host_id, "alice:workstation", "Workstation")
      .await
      .is_err(),
    "directory cannot move an existing saved alias to another UUID"
  );
  assert!(
    restarted
      .remember_metadata(&harness.hub_url, &second.host_id, "alice:workstation", "Workstation")
      .await
      .is_err(),
    "saved alias collision cannot overwrite the first host"
  );
  assert_eq!(store.hosts().unwrap()[0], remembered);
  assert_eq!(store.selected_host().unwrap().as_deref(), Some(host.host_id.as_str()));
}

#[tokio::test]
async fn native_directory_rejects_redirects_oversize_mime_and_mismatched_identity_metadata() {
  let harness = Harness::start().await;
  let manager = harness.manager();
  for (mode, expected) in [
    (1, "rejected lookup"),
    (2, "size limit"),
    (3, "size limit"),
    (4, "response type"),
    (5, "not registered"),
    (6, "invalid machine metadata"),
    (7, "invalid machine metadata"),
  ] {
    harness.state.mode.store(mode, Ordering::SeqCst);
    let error = manager
      .resolve(&harness.hub_url, "alice:workstation")
      .await
      .unwrap_err();
    assert!(error.contains(expected), "mode {mode}: {error}");
  }
  assert_eq!(
    harness.state.redirects.load(Ordering::SeqCst),
    0,
    "HTTP redirects must never be followed"
  );
  harness.state.mode.store(0, Ordering::SeqCst);
  harness.state.resolved.lock().unwrap().machine_address = "bob:workstation".into();
  assert!(
    manager
      .resolve(&harness.hub_url, "alice:workstation")
      .await
      .unwrap_err()
      .contains("different machine address")
  );
  harness.state.resolved.lock().unwrap().machine_address = "alice:workstation".into();
  harness.state.resolved.lock().unwrap().host_id = "not-a-uuid".into();
  assert!(
    manager
      .resolve(&harness.hub_url, "alice:workstation")
      .await
      .unwrap_err()
      .contains("UUID")
  );
  let lookups = harness.state.lookups.load(Ordering::SeqCst);
  assert!(manager.resolve(&harness.hub_url, "Alice:workstation").await.is_err());
  assert_eq!(
    harness.state.lookups.load(Ordering::SeqCst),
    lookups,
    "invalid addresses are rejected before network access"
  );
}
