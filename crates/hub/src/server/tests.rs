use super::*;
use crate::{
  auth::test_support::{login_for_test, post_json},
  connector::{self, ConnectorConfig},
};
use axum::body::to_bytes;
use std::{
  sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  },
  time::Duration,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const ORIGIN: &str = "http://localhost:5559";

fn state(directory: &std::path::Path) -> HubState {
  HubState::new(Store::open(directory.join("hub.sqlite")).unwrap(), ORIGIN).unwrap()
}

async fn get(app: Router, path: &str, token: Option<&str>) -> Response {
  let mut request = Request::builder().uri(path);
  if let Some(token) = token {
    request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
  }
  app.oneshot(request.body(Body::empty()).unwrap()).await.unwrap()
}

async fn json_body(response: Response) -> Value {
  serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

async fn listen(app: Router) -> (String, JoinHandle<()>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let origin = format!("http://{}", listener.local_addr().unwrap());
  let task = tokio::spawn(async move {
    axum::serve(listener, app).await.unwrap();
  });
  (origin, task)
}

async fn wait_until(mut ready: impl FnMut() -> bool) {
  tokio::time::timeout(Duration::from_secs(5), async {
    while !ready() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("condition did not become ready");
}

#[tokio::test]
async fn unauthenticated_routes_and_unknown_api_paths_never_serve_the_spa() {
  let directory = tempfile::tempdir().unwrap();
  let state = state(directory.path());
  let web = directory.path().join("web");
  std::fs::create_dir(&web).unwrap();
  std::fs::write(web.join("index.html"), "<main>Hub</main>").unwrap();
  let app = with_web_ui(router(state), web).unwrap();
  for path in ["/hub/v1/hosts", "/hub/v1/enrollments", "/hosts/example/api/v1/health"] {
    assert_eq!(
      get(app.clone(), path, None).await.status(),
      StatusCode::UNAUTHORIZED,
      "{path}"
    );
  }
  for path in [
    "/hub/v1/typo",
    "/hosts/example/typo",
    "/api/v1/health",
    "/hub/",
    "/hosts/",
    "/hosts//bad",
    "/api/",
  ] {
    assert_eq!(
      get(app.clone(), path, None).await.status(),
      StatusCode::NOT_FOUND,
      "{path}"
    );
  }
  let page = get(app.clone(), "/", None).await;
  assert_eq!(page.status(), StatusCode::OK);
  assert_eq!(page.headers()[header::X_FRAME_OPTIONS], "DENY");
  assert_eq!(page.headers()[header::REFERRER_POLICY], "no-referrer");
  let status = get(app, "/hub/v1/auth/status", None).await;
  assert_eq!(status.headers()[header::CACHE_CONTROL], "no-store");
  assert_eq!(
    json_body(status).await,
    json!({"configured": false, "authenticated": false})
  );
}

#[tokio::test]
async fn namespaces_are_admin_owned_and_public_discovery_never_returns_a_key() {
  let directory = tempfile::tempdir().unwrap();
  let state = state(directory.path());
  let first = crate::store::HostRecord {
    host_id: uuid::Uuid::new_v4().to_string(),
    name: "Workstation".into(),
    public_key: crate::protocol::encode(&[1; 32]),
    access: "view".into(),
  };
  let second = crate::store::HostRecord {
    host_id: uuid::Uuid::new_v4().to_string(),
    name: "Laptop".into(),
    public_key: crate::protocol::encode(&[2; 32]),
    access: "view".into(),
  };
  let legacy = crate::store::HostRecord {
    host_id: uuid::Uuid::new_v4().to_string(),
    name: "Legacy".into(),
    public_key: crate::protocol::encode(&[3; 32]),
    access: "view".into(),
  };
  state.store.register_encrypted_host(first.clone()).unwrap();
  state.store.register_encrypted_host(second.clone()).unwrap();
  state.store.approve_host(legacy.clone()).unwrap();
  let app = router(state.clone());
  assert_eq!(
    get(app.clone(), "/hub/v1/namespaces", None).await.status(),
    StatusCode::UNAUTHORIZED
  );
  assert_eq!(
    post_json(
      app.clone(),
      "/hub/v1/namespaces",
      Some(ORIGIN),
      None,
      json!({"username": "alice"})
    )
    .await
    .0,
    StatusCode::UNAUTHORIZED
  );
  let binding_path = "/hub/v1/namespaces/alice/machines/workstation";
  assert_eq!(
    post_json(
      app.clone(),
      binding_path,
      Some(ORIGIN),
      None,
      json!({"host_id": first.host_id})
    )
    .await
    .0,
    StatusCode::UNAUTHORIZED
  );
  let token = login_for_test(app.clone(), &state.auth).await;
  assert_eq!(
    post_json(
      app.clone(),
      "/hub/v1/namespaces",
      Some("https://untrusted.example"),
      Some(&token),
      json!({"username": "alice"})
    )
    .await
    .0,
    StatusCode::FORBIDDEN
  );
  assert!(state.store.namespaces().unwrap().is_empty());
  for username in ["Alice", "a_b", "-alice", "alice-", "alice:other"] {
    assert_eq!(
      post_json(
        app.clone(),
        "/hub/v1/namespaces",
        Some(ORIGIN),
        Some(&token),
        json!({"username": username})
      )
      .await
      .0,
      StatusCode::BAD_REQUEST
    );
  }
  assert_eq!(
    post_json(
      app.clone(),
      "/hub/v1/namespaces",
      Some(ORIGIN),
      Some(&token),
      json!({"username": "alice", "owner_id": "attacker"})
    )
    .await
    .0,
    StatusCode::BAD_REQUEST
  );
  assert_eq!(
    post_json(
      app.clone(),
      "/hub/v1/namespaces",
      Some(ORIGIN),
      Some(&token),
      json!({"username": "alice"})
    )
    .await,
    (StatusCode::OK, json!({"username": "alice"}))
  );
  assert_eq!(
    post_json(
      app.clone(),
      "/hub/v1/namespaces",
      Some(ORIGIN),
      Some(&token),
      json!({"username": "alice"})
    )
    .await
    .0,
    StatusCode::CONFLICT
  );
  assert_eq!(
    json_body(get(app.clone(), "/hub/v1/namespaces", Some(&token)).await).await,
    json!({"namespaces": [{"username": "alice"}]})
  );

  for (path, host_id, status) in [
    (
      "/hub/v1/namespaces/unknown/machines/workstation",
      first.host_id.as_str(),
      StatusCode::NOT_FOUND,
    ),
    (
      "/hub/v1/namespaces/alice/machines/Workstation",
      first.host_id.as_str(),
      StatusCode::BAD_REQUEST,
    ),
    (
      "/hub/v1/namespaces/alice/machines/legacy",
      legacy.host_id.as_str(),
      StatusCode::NOT_FOUND,
    ),
    (binding_path, "not-a-uuid", StatusCode::BAD_REQUEST),
  ] {
    assert_eq!(
      post_json(
        app.clone(),
        path,
        Some(ORIGIN),
        Some(&token),
        json!({"host_id": host_id})
      )
      .await
      .0,
      status,
      "{path}"
    );
  }
  let binding = post_json(
    app.clone(),
    binding_path,
    Some(ORIGIN),
    Some(&token),
    json!({"host_id": first.host_id}),
  )
  .await;
  let expected =
    json!({"host_id": first.host_id, "machine_address": "alice:workstation", "name": "Workstation", "online": false});
  assert_eq!(binding, (StatusCode::OK, expected.clone()));
  assert_eq!(
    post_json(
      app.clone(),
      binding_path,
      Some(ORIGIN),
      Some(&token),
      json!({"host_id": second.host_id})
    )
    .await
    .0,
    StatusCode::CONFLICT
  );
  assert_eq!(
    post_json(
      app.clone(),
      "/hub/v1/namespaces/alice/machines/other",
      Some(ORIGIN),
      Some(&token),
      json!({"host_id": first.host_id})
    )
    .await
    .0,
    StatusCode::CONFLICT
  );
  let public = get(app.clone(), "/hub/v1/resolve/alice/workstation", None).await;
  assert_eq!(public.status(), StatusCode::OK);
  assert_eq!(public.headers()[header::CACHE_CONTROL], "no-store");
  assert_eq!(json_body(public).await, expected);
  assert_eq!(
    get(app.clone(), "/hub/v1/resolve/alice/unknown", None).await.status(),
    StatusCode::NOT_FOUND
  );
  assert_eq!(
    get(app.clone(), "/hub/v1/resolve/Alice/workstation", None)
      .await
      .status(),
    StatusCode::BAD_REQUEST
  );
  let wrong_origin = Request::builder()
    .uri("/hub/v1/resolve/alice/workstation")
    .header(header::ORIGIN, "https://untrusted.example")
    .body(Body::empty())
    .unwrap();
  assert_eq!(
    app.clone().oneshot(wrong_origin).await.unwrap().status(),
    StatusCode::FORBIDDEN
  );
  let catalog = json_body(get(app.clone(), "/hub/v1/hosts", Some(&token)).await).await;
  let registered = catalog["hosts"]
    .as_array()
    .unwrap()
    .iter()
    .find(|host| host["host_id"] == first.host_id)
    .unwrap();
  assert_eq!(registered["secure_only"], true);
  assert_eq!(registered["online"], false);
  assert_eq!(registered["machine_address"], "alice:workstation");
  let legacy_host = catalog["hosts"]
    .as_array()
    .unwrap()
    .iter()
    .find(|host| host["host_id"] == legacy.host_id)
    .unwrap();
  assert_eq!(legacy_host["secure_only"], false);
  assert!(legacy_host.get("machine_address").is_none());

  let removal = Request::builder()
    .method("DELETE")
    .uri(format!("/hub/v1/hosts/{}", first.host_id))
    .header(header::AUTHORIZATION, format!("Bearer {token}"))
    .body(Body::empty())
    .unwrap();
  assert_eq!(
    app.clone().oneshot(removal).await.unwrap().status(),
    StatusCode::NO_CONTENT
  );
  assert_eq!(
    get(app.clone(), "/hub/v1/resolve/alice/workstation", None)
      .await
      .status(),
    StatusCode::NOT_FOUND
  );
  assert_eq!(
    post_json(
      app.clone(),
      binding_path,
      Some(ORIGIN),
      Some(&token),
      json!({"host_id": second.host_id})
    )
    .await
    .0,
    StatusCode::CONFLICT
  );
  assert_eq!(
    post_json(
      app.clone(),
      "/hub/v1/auth/logout",
      Some(ORIGIN),
      Some(&token),
      json!({})
    )
    .await
    .0,
    StatusCode::NO_CONTENT
  );
  assert_eq!(
    post_json(
      app,
      "/hub/v1/namespaces",
      Some(ORIGIN),
      Some(&token),
      json!({"username": "after-logout"})
    )
    .await
    .0,
    StatusCode::UNAUTHORIZED
  );
}

#[tokio::test]
async fn only_the_native_passkey_page_preserves_the_origin_for_its_form_callback() {
  let directory = tempfile::tempdir().unwrap();
  let web = directory.path().join("web");
  std::fs::create_dir(&web).unwrap();
  std::fs::write(web.join("index.html"), "<main>Hub</main>").unwrap();
  let app = with_web_ui(router(state(directory.path())), web).unwrap();
  for (path, policy) in [
    ("/passkey", "origin"),
    ("/passkey?request=ignored", "origin"),
    ("/", "no-referrer"),
    ("/admin", "no-referrer"),
    ("/connect", "no-referrer"),
    ("/passkey/", "no-referrer"),
    ("/passkey-other", "no-referrer"),
    ("/hub/v1/auth/status", "no-referrer"),
  ] {
    let response = get(app.clone(), path, None).await;
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    assert_eq!(response.headers()[header::REFERRER_POLICY], policy, "{path}");
  }
}

#[tokio::test]
async fn browser_secure_channels_require_the_exact_ui_origin_and_connectors_remain_native_only() {
  use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error, client::IntoClientRequest},
  };
  let directory = tempfile::tempdir().unwrap();
  let (hub_url, task) = listen(router(state(directory.path()))).await;
  let websocket_origin = hub_url.replacen("http://", "ws://", 1);
  let cases = [
    ("/hub/v1/secure/offline", None, StatusCode::BAD_GATEWAY),
    ("/hub/v1/secure/offline", Some(ORIGIN), StatusCode::BAD_GATEWAY),
    (
      "/hub/v1/secure/offline",
      Some("http://localhost:5560"),
      StatusCode::FORBIDDEN,
    ),
    (
      "/hub/v1/secure/offline",
      Some("https://untrusted.example"),
      StatusCode::FORBIDDEN,
    ),
    ("/hub/v1/secure/offline", Some("null"), StatusCode::FORBIDDEN),
    ("/hub/v1/tunnel", Some(ORIGIN), StatusCode::FORBIDDEN),
  ];
  for (path, origin, status) in cases {
    let mut request = format!("{websocket_origin}{path}").into_client_request().unwrap();
    if let Some(origin) = origin {
      request.headers_mut().insert(header::ORIGIN, origin.parse().unwrap());
    }
    let Err(Error::Http(response)) = connect_async(request).await else {
      panic!("Expected boundary rejection for {path} from {origin:?}");
    };
    assert_eq!(response.status(), status, "{path}, {origin:?}");
  }
  task.abort();
}

#[tokio::test]
async fn passkey_owner_routes_multiple_hosts_and_logout_stops_live_streams() {
  let directory = tempfile::tempdir().unwrap();
  let state = state(directory.path());
  let app = router(state.clone());
  let token = login_for_test(app.clone(), &state.auth).await;
  let (hub_url, hub_task) = listen(app.clone()).await;
  let submissions = Arc::new(AtomicUsize::new(0));
  let writes = submissions.clone();
  let local = Router::new()
    .route(
      "/api/v1/health",
      axum::routing::get(|| async { Json(json!({"version": 1})) }),
    )
    .route(
      "/api/v1/list_sessions",
      post(|headers: HeaderMap| async move {
        Json(json!({"local_authorization": headers.get(header::AUTHORIZATION).unwrap().to_str().unwrap()}))
      }),
    )
    .route(
      "/api/v1/submit_session_input",
      post(move || {
        let writes = writes.clone();
        async move {
          writes.fetch_add(1, Ordering::SeqCst);
          Json(json!({"status": "accepted"}))
        }
      }),
    )
    .route(
      "/api/v1/events",
      axum::routing::get(|| async {
        let stream = async_stream::stream! {
          yield Ok::<_, std::io::Error>(Bytes::from_static(b"event: ready\ndata: {}\n\n"));
          std::future::pending::<()>().await;
        };
        Response::builder()
          .header(header::CONTENT_TYPE, "text/event-stream")
          .body(Body::from_stream(stream))
          .unwrap()
      }),
    );
  let (local_url, local_task) = listen(local).await;
  let stop = CancellationToken::new();
  let mut connectors = Vec::new();
  for (name, allow_control) in [("View host", false), ("Control host", true)] {
    let config = ConnectorConfig {
      hub_url: hub_url.parse().unwrap(),
      local_url: local_url.parse().unwrap(),
      key_file: directory.path().join(format!("{name}.key")),
      name: name.into(),
      local_token: Some("local-only-token".into()),
      allow_control,
      insecure_loopback: true,
      ice_servers: Vec::new(),
      secure: None,
      paired: None,
    };
    connectors.push(tokio::spawn(connector::run(config, stop.clone())));
  }
  wait_until(|| state.tunnels.pending().len() == 2).await;
  let pending = json_body(get(app.clone(), "/hub/v1/enrollments", Some(&token)).await).await;
  for enrollment in pending["enrollments"].as_array().unwrap() {
    let request = json!({"pairing_code": enrollment["pairing_code"]});
    let (status, _) = post_json(
      app.clone(),
      "/hub/v1/enrollments/approve",
      Some(ORIGIN),
      Some(&token),
      request,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
  }
  let enrolled = state.store.hosts().unwrap();
  wait_until(|| enrolled.iter().all(|host| state.tunnels.online(&host.host_id))).await;
  let host_list = json_body(get(app.clone(), "/hub/v1/hosts", Some(&token)).await).await;
  assert_eq!(host_list["hosts"].as_array().unwrap().len(), 2);
  assert!(
    host_list["hosts"]
      .as_array()
      .unwrap()
      .iter()
      .all(|host| host["online"] == true)
  );

  for host in &enrolled {
    let path = format!("/hosts/{}/api/v1/list_sessions", host.host_id);
    let (status, result) = post_json(app.clone(), &path, Some(ORIGIN), Some(&token), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["local_authorization"], "Bearer local-only-token");
    let path = format!("/hosts/{}/api/v1/submit_session_input", host.host_id);
    let (status, _) = post_json(app.clone(), &path, Some(ORIGIN), Some(&token), json!({})).await;
    assert_eq!(
      status,
      if host.access == "control" {
        StatusCode::OK
      } else {
        StatusCode::FORBIDDEN
      }
    );
    let (status, _) = post_json(
      app.clone(),
      &path,
      Some("https://unrelated.example"),
      Some(&token),
      json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
  }
  assert_eq!(submissions.load(Ordering::SeqCst), 1);
  let view = enrolled.iter().find(|host| host.access == "view").unwrap();
  let path = format!("/hosts/{}/api/v1/get_session_input_status", view.host_id);
  let (status, result) = post_json(app.clone(), &path, Some(ORIGIN), Some(&token), json!({})).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(result["available"], false);

  let path = format!("/hosts/{}/api/v1/events", view.host_id);
  let response = get(app.clone(), &path, Some(&token)).await;
  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream");
  let mut stream = response.into_body().into_data_stream();
  let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
    .await
    .unwrap()
    .unwrap()
    .unwrap();
  assert!(String::from_utf8_lossy(&first).contains("event: ready"));
  let (status, _) = post_json(
    app.clone(),
    "/hub/v1/auth/logout",
    Some(ORIGIN),
    Some(&token),
    json!({}),
  )
  .await;
  assert_eq!(status, StatusCode::NO_CONTENT);
  assert!(
    tokio::time::timeout(Duration::from_secs(2), stream.next())
      .await
      .unwrap()
      .is_none()
  );
  assert_eq!(
    get(app, "/hub/v1/hosts", Some(&token)).await.status(),
    StatusCode::UNAUTHORIZED
  );

  stop.cancel();
  state.tunnels.shutdown();
  state.auth.shutdown();
  for connector in connectors {
    connector.await.unwrap().unwrap();
  }
  hub_task.abort();
  local_task.abort();
}
