use std::path::Path;

use axum::{
  Router,
  body::{Body, to_bytes},
  http::{Method, Request, StatusCode, header},
};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokn_viewer_core::ViewerService;
use tower::ServiceExt;

const REQUEST_ID: &str = "9a1b2c3d-1234-4321-9876-0123456789ab";
const TOKEN: &str = "test-input-secret";

struct Fixture {
  app: Router,
  directory: tempfile::TempDir,
}

impl Fixture {
  fn new() -> Self {
    let directory = tempfile::tempdir().unwrap();
    // An isolated empty catalog deliberately cannot authorize local provider
    // sessions. These tests never initialize a real provider input transport.
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let (events, _) = broadcast::channel(16);
    Self {
      app: tokn_viewer_api::router(service, events, Some(TOKEN.into()), vec![], CancellationToken::new()),
      directory,
    }
  }

  async fn post(&self, command: &str, authorization: Option<&str>, body: String) -> axum::response::Response {
    let mut request = Request::builder()
      .method(Method::POST)
      .uri(format!("/api/v1/{command}"))
      .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = authorization {
      request = request.header(header::AUTHORIZATION, value);
    }
    self
      .app
      .clone()
      .oneshot(request.body(Body::from(body)).unwrap())
      .await
      .unwrap()
  }

  async fn authorized(&self, command: &str, payload: Value) -> (StatusCode, Value) {
    let response = self
      .post(command, Some(&format!("Bearer {TOKEN}")), payload.to_string())
      .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
  }
}

fn session_key(provider: &str, path: &Path) -> String {
  let payload = serde_json::to_vec(&json!({
    "version": 1, "provider": provider, "session_id": "unregistered-session", "source_path": path
  }))
  .unwrap();
  let encoded = payload.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
  format!("session.v1.{encoded}")
}

fn submission(key: &str, text: &str) -> Value {
  json!({ "request": { "session_key": key, "request_id": REQUEST_ID, "text": text } })
}

#[tokio::test]
async fn input_routes_require_authentication_before_parsing_or_target_validation() {
  let fixture = Fixture::new();
  for command in [
    "get_session_input_status",
    "submit_session_input",
    "get_session_control",
    "set_session_control",
    "cancel_session_turn",
  ] {
    for authorization in [None, Some("Bearer wrong-token"), Some(TOKEN)] {
      // Even invalid JSON must first pass the same auth middleware as reads.
      let response = fixture.post(command, authorization, "{".into()).await;
      assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{command}");
      let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
      assert_eq!(body["error"], "Invalid viewer API token");
    }
    let response = fixture
      .post(command, Some(&format!("Bearer {TOKEN}")), "{".into())
      .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{command}");

    let response = fixture
      .app
      .clone()
      .oneshot(
        Request::builder()
          .method(Method::GET)
          .uri(format!("/api/v1/{command}"))
          .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(
      response.status(),
      StatusCode::METHOD_NOT_ALLOWED,
      "{command} requires POST"
    );
  }
}

#[tokio::test]
async fn input_routes_reject_malformed_request_envelopes() {
  let fixture = Fixture::new();
  for command in [
    "get_session_input_status",
    "submit_session_input",
    "get_session_control",
    "set_session_control",
    "cancel_session_turn",
  ] {
    for payload in [
      json!({}),
      json!({"request": null}),
      json!({"request": {"session_key": 42}}),
    ] {
      let (status, body) = fixture.authorized(command, payload).await;
      assert_eq!(status, StatusCode::BAD_REQUEST, "{command}");
      assert!(body["error"].as_str().is_some_and(|message| !message.is_empty()));
    }
  }
  for payload in [
    json!({"request": {"session_key": "forged", "text": "missing id"}}),
    json!({"request": {"session_key": "forged", "request_id": REQUEST_ID, "text": ["not text"]}}),
  ] {
    let (status, body) = fixture.authorized("submit_session_input", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
  }
}

#[tokio::test]
async fn invalid_message_bodies_fail_before_delivery_admission() {
  let fixture = Fixture::new();
  for text in [" \n\t".to_owned(), "hello\0world".into(), "🦀".repeat(16_385)] {
    let (status, body) = fixture
      .authorized("submit_session_input", submission("forged", &text))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "not_sent");
    assert_eq!(body["request_id"], REQUEST_ID);
    assert!(body["message"].is_string());
  }
  for request in [
    json!({"session_key":"forged", "request_id":"not-a-uuid", "text":"hello"}),
    json!({"session_key":"forged", "request_id":REQUEST_ID, "text":"hello", "image_ids":["invalid"]}),
  ] {
    let (status, body) = fixture
      .authorized("submit_session_input", json!({"request":request}))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "not_sent");
    assert!(body["message"].is_string());
  }
}

#[tokio::test]
async fn forged_and_unregistered_session_keys_never_authorize_delivery() {
  let fixture = Fixture::new();
  let private_file = fixture.directory.path().join("private-session.jsonl");
  std::fs::write(&private_file, "private-session-body-must-not-be-read").unwrap();
  let forged_keys = [
    "../../private-session.jsonl".to_owned(),
    session_key("codex", &private_file),
    session_key("pi", &private_file),
    session_key("dsh", &private_file),
  ];
  for key in forged_keys {
    let (status, body) = fixture
      .authorized("get_session_input_status", json!({"request": {"session_key": key}}))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], false);
    assert_eq!(body["max_length"], 16_384);
    assert!(!body.to_string().contains("private-session"));

    let (status, body) = fixture
      .authorized("submit_session_input", submission(&key, "hello"))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["request_id"], REQUEST_ID);
    assert_eq!(body["status"], "not_sent");
    assert!(!body.to_string().contains("private-session"));
  }
}

#[tokio::test]
async fn unsupported_providers_return_read_only_status_and_no_admission() {
  let fixture = Fixture::new();
  for provider in ["zcode", "workbuddy"] {
    let key = session_key(provider, &fixture.directory.path().join("provider-session"));
    let (status, body) = fixture
      .authorized("get_session_input_status", json!({"request": {"session_key": key}}))
      .await;
    assert_eq!(status, StatusCode::OK, "{provider}");
    assert_eq!(body["available"], false, "{provider}");
    assert!(
      body["message"]
        .as_str()
        .unwrap()
        .contains("no compatible live history binding")
    );
    let (status, body) = fixture
      .authorized("submit_session_input", submission(&key, "hello"))
      .await;
    assert_eq!(status, StatusCode::OK, "{provider}");
    assert_eq!(body["status"], "not_sent", "{provider}");
    assert_eq!(body["request_id"], REQUEST_ID);
    assert!(
      body["message"]
        .as_str()
        .unwrap()
        .contains("no compatible live history binding")
    );
  }
}

#[tokio::test]
async fn opencode_unreadable_and_unregistered_keys_never_authorize_delivery() {
  let fixture = Fixture::new();
  let private_file = fixture.directory.path().join("private-opencode.db");
  std::fs::write(&private_file, "private-opencode-content-must-not-be-read").unwrap();
  for path in [private_file, fixture.directory.path().join("missing-opencode.db")] {
    let key = session_key("opencode", &path);
    let (status, body) = fixture
      .authorized("get_session_input_status", json!({"request":{"session_key":key}}))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], false);
    assert!(!body.to_string().contains("private-opencode"));
    assert!(!body.to_string().contains("missing-opencode"));
    let (status, body) = fixture
      .authorized("submit_session_input", submission(&key, "hello"))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "not_sent");
    assert_eq!(body["request_id"], REQUEST_ID);
    assert!(!body.to_string().contains("private-opencode"));
    assert!(!body.to_string().contains("missing-opencode"));
  }
}
