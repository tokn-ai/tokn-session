use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

use super::*;

const ORIGIN: &str = "https://hub.example.test";
const EMAIL: &str = "person@example.test";

fn fixture() -> (TempDir, Accounts, Router) {
  let directory = TempDir::new().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let state = Accounts::with_mailer(store, ORIGIN, Some(Sendmail::test_delivery())).unwrap();
  let router = state.router();
  (directory, state, router)
}

async fn request(app: &Router, method: &str, path: &str, body: Value, token: Option<&str>) -> (StatusCode, Value) {
  let mut request = Request::builder()
    .method(method)
    .uri(format!("/hub/v1/account/{path}"))
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::ORIGIN, ORIGIN);
  if let Some(token) = token {
    request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
  }
  let response = app
    .clone()
    .oneshot(request.body(Body::from(body.to_string())).unwrap())
    .await
    .unwrap();
  let status = response.status();
  assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
  let body = response.into_body().collect().await.unwrap().to_bytes();
  (
    status,
    if body.is_empty() {
      Value::Null
    } else {
      serde_json::from_slice(&body).unwrap()
    },
  )
}

async fn signup(app: &Router, state: &Accounts, email: &str, username: &str) -> Value {
  let (code, _) = state.issue_email_code(email).unwrap();
  let (status, value) = request(
    app,
    "POST",
    "email/verify",
    json!({ "email": email, "code": code, "username": username }),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  value
}

#[tokio::test]
async fn email_code_creates_one_identity_and_can_only_be_used_once() {
  let (_directory, state, app) = fixture();
  state.inner.store.create_namespace("reserved").unwrap();
  let (code, _) = state.issue_email_code(EMAIL).unwrap();
  let request_body = json!({"email": EMAIL, "code": code});
  let (status, body) = request(&app, "POST", "email/verify", request_body.clone(), None).await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(body["username_required"], true);
  let (status, _) = request(
    &app,
    "POST",
    "email/verify",
    json!({"email": EMAIL, "code": code, "username": "reserved"}),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::CONFLICT);
  let (status, first) = request(
    &app,
    "POST",
    "email/verify",
    json!({"email": EMAIL, "code": code, "username": "alice"}),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(first["username"], "alice");
  assert!(Uuid::parse_str(first["user_id"].as_str().unwrap()).is_ok());

  assert_eq!(
    request(&app, "POST", "email/verify", request_body, None).await.0,
    StatusCode::UNAUTHORIZED
  );
  assert_eq!(
    state.issue_email_code(EMAIL).unwrap_err().status,
    StatusCode::TOO_MANY_REQUESTS
  );
  state.pending().unwrap().emails.get_mut(EMAIL).unwrap().issued_at -= MAIL_RESEND;
  let (code, _) = state.issue_email_code(EMAIL).unwrap();
  let (status, second) = request(
    &app,
    "POST",
    "email/verify",
    json!({"email": EMAIL, "code": code}),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(first["user_id"], second["user_id"]);
  assert_ne!(first["token"], second["token"]);
  assert_eq!(
    request(&app, "GET", "me", Value::Null, first["token"].as_str()).await.0,
    StatusCode::OK
  );
}

#[tokio::test]
async fn email_attempts_and_expiry_block_redeeming_valid_codes() {
  let (_directory, state, app) = fixture();
  let (code, _) = state.issue_email_code(EMAIL).unwrap();
  let wrong = if code == "000000" { "000001" } else { "000000" };
  for _ in 0..MAX_MAIL_ATTEMPTS {
    assert_eq!(
      request(
        &app,
        "POST",
        "email/verify",
        json!({"email": EMAIL, "code": wrong}),
        None
      )
      .await
      .0,
      StatusCode::UNAUTHORIZED
    );
  }
  assert_eq!(
    request(
      &app,
      "POST",
      "email/verify",
      json!({"email": EMAIL, "code": code}),
      None
    )
    .await
    .0,
    StatusCode::UNAUTHORIZED
  );
  let other = "other@example.test";
  let (code, _) = state.issue_email_code(other).unwrap();
  state.pending().unwrap().emails.get_mut(other).unwrap().issued_at -= MAIL_LIFETIME;
  assert_eq!(
    request(
      &app,
      "POST",
      "email/verify",
      json!({"email": other, "code": code}),
      None
    )
    .await
    .0,
    StatusCode::UNAUTHORIZED
  );
  assert!(state.issue_email_code(other).is_ok());
}

#[tokio::test]
async fn device_approval_redeems_one_cli_credential_and_logout_revokes_it() {
  let (_directory, state, app) = fixture();
  let login = signup(&app, &state, EMAIL, "alice").await;
  let browser = login["token"].as_str();
  let (status, challenge) = request(&app, "POST", "device/start", json!({}), None).await;
  assert_eq!(status, StatusCode::OK);
  let device_code = challenge["device_code"].as_str().unwrap();
  let redemption = json!({"device_code": device_code});
  assert_eq!(
    request(&app, "POST", "device/token", redemption.clone(), None).await.0,
    StatusCode::ACCEPTED
  );
  assert_eq!(
    request(&app, "POST", "device/token", redemption.clone(), None).await.0,
    StatusCode::TOO_MANY_REQUESTS
  );
  let approval = json!({"user_code": challenge["user_code"]});
  assert_eq!(
    request(&app, "POST", "device/approve", approval.clone(), None).await.0,
    StatusCode::UNAUTHORIZED
  );
  assert_eq!(
    request(&app, "POST", "device/approve", approval.clone(), browser)
      .await
      .0,
    StatusCode::NO_CONTENT
  );
  assert_eq!(
    request(&app, "POST", "device/approve", approval, browser).await.0,
    StatusCode::NOT_FOUND
  );

  state
    .pending()
    .unwrap()
    .devices
    .get_mut(&hash(device_code))
    .unwrap()
    .next_poll = Instant::now();
  let (status, cli) = request(&app, "POST", "device/token", redemption.clone(), None).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(login["user_id"], cli["user_id"]);
  assert_ne!(login["token"], cli["token"]);
  assert_eq!(
    request(&app, "POST", "device/token", redemption, None).await.0,
    StatusCode::BAD_REQUEST
  );

  let cli = cli["token"].as_str();
  assert_eq!(request(&app, "GET", "me", Value::Null, cli).await.0, StatusCode::OK);
  assert_eq!(
    request(&app, "GET", "keys", Value::Null, cli).await.0,
    StatusCode::FORBIDDEN
  );
  assert_eq!(
    request(&app, "POST", "logout", json!({}), cli).await.0,
    StatusCode::NO_CONTENT
  );
  assert_eq!(
    request(&app, "GET", "me", Value::Null, cli).await.0,
    StatusCode::UNAUTHORIZED
  );
  assert_eq!(request(&app, "GET", "me", Value::Null, browser).await.0, StatusCode::OK);
}

#[tokio::test]
async fn api_keys_are_scoped_revocable_and_visible_only_to_their_owner() {
  let (_directory, state, app) = fixture();
  let alice = signup(&app, &state, EMAIL, "alice").await;
  let bob = signup(&app, &state, "bob@example.test", "bob").await;
  let alice_token = alice["token"].as_str();
  let bob_token = bob["token"].as_str();
  let (status, key) = request(
    &app,
    "POST",
    "keys",
    json!({"name": "inventory", "scopes": [MACHINES_READ]}),
    alice_token,
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let token = key["token"].as_str().unwrap();
  let key_id = key["key_id"].as_str().unwrap();
  let mut headers = HeaderMap::new();
  headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
  assert_eq!(
    state.authenticate(&headers, MACHINES_READ).await.unwrap().username,
    "alice"
  );
  assert_eq!(
    state
      .authenticate(&headers, MACHINES_REGISTER)
      .await
      .unwrap_err()
      .status,
    StatusCode::FORBIDDEN
  );

  let (status, keys) = request(&app, "GET", "keys", Value::Null, alice_token).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(keys["keys"][0]["key_id"], key_id);
  assert!(keys["keys"][0].get("token").is_none());
  assert_eq!(
    request(&app, "GET", "keys", Value::Null, bob_token).await.1,
    json!({"keys":[]})
  );
  let path = format!("keys/{key_id}");
  assert_eq!(
    request(&app, "DELETE", &path, Value::Null, bob_token).await.0,
    StatusCode::NOT_FOUND
  );
  assert_eq!(
    request(&app, "DELETE", &path, Value::Null, alice_token).await.0,
    StatusCode::NO_CONTENT
  );
  assert_eq!(
    state.authenticate(&headers, MACHINES_READ).await.unwrap_err().status,
    StatusCode::UNAUTHORIZED
  );
}

#[tokio::test]
async fn signup_without_mail_and_foreign_browser_requests_fail_closed() {
  let (_directory, state, _) = fixture();
  let disabled = Accounts::with_mailer(state.inner.store.clone(), ORIGIN, None)
    .unwrap()
    .router();
  assert_eq!(
    request(&disabled, "GET", "capabilities", Value::Null, None).await.1,
    json!({"enabled": false})
  );
  assert_eq!(
    request(&disabled, "POST", "email/start", json!({"email": EMAIL}), None)
      .await
      .0,
    StatusCode::SERVICE_UNAVAILABLE
  );
  let response = disabled
    .oneshot(
      Request::builder()
        .uri("/hub/v1/account/capabilities")
        .header(header::ORIGIN, "https://unrelated.example.test")
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
  assert!(mail::mailbox("victim@example.test\r\nBcc: other@example.test").is_none());
  assert_eq!(
    mail::mailbox("user+alias@EXAMPLE.TEST").as_deref(),
    Some("user+alias@example.test")
  );
}
