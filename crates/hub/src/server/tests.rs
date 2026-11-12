use super::*;
use crate::auth::test_support::{login_for_test, post_json};
use axum::body::{Body, to_bytes};
use std::time::Duration;
use tokio::task::JoinHandle;
use tower::ServiceExt;

const ORIGIN: &str = "http://localhost:5559";

mod presence;

#[tokio::test]
async fn discovery_lists_claimed_machines_only_for_the_correct_account_group() {
  use tokn_hub_client_core::discovery::{DiscoveryGroup, DiscoveryResult};

  let directory = tempfile::tempdir().unwrap();
  let state = state(directory.path());
  let host = crate::store::HostRecord {
    host_id: uuid::Uuid::new_v4().to_string(),
    name: "Laptop".into(),
    public_key: protocol::encode(&[1; 32]),
    access: "view".into(),
  };
  let assignment = state.store.claim_fixture_host(host.clone());
  let group = assignment.group;
  let app = router(state.clone());
  let path = format!("/hub/v1/discovery/{}/{}", group.namespace, group.secret_digest);
  let response = get(app.clone(), &path, None).await;
  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
  let result: DiscoveryResult = serde_json::from_value(json_body(response).await).unwrap();
  result.validate_for(&group).unwrap();
  assert_eq!(result.machines.len(), 1);
  assert_eq!(result.machines[0].host_id, host.host_id);
  assert!(!result.machines[0].online);
  let wrong_namespace = DiscoveryGroup {
    namespace: "other".into(),
    ..group.clone()
  };
  let wrong_digest = DiscoveryGroup {
    secret_digest: protocol::encode(&[7; 32]),
    ..group.clone()
  };
  for other in [wrong_namespace, wrong_digest] {
    let path = format!("/hub/v1/discovery/{}/{}", other.namespace, other.secret_digest);
    assert_eq!(
      json_body(get(app.clone(), &path, None).await).await["machines"],
      json!([])
    );
  }
  assert_eq!(
    get(app.clone(), "/hub/v1/discovery/fixture/invalid", None)
      .await
      .status(),
    StatusCode::BAD_REQUEST
  );
  state.store.remove_host(&host.host_id).unwrap();
  assert_eq!(json_body(get(app, &path, None).await).await["machines"], json!([]));
}

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
  for path in ["/hub/v1/hosts", "/hub/v1/account/machines"] {
    assert_eq!(
      get(app.clone(), path, None).await.status(),
      StatusCode::UNAUTHORIZED,
      "{path}"
    );
  }
  for path in [
    "/hub/v1/typo",
    "/hub/v1/enrollments",
    "/hub/v1/namespaces",
    "/hosts/example/api/v1/health",
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
async fn existing_machine_enters_catalog_and_resolves_only_after_account_claim() {
  let directory = tempfile::tempdir().unwrap();
  let state = state(directory.path());
  let existing = crate::store::HostRecord {
    host_id: uuid::Uuid::new_v4().to_string(),
    name: "Workstation".into(),
    public_key: protocol::encode(&[1; 32]),
    access: "view".into(),
  };
  state.store.register_encrypted_host(existing.clone()).unwrap();
  let app = router(state.clone());
  let token = login_for_test(app.clone(), &state.auth).await;
  let catalog_path = "/hub/v1/hosts";
  let resolve_path = format!("/hub/v1/resolve/fixture/{}", existing.host_id);
  assert_eq!(
    json_body(get(app.clone(), catalog_path, Some(&token)).await).await,
    json!({"hosts": []})
  );
  assert_eq!(
    get(app.clone(), &resolve_path, None).await.status(),
    StatusCode::NOT_FOUND
  );

  let claim = state.store.claim_fixture_host(existing.clone());
  let public = get(app.clone(), &resolve_path, None).await;
  assert_eq!(public.status(), StatusCode::OK);
  assert_eq!(public.headers()[header::CACHE_CONTROL], "no-store");
  assert_eq!(
    json_body(public).await,
    json!({
      "host_id": existing.host_id,
      "machine_address": claim.machine_address,
      "name": "Workstation",
      "online": false
    })
  );
  let catalog = json_body(get(app.clone(), catalog_path, Some(&token)).await).await;
  assert_eq!(catalog["hosts"][0]["machine_address"], claim.machine_address);
  assert_eq!(catalog["hosts"][0]["online"], false);
  assert_eq!(
    get(app.clone(), "/hub/v1/resolve/Fixture/workstation", None)
      .await
      .status(),
    StatusCode::BAD_REQUEST
  );
  let wrong_origin = Request::builder()
    .uri(resolve_path.as_str())
    .header(header::ORIGIN, "https://untrusted.example")
    .body(Body::empty())
    .unwrap();
  assert_eq!(
    app.clone().oneshot(wrong_origin).await.unwrap().status(),
    StatusCode::FORBIDDEN
  );

  let removal = Request::builder()
    .method("DELETE")
    .uri(format!("/hub/v1/hosts/{}", existing.host_id))
    .header(header::AUTHORIZATION, format!("Bearer {token}"))
    .body(Body::empty())
    .unwrap();
  assert_eq!(
    app.clone().oneshot(removal).await.unwrap().status(),
    StatusCode::NO_CONTENT
  );
  assert_eq!(get(app, &resolve_path, None).await.status(), StatusCode::NOT_FOUND);
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
