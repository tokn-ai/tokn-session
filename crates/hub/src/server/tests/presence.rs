use super::*;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::SinkExt;
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream, connect_async,
  tungstenite::{Message, client::IntoClientRequest},
};
use tokn_hub_client_core::presence::{MAX_PRESENCE_HOSTS, PresenceResult};

const FIRST: &str = "550e8400-e29b-41d4-a716-446655440000";
const SECOND: &str = "550e8400-e29b-41d4-a716-446655440001";

async fn registered_socket(
  hub_url: &str,
  connector_version: Option<&str>,
) -> WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>> {
  let endpoint = format!("{}/hub/v1/tunnel", hub_url.replacen("http://", "ws://", 1));
  let mut request = endpoint.into_client_request().unwrap();
  if let Some(version) = connector_version {
    request
      .headers_mut()
      .insert(CONNECTOR_VERSION_HEADER, version.parse().unwrap());
  }
  let (mut socket, response) = connect_async(request).await.unwrap();
  assert_eq!(response.headers()[HUB_VERSION_HEADER], env!("CARGO_PKG_VERSION"));
  let challenge: protocol::Frame =
    serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
  let protocol::Frame::Challenge { nonce, .. } = challenge else {
    panic!("Expected challenge")
  };
  let key = SigningKey::from_bytes(&[7; 32]);
  let public_key = protocol::encode(key.verifying_key().as_bytes());
  let proof = protocol::registration_proof(&nonce, FIRST, &public_key, "Workstation", false);
  let registration = protocol::Frame::Register {
    version: protocol::VERSION,
    host_id: FIRST.into(),
    public_key,
    name: "Workstation".into(),
    allow_control: false,
    discovery_group: None,
    signature: protocol::encode(&key.sign(&proof).to_bytes()),
  };
  socket
    .send(Message::Text(serde_json::to_string(&registration).unwrap().into()))
    .await
    .unwrap();
  let ready: protocol::Frame = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
  assert!(matches!(ready, protocol::Frame::Ready { .. }));
  socket
}

async fn versioned_presence(app: Router, payload: &Value) -> Value {
  let request = Request::builder()
    .method("POST")
    .uri("/hub/v1/presence")
    .header(header::CONTENT_TYPE, "application/json")
    .header(PRESENCE_VERSIONS_HEADER, "1")
    .body(Body::from(payload.to_string()))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  json_body(response).await
}

#[tokio::test]
async fn presence_tracks_connect_disconnect_and_revocation_without_exposing_identity_metadata() {
  let directory = tempfile::tempdir().unwrap();
  let state = state(directory.path());
  let app = router(state.clone());
  let ids = vec![FIRST.to_string(), SECOND.to_string()];
  let payload = json!({"host_ids":ids});
  let (status, body) = post_json(app.clone(), "/hub/v1/presence", None, None, payload.clone()).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(
    body,
    json!({"machines":[{"host_id":FIRST,"online":false},{"host_id":SECOND,"online":false}]})
  );
  let result: PresenceResult = serde_json::from_value(body).unwrap();
  result.validate_for(&ids).unwrap();
  let (hub_url, task) = listen(app.clone()).await;
  let mut socket = registered_socket(&hub_url, Some("0.2.0-rc.1")).await;
  assert!(state.tunnels.online(FIRST));
  let (status, body) = post_json(app.clone(), "/hub/v1/presence", Some(ORIGIN), None, payload.clone()).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(
    body,
    json!({"machines":[{"host_id":FIRST,"online":true},{"host_id":SECOND,"online":false}]})
  );
  let body = versioned_presence(app.clone(), &payload).await;
  assert_eq!(
    body,
    json!({"machines":[{"host_id":FIRST,"online":true,"connector_version":"0.2.0-rc.1"},{"host_id":SECOND,"online":false}]})
  );
  socket.close(None).await.unwrap();
  wait_until(|| !state.tunnels.online(FIRST)).await;
  let body = versioned_presence(app.clone(), &payload).await;
  assert_eq!(body["machines"][0], json!({"host_id":FIRST,"online":false}));
  let _old_socket = registered_socket(&hub_url, None).await;
  let body = versioned_presence(app.clone(), &payload).await;
  assert_eq!(body["machines"][0], json!({"host_id":FIRST,"online":true}));
  let _socket = registered_socket(&hub_url, Some("0.2.0")).await;
  let body = versioned_presence(app.clone(), &payload).await;
  assert_eq!(body["machines"][0]["connector_version"], "0.2.0");
  state.tunnels.revoke(FIRST).unwrap();
  let body = versioned_presence(app, &payload).await;
  assert_eq!(body["machines"][0], json!({"host_id":FIRST,"online":false}));
  task.abort();
}

#[tokio::test]
async fn presence_checks_origin_validates_bounded_batches_and_allows_empty_native_requests() {
  let directory = tempfile::tempdir().unwrap();
  let app = router(state(directory.path()));
  for origin in [None, Some(ORIGIN)] {
    assert_eq!(
      post_json(app.clone(), "/hub/v1/presence", origin, None, json!({"host_ids":[]})).await,
      (StatusCode::OK, json!({"machines":[]}))
    );
  }
  for origin in ["https://untrusted.example", "null", "http://localhost:5560"] {
    let (status, _) = post_json(
      app.clone(),
      "/hub/v1/presence",
      Some(origin),
      None,
      json!({"host_ids":[FIRST]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
  }
  for payload in [
    json!({"host_ids":[FIRST,FIRST]}),
    json!({"host_ids":["not-a-uuid"]}),
    json!({"host_ids":[FIRST.to_uppercase()]}),
    json!({"host_ids":[] , "all":true}),
    json!({"host_ids":"all"}),
    json!({}),
    json!({"host_ids":(0..=MAX_PRESENCE_HOSTS).map(|id| format!("550e8400-e29b-41d4-a716-{id:012x}")).collect::<Vec<_>>()}),
  ] {
    let (status, _) = post_json(app.clone(), "/hub/v1/presence", None, None, payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
  }
  let ids = (0..MAX_PRESENCE_HOSTS)
    .map(|id| format!("550e8400-e29b-41d4-a716-{id:012x}"))
    .collect::<Vec<_>>();
  let (status, body) = post_json(app.clone(), "/hub/v1/presence", None, None, json!({"host_ids":ids})).await;
  assert_eq!(status, StatusCode::OK);
  serde_json::from_value::<PresenceResult>(body)
    .unwrap()
    .validate_for(&ids)
    .unwrap();
  let oversized = Request::builder()
    .method("POST")
    .uri("/hub/v1/presence")
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(format!("{{\"host_ids\":[]}}{}", " ".repeat(8 * 1024))))
    .unwrap();
  assert_eq!(
    app.clone().oneshot(oversized).await.unwrap().status(),
    StatusCode::PAYLOAD_TOO_LARGE
  );
  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/hub/v1/presence")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{broken"))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}
