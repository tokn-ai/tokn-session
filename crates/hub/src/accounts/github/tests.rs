use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tokn_hub_client_core::account::{DEVICE_TOKEN, VIEWER_START};
use tower::ServiceExt;
use url::Url;

use super::super::{ACCOUNT_ID_HEADER, BROWSER_COOKIE, MACHINES_REGISTER, Sendmail, Store, TokenTransport};
use super::*;

const ORIGIN: &str = "https://hub.example.test";
const BASE: &str = "/hub/v1/account/browser/github";
const START: &str = "/hub/v1/account/browser/github/start";
const COMPLETE: &str = "/hub/v1/account/browser/github/complete";
const SIGNUP: &str = "/hub/v1/account/browser/github/signup";
const EMAIL: &str = "person@example.test";

fn fixture() -> (TempDir, Accounts, Router) {
  let directory = TempDir::new().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let mut provider = Provider::new(ORIGIN, "example-client".into(), Zeroizing::new("test-secret".into())).unwrap();
  provider.exchange_for_test = Some(|code| match code {
    "existing" => Ok(Identity {
      id: 123,
      login: "Octocat".into(),
      verified_email: Some(EMAIL.into()),
    }),
    "new" => Ok(Identity {
      id: 321,
      login: "Another-Hub".into(),
      verified_email: Some("new@example.test".into()),
    }),
    _ => Err(()),
  });
  let accounts = Accounts::with_providers(
    store,
    ORIGIN,
    Some(Sendmail::test_delivery()),
    Some(GitHub::new(provider)),
  )
  .unwrap();
  let router = accounts.router();
  (directory, accounts, router)
}

fn headers(cookie: Option<&str>, account: Option<&Value>) -> HeaderMap {
  let mut headers = HeaderMap::new();
  headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
  if let Some(cookie) = cookie {
    headers.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
  }
  if let Some(account) = account {
    headers.insert(ACCOUNT_ID_HEADER, account["user_id"].as_str().unwrap().parse().unwrap());
  }
  headers
}

async fn request(
  app: &Router,
  method: &str,
  path: &str,
  body: Value,
  headers: HeaderMap,
) -> (StatusCode, HeaderMap, Value) {
  let mut request = Request::builder()
    .method(method)
    .uri(path)
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(body.to_string()))
    .unwrap();
  request.headers_mut().extend(headers);
  let (parts, body) = app.clone().oneshot(request).await.unwrap().into_parts();
  assert_eq!(parts.headers[header::CACHE_CONTROL], "no-store");
  let body = body.collect().await.unwrap().to_bytes();
  (
    parts.status,
    parts.headers,
    serde_json::from_slice(&body).unwrap_or(Value::Null),
  )
}

fn cookie(headers: &HeaderMap, name: &str) -> String {
  headers
    .get_all(header::SET_COOKIE)
    .iter()
    .map(|value| value.to_str().unwrap())
    .find(|value| value.starts_with(&format!("{name}=")))
    .unwrap()
    .split(';')
    .next()
    .unwrap()
    .to_owned()
}

async fn email_session(app: &Router, state: &Accounts) -> (String, Value) {
  let (code, _) = state.issue_email_code(EMAIL).unwrap();
  let (status, response, identity) = request(
    app,
    "POST",
    "/hub/v1/account/browser/email/verify",
    json!({"email": EMAIL, "code": code, "username": "alice"}),
    headers(None, None),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  (cookie(&response, BROWSER_COOKIE), identity)
}

async fn start_flow(app: &Router, data: Value, headers: HeaderMap) -> (String, Url) {
  let (status, headers, body) = request(app, "POST", START, data, headers).await;
  assert_eq!(status, StatusCode::OK);
  let url = body["authorization_url"].as_str().unwrap().parse::<Url>().unwrap();
  (cookie(&headers, FLOW_COOKIE), url)
}

fn callback_path(url: &Url, code: &str) -> String {
  let state = url
    .query_pairs()
    .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
    .unwrap();
  format!("{CALLBACK}?state={state}&code={code}")
}

async fn exchange(app: &Router, url: &Url, code: &str, cookie: &str) -> (StatusCode, HeaderMap) {
  let mut headers = headers(Some(cookie), None);
  headers.remove(header::ORIGIN);
  let (status, headers, _) = request(app, "GET", &callback_path(url, code), Value::Null, headers).await;
  assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
  (status, headers)
}

#[tokio::test]
async fn github_login_binds_browser_state_uses_pkce_and_preserves_explicit_viewer_approval() {
  let (_directory, accounts, app) = fixture();
  let (status, _, viewer) = request(&app, "POST", VIEWER_START, json!({}), headers(None, None)).await;
  assert_eq!(status, StatusCode::OK);
  let user_code = viewer["user_code"].as_str().unwrap();
  let device_code = viewer["device_code"].as_str().unwrap();
  let (bind, url) = start_flow(
    &app,
    json!({"mode":"sign_in", "user_code": user_code, "device":"viewer"}),
    headers(None, None),
  )
  .await;
  assert_eq!(url.origin().ascii_serialization(), "https://github.com");
  assert_eq!(url.path(), "/login/oauth/authorize");
  let query = url.query_pairs().collect::<HashMap<_, _>>();
  assert_eq!(query["client_id"], "example-client");
  assert_eq!(query["redirect_uri"], format!("{ORIGIN}{CALLBACK}"));
  assert_eq!(query["scope"], "user:email");
  assert_eq!(query["code_challenge_method"], "S256");
  assert_eq!(query["prompt"], "select_account");
  let key = flow_key(&headers(Some(&bind), None)).unwrap();
  match &accounts.inner.github.as_ref().unwrap().pending().unwrap().flows[&key].phase {
    Phase::Authorizing { state_hash, verifier } => {
      assert_eq!(query["code_challenge"], code_challenge(verifier));
      assert!(bool::from(state_hash.ct_eq(&hash(query["state"].as_ref()))));
      assert_ne!(query["state"], **verifier);
      assert!(!url.as_str().contains(user_code));
      assert!(!url.as_str().contains(device_code));
    }
    _ => panic!("authorization disappeared"),
  }
  let (status, headers_after) = exchange(&app, &url, "new", "__Host-tokn-hub-github=bogus").await;
  assert_eq!(status, StatusCode::SEE_OTHER);
  assert_eq!(headers_after[header::LOCATION], "/account?github=failed");
  assert_eq!(
    request(
      &app,
      "HEAD",
      &callback_path(&url, "new"),
      Value::Null,
      headers(Some(&bind), None)
    )
    .await
    .0,
    StatusCode::METHOD_NOT_ALLOWED,
  );
  let (status, headers_after) = exchange(&app, &url, "new", &bind).await;
  assert_eq!(status, StatusCode::SEE_OTHER);
  assert_eq!(
    headers_after[header::LOCATION],
    format!("/account?github=complete&user_code={user_code}&device=viewer")
  );
  assert!(!headers_after.contains_key(header::SET_COOKIE));
  assert_eq!(
    exchange(&app, &url, "new", &bind).await.1[header::LOCATION],
    "/account?github=failed"
  );

  let mut foreign = headers(Some(&bind), None);
  foreign.insert(
    header::ORIGIN,
    HeaderValue::from_static("https://elsewhere.example.test"),
  );
  assert_eq!(
    request(&app, "POST", COMPLETE, json!({}), foreign).await.0,
    StatusCode::FORBIDDEN
  );
  let (status, _, result) = request(&app, "POST", COMPLETE, json!({}), headers(Some(&bind), None)).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(
    result,
    json!({"status":"signup_required", "suggested_username":"another-hub"})
  );
  let (status, _, _) = request(
    &app,
    "POST",
    SIGNUP,
    json!({"username":"invalid slug"}),
    headers(Some(&bind), None),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  let (status, response, identity) = request(
    &app,
    "POST",
    SIGNUP,
    json!({"username":"example"}),
    headers(Some(&bind), None),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  let cookie = cookie(&response, BROWSER_COOKIE);
  assert_eq!(
    request(
      &app,
      "POST",
      SIGNUP,
      json!({"username":"retry"}),
      headers(Some(&bind), None)
    )
    .await
    .0,
    StatusCode::BAD_REQUEST
  );

  let poll = json!({"device_code":device_code});
  assert_eq!(
    request(&app, "POST", DEVICE_TOKEN, poll.clone(), headers(None, None))
      .await
      .0,
    StatusCode::ACCEPTED
  );
  let approval = json!({"user_code": user_code});
  let browser = headers(Some(&cookie), Some(&identity));
  assert_eq!(
    request(
      &app,
      "POST",
      "/hub/v1/account/device/approve",
      approval.clone(),
      browser.clone()
    )
    .await
    .0,
    StatusCode::NOT_FOUND
  );
  assert_eq!(
    request(&app, "POST", "/hub/v1/account/device/viewer/approve", approval, browser)
      .await
      .0,
    StatusCode::NO_CONTENT
  );
  accounts
    .pending()
    .unwrap()
    .devices
    .get_mut(&hash(device_code))
    .unwrap()
    .next_poll = Instant::now();
  let (status, _, viewer_login) = request(&app, "POST", DEVICE_TOKEN, poll, headers(None, None)).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(viewer_login["user_id"], identity["user_id"]);
  let mut bearer = headers(None, None);
  bearer.insert(
    header::AUTHORIZATION,
    format!("Bearer {}", viewer_login["token"].as_str().unwrap())
      .parse()
      .unwrap(),
  );
  assert_eq!(
    accounts.authenticate_machine_reader(&bearer).await.unwrap().username,
    "example"
  );
  assert_eq!(
    accounts
      .authenticate(&bearer, MACHINES_REGISTER)
      .await
      .unwrap_err()
      .status,
    StatusCode::FORBIDDEN
  );
}

#[tokio::test]
async fn matching_email_does_not_link_and_linking_requires_its_original_browser_session() {
  let (_directory, accounts, app) = fixture();
  let (email_cookie, identity) = email_session(&app, &accounts).await;
  let (bind, url) = start_flow(&app, json!({"mode":"sign_in"}), headers(None, None)).await;
  assert_eq!(exchange(&app, &url, "existing", &bind).await.0, StatusCode::SEE_OTHER);
  let (status, response, outcome) = request(&app, "POST", COMPLETE, json!({}), headers(Some(&bind), None)).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(outcome, json!({"status":"link_required"}));
  assert!(
    response
      .get_all(header::SET_COOKIE)
      .iter()
      .all(|value| !value.to_str().unwrap().starts_with(BROWSER_COOKIE))
  );
  let (status, _, outcome) = request(
    &app,
    "GET",
    BASE,
    Value::Null,
    headers(Some(&email_cookie), Some(&identity)),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(outcome, json!({"connection": null}));

  let good = headers(Some(&email_cookie), Some(&identity));
  let (bind, url) = start_flow(&app, json!({"mode":"link"}), good.clone()).await;
  exchange(&app, &url, "existing", &bind).await;
  let other_login = storage::email_login(&accounts.inner.store, EMAIL, None, TokenTransport::Cookie).unwrap();
  let other_cookie = format!("{BROWSER_COOKIE}={}", other_login.token);
  let wrong_session = format!("{bind}; {other_cookie}");
  assert_eq!(
    request(
      &app,
      "POST",
      COMPLETE,
      json!({}),
      headers(Some(&wrong_session), Some(&identity))
    )
    .await
    .0,
    StatusCode::CONFLICT
  );

  let (bind, url) = start_flow(&app, json!({"mode":"link"}), good.clone()).await;
  exchange(&app, &url, "existing", &bind).await;
  let same_session = format!("{bind}; {email_cookie}");
  let (status, _, outcome) = request(
    &app,
    "POST",
    COMPLETE,
    json!({}),
    headers(Some(&same_session), Some(&identity)),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(outcome, json!({"status":"linked"}));
  assert_eq!(
    request(&app, "GET", BASE, Value::Null, good).await.2,
    json!({"connection": {"github_login":"Octocat"}})
  );

  let (bind, url) = start_flow(&app, json!({"mode":"sign_in"}), headers(None, None)).await;
  exchange(&app, &url, "existing", &bind).await;
  let (status, response, outcome) = request(&app, "POST", COMPLETE, json!({}), headers(Some(&bind), None)).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(outcome["status"], "signed_in");
  assert_eq!(outcome["user_id"], identity["user_id"]);
  assert_eq!(outcome["username"], "alice");
  assert!(cookie(&response, BROWSER_COOKIE).starts_with(&format!("{BROWSER_COOKIE}=")));
}
