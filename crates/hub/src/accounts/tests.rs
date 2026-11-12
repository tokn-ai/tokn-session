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

fn origin_header() -> HeaderMap {
  let mut headers = HeaderMap::new();
  headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
  headers
}

async fn request(app: &Router, method: &str, path: &str, body: Value, token: Option<&str>) -> (StatusCode, Value) {
  let mut headers = origin_header();
  if let Some(token) = token {
    headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
  }
  let (status, _, body) = account_request(app, method, path, body, headers).await;
  (status, body)
}

async fn account_request(
  app: &Router,
  method: &str,
  path: &str,
  body: Value,
  headers: HeaderMap,
) -> (StatusCode, HeaderMap, Value) {
  let mut request = Request::builder()
    .method(method)
    .uri(format!("/hub/v1/account/{path}"))
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(body.to_string()))
    .unwrap();
  request.headers_mut().extend(headers);
  let response = app.clone().oneshot(request).await.unwrap();
  let (response, body) = response.into_parts();
  assert_eq!(response.headers[header::CACHE_CONTROL], "no-store");
  let body = body.collect().await.unwrap().to_bytes();
  (
    response.status,
    response.headers,
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

async fn browser_signup(app: &Router, state: &Accounts, email: &str, username: &str) -> (String, Value) {
  let (code, _) = state.issue_email_code(email).unwrap();
  let (status, response, identity) = account_request(
    app,
    "POST",
    "browser/email/verify",
    json!({"email": email, "code": code, "username": username}),
    origin_header(),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  let cookie = response[header::SET_COOKIE]
    .to_str()
    .unwrap()
    .split(';')
    .next()
    .unwrap()
    .to_owned();
  (cookie, identity)
}

#[tokio::test]
async fn browser_session_requires_same_origin_to_issue_and_rehydrates_from_an_http_only_cookie() {
  let (_directory, state, app) = fixture();
  let (code, _) = state.issue_email_code(EMAIL).unwrap();
  let verify = json!({"email": EMAIL, "code": code, "username": "alice"});
  let mut headers = HeaderMap::new();
  for origin in [None, Some("null"), Some("https://sibling.example.test")] {
    if let Some(origin) = origin {
      headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
    }
    let (status, response, _) =
      account_request(&app, "POST", "browser/email/verify", verify.clone(), headers.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!response.contains_key(header::SET_COOKIE));
  }

  headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
  headers.append(header::ORIGIN, HeaderValue::from_static(ORIGIN));
  assert_eq!(
    account_request(&app, "POST", "browser/email/verify", verify.clone(), headers.clone())
      .await
      .0,
    StatusCode::FORBIDDEN
  );
  headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
  let (status, response, identity) = account_request(&app, "POST", "browser/email/verify", verify, headers).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(identity["username"], "alice");
  assert!(Uuid::parse_str(identity["user_id"].as_str().unwrap()).is_ok());
  assert!(identity.get("token").is_none());

  let set_cookie = response[header::SET_COOKIE].to_str().unwrap();
  let (cookie, attributes) = set_cookie.split_once("; ").unwrap();
  let value = cookie.strip_prefix("__Host-tokn-hub-account=").unwrap();
  assert_eq!(attributes, "Path=/; Max-Age=7776000; HttpOnly; SameSite=Strict; Secure");
  let mut headers = HeaderMap::new();
  headers.insert(header::COOKIE, format!("unrelated=1; {cookie}").parse().unwrap());
  let (status, _, restored) = account_request(&app, "GET", "me", Value::Null, headers.clone()).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(restored, identity);
  assert_eq!(
    state.authenticate_machine_reader(&headers).await.unwrap_err().status,
    StatusCode::CONFLICT
  );
  headers.insert(
    ACCOUNT_ID_HEADER,
    identity["user_id"].as_str().unwrap().parse().unwrap(),
  );
  assert_eq!(
    state.authenticate_machine_reader(&headers).await.unwrap().username,
    "alice"
  );
  assert_eq!(
    state
      .authenticate(&headers, MACHINES_REGISTER)
      .await
      .unwrap_err()
      .status,
    StatusCode::UNAUTHORIZED
  );
  assert_eq!(
    request(&app, "GET", "me", Value::Null, Some(value)).await.0,
    StatusCode::UNAUTHORIZED
  );
  headers.append(header::COOKIE, cookie.parse().unwrap());
  assert_eq!(
    account_request(&app, "GET", "me", Value::Null, headers).await.0,
    StatusCode::UNAUTHORIZED
  );
}

#[tokio::test]
async fn browser_cookie_mutations_check_origin_and_logout_revokes_only_browser_sessions() {
  let (_directory, state, app) = fixture();
  let (cookie, identity) = browser_signup(&app, &state, EMAIL, "alice").await;
  let mut headers = HeaderMap::new();
  headers.insert(header::COOKIE, cookie.parse().unwrap());

  let new_key = json!({"name": "inventory", "scopes": [MACHINES_READ]});
  assert_eq!(
    account_request(&app, "POST", "keys", new_key.clone(), headers.clone())
      .await
      .0,
    StatusCode::FORBIDDEN
  );
  headers.insert(header::ORIGIN, HeaderValue::from_static("https://sibling.example.test"));
  assert_eq!(
    account_request(&app, "POST", "logout", Value::Null, headers.clone())
      .await
      .0,
    StatusCode::FORBIDDEN
  );
  headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
  let (status, _, response) = account_request(&app, "POST", "keys", new_key.clone(), headers.clone()).await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(response["account_session_changed"], true);
  let (_, stale_identity) = browser_signup(&app, &state, "stale@example.test", "bob").await;
  headers.insert(
    ACCOUNT_ID_HEADER,
    stale_identity["user_id"].as_str().unwrap().parse().unwrap(),
  );
  let (status, response, body) = account_request(&app, "POST", "logout", Value::Null, headers.clone()).await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(body["account_session_changed"], true);
  assert!(!response.contains_key(header::SET_COOKIE));
  let (status, _, response) = account_request(&app, "GET", "keys", Value::Null, headers.clone()).await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(response["account_session_changed"], true);
  headers.insert(
    ACCOUNT_ID_HEADER,
    identity["user_id"].as_str().unwrap().parse().unwrap(),
  );
  let (status, _, key) = account_request(&app, "POST", "keys", new_key, headers.clone()).await;
  assert_eq!(status, StatusCode::CREATED);
  let (status, _, keys) = account_request(&app, "GET", "keys", Value::Null, headers.clone()).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(keys["keys"][0]["key_id"], key["key_id"]);

  let api_key = key["token"].as_str().unwrap();
  let mut impostor = headers.clone();
  impostor.insert(header::COOKIE, format!("{BROWSER_COOKIE}={api_key}").parse().unwrap());
  assert_eq!(
    account_request(&app, "GET", "me", Value::Null, impostor.clone())
      .await
      .0,
    StatusCode::UNAUTHORIZED
  );
  assert_eq!(
    account_request(&app, "POST", "logout", Value::Null, impostor).await.0,
    StatusCode::UNAUTHORIZED
  );
  assert_eq!(
    request(&app, "GET", "me", Value::Null, Some(api_key)).await.0,
    StatusCode::OK
  );
  let mut explicit_bearer = headers.clone();
  explicit_bearer.insert(header::AUTHORIZATION, format!("Bearer {api_key}").parse().unwrap());
  assert_eq!(
    account_request(&app, "GET", "keys", Value::Null, explicit_bearer.clone())
      .await
      .0,
    StatusCode::FORBIDDEN
  );
  explicit_bearer.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer malformed"));
  assert_eq!(
    account_request(&app, "GET", "me", Value::Null, explicit_bearer).await.0,
    StatusCode::UNAUTHORIZED
  );

  let (_, challenge) = request(&app, "POST", "device/start", json!({}), None).await;
  assert_eq!(
    account_request(
      &app,
      "POST",
      "device/approve",
      json!({"user_code": challenge["user_code"]}),
      headers.clone(),
    )
    .await
    .0,
    StatusCode::NO_CONTENT
  );
  let path = format!("keys/{}", key["key_id"].as_str().unwrap());
  assert_eq!(
    account_request(&app, "DELETE", &path, Value::Null, headers.clone())
      .await
      .0,
    StatusCode::NO_CONTENT
  );

  let (status, response, _) = account_request(&app, "POST", "logout", Value::Null, headers.clone()).await;
  assert_eq!(status, StatusCode::NO_CONTENT);
  assert_eq!(
    response[header::SET_COOKIE],
    "__Host-tokn-hub-account=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict; Secure"
  );
  assert_eq!(
    account_request(&app, "POST", "browser/refresh", json!({}), headers)
      .await
      .0,
    StatusCode::UNAUTHORIZED
  );
}

#[tokio::test]
async fn browser_refresh_extends_only_due_unexpired_sessions() {
  let (_directory, state, app) = fixture();
  let (cookie, identity) = browser_signup(&app, &state, EMAIL, "alice").await;
  let mut headers = HeaderMap::new();
  headers.insert(header::COOKIE, cookie.parse().unwrap());
  assert_eq!(
    account_request(&app, "POST", "browser/refresh", json!({}), headers.clone())
      .await
      .0,
    StatusCode::FORBIDDEN
  );
  headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
  let (status, response, body) = account_request(&app, "POST", "browser/refresh", json!({}), headers.clone()).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body, identity);
  assert!(!response.contains_key(header::SET_COOKIE));

  let token = hash(cookie.split_once('=').unwrap().1);
  let store = &state.inner.store;
  let now = crate::secure_client::unix_time().unwrap() as i64;
  let set_expiry = |expires_at| {
    store
      .connection()
      .unwrap()
      .execute(
        "UPDATE hub_account_credentials SET expires_at = ?1 WHERE token_hash = ?2",
        rusqlite::params![expires_at, token.as_slice()],
      )
      .unwrap();
  };
  set_expiry(now + 120);
  let (status, response, body) = account_request(&app, "POST", "browser/refresh", json!({}), headers.clone()).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body, identity);
  assert_eq!(
    response[header::SET_COOKIE],
    format!("{cookie}; Path=/; Max-Age=7776000; HttpOnly; SameSite=Strict; Secure")
  );
  let expires_at: i64 = store
    .connection()
    .unwrap()
    .query_row(
      "SELECT expires_at FROM hub_account_credentials WHERE token_hash = ?1",
      [token.as_slice()],
      |row| row.get(0),
    )
    .unwrap();
  assert!(expires_at >= now + storage::BROWSER_COOKIE_TTL);

  set_expiry(now - 1);
  let (status, response, _) = account_request(&app, "POST", "browser/refresh", json!({}), headers).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
  assert!(!response.contains_key(header::SET_COOKIE));
}

#[test]
fn development_cookie_is_separate_and_scoped_to_account_routes() {
  let directory = TempDir::new().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let state = Accounts::with_mailer(store, "http://localhost:1234", None).unwrap();
  assert_eq!(
    state.browser_cookie("value", 60).unwrap(),
    "tokn_hub_account_dev=value; Path=/hub/v1/account; Max-Age=60; HttpOnly; SameSite=Strict"
  );
}

#[test]
fn new_cookie_session_evicts_oldest_cookie_without_affecting_bearer() {
  let (_directory, state, _) = fixture();
  let store = &state.inner.store;
  let bearer = storage::email_login(store, EMAIL, Some("alice"), TokenTransport::Bearer).unwrap();
  let oldest = storage::email_login(store, EMAIL, None, TokenTransport::Cookie).unwrap();
  let now = crate::secure_client::unix_time().unwrap() as i64;
  store
    .connection()
    .unwrap()
    .execute(
      "UPDATE hub_account_credentials SET expires_at = ?1 WHERE token_hash = ?2",
      rusqlite::params![now + 120, hash(&oldest.token).as_slice()],
    )
    .unwrap();
  let last = (0..32)
    .map(|_| storage::email_login(store, EMAIL, None, TokenTransport::Cookie).unwrap())
    .last()
    .unwrap();
  let credential = |login: &Login, from_cookie| {
    storage::credential(
      store,
      &AccountToken {
        hash: hash(&login.token),
        from_cookie,
        expected_user_id: None,
      },
    )
  };
  assert_eq!(
    credential(&oldest, true).err().unwrap().status,
    StatusCode::UNAUTHORIZED
  );
  assert!(credential(&last, true).is_ok());
  assert!(credential(&bearer, false).is_ok());
}

#[tokio::test]
async fn email_code_creates_one_identity_and_can_only_be_used_once() {
  let (_directory, state, app) = fixture();
  state
    .inner
    .store
    .connection()
    .unwrap()
    .execute_batch(
      "CREATE TABLE namespaces (username TEXT PRIMARY KEY, owner_id TEXT NOT NULL);
       INSERT INTO namespaces VALUES ('legacy', 'legacy-owner');
       INSERT INTO hub_accounts VALUES ('existing-account', 'reserved', 'reserved@example.invalid');",
    )
    .unwrap();
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
    json!({"email": EMAIL, "code": code, "username": "legacy"}),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(first["username"], "legacy");
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
  let (_directory, state, app) = fixture();
  let account = signup(&app, &state, EMAIL, "alice").await;
  let disabled = Accounts::with_mailer(state.inner.store.clone(), ORIGIN, None)
    .unwrap()
    .router();
  assert_eq!(
    request(&disabled, "GET", "capabilities", Value::Null, None).await.1,
    json!({"enabled": true, "email_configured": false})
  );
  assert_eq!(
    request(&disabled, "POST", "email/start", json!({"email": EMAIL}), None)
      .await
      .0,
    StatusCode::SERVICE_UNAVAILABLE
  );
  assert_eq!(
    request(&disabled, "GET", "me", Value::Null, account["token"].as_str())
      .await
      .0,
    StatusCode::OK
  );
  let (status, device) = request(&disabled, "POST", "device/start", json!({}), None).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(
    request(
      &disabled,
      "POST",
      "device/approve",
      json!({"user_code": device["user_code"]}),
      account["token"].as_str()
    )
    .await
    .0,
    StatusCode::NO_CONTENT
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

#[tokio::test]
async fn existing_account_claims_new_machine_and_clients_negotiate_canonical_discovery_addresses() {
  use crate::{
    account_host,
    host_setup::{HostOptions, prepare_host},
    server::{self, HubState},
  };
  use tokn_hub_client_core::discovery::DISCOVERY_ADDRESSES_HEADER;

  let (directory, accounts, account_app) = fixture();
  let login = signup(&account_app, &accounts, EMAIL, "alice").await;
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let hub_url = format!("http://localhost:{}", listener.local_addr().unwrap().port());
  let state = HubState::new(accounts.inner.store.clone(), &hub_url).unwrap();
  let app = server::router(state.clone());
  let api = app.clone();
  let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
  let host_dir = directory.path().join("host");
  let host = prepare_host(HostOptions {
    hub: Some(Url::parse(&hub_url).unwrap()),
    name: Some("Workstation".into()),
    viewer_url: None,
    passkey_origin: None,
    state_dir: host_dir.clone(),
    totp_secret_file: None,
    viewer_token: None,
    allow_control: Some(false),
    insecure_loopback: true,
    ice_servers: None,
  })
  .unwrap();
  let host_id = &host.config.paired.as_ref().unwrap().host_id;
  assert!(state.store.host(host_id).unwrap().is_none());
  assert_eq!(
    account_host::claim(&host.config, "workstation", login["token"].as_str().unwrap())
      .await
      .unwrap(),
    "alice:workstation"
  );
  let stored = state.store.host(host_id).unwrap().unwrap();
  assert_eq!(state.store.register_tunnel_host(stored.clone(), None).unwrap(), stored);
  let group = account_host::saved_discovery_group(&host_dir, &hub_url, host_id)
    .unwrap()
    .unwrap();
  assert_eq!(group.namespace, "alice");
  let path = format!("/hub/v1/discovery/{}/{}", group.namespace, group.secret_digest);
  for negotiated in [false, true] {
    let mut request = Request::builder().uri(&path);
    if negotiated {
      request = request.header(DISCOVERY_ADDRESSES_HEADER, "1");
    }
    let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let result: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result["machines"][0]["host_id"], *host_id);
    assert_eq!(
      result["machines"][0].get("machine_address").and_then(Value::as_str),
      negotiated.then_some("alice:workstation")
    );
  }
  server.abort();
}
