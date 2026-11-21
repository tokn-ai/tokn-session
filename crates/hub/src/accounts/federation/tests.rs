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
const ISSUER: &str = "https://account.example.test/oidc";
const BASE: &str = "/hub/v1/account/browser/github";
const COMPLETE: &str = "/hub/v1/account/browser/github/complete";
const APELL_BASE: &str = "/hub/v1/account/browser/apell";
const APELL_COMPLETE: &str = "/hub/v1/account/browser/apell/complete";
const APELL_CONFIRM_LINK: &str = "/hub/v1/account/browser/apell/confirm-link";
const EMAIL: &str = "person@example.test";

fn fixture() -> (TempDir, Accounts, Router) {
  let directory = TempDir::new().unwrap();
  let store = Store::open(directory.path().join("hub.sqlite")).unwrap();
  let mut provider = Provider::new(ORIGIN, "example-client".into(), Zeroizing::new("test-secret".into())).unwrap();
  provider.exchange_for_test = Some(|code| match code {
    "existing" => Ok(github::Identity {
      id: 123,
      login: "Octocat".into(),
      verified_email: Some(EMAIL.into()),
    }),
    "new" => Ok(github::Identity {
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
    Federation {
      github: Some(provider),
      apell: Some(Oidc::for_test(ISSUER, |code, nonce| {
        assert!(valid_secret(nonce));
        match code {
          "existing" => Ok(apell::Identity {
            issuer: ISSUER.into(),
            subject: "apell-123".into(),
            login: Some("Octocat".into()),
            verified_email: Some(EMAIL.into()),
          }),
          "new" => Ok(apell::Identity {
            issuer: ISSUER.into(),
            subject: "apell-321".into(),
            login: Some("Another-Hub".into()),
            verified_email: Some("oidc-new@example.test".into()),
          }),
          _ => Err(()),
        }
      })),
      ..Default::default()
    },
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
  start_provider_flow(app, Kind::GitHub, data, headers).await
}

async fn start_provider_flow(app: &Router, kind: Kind, data: Value, headers: HeaderMap) -> (String, Url) {
  let (status, headers, body) = request(
    app,
    "POST",
    &format!("/hub/v1/account/browser/{}/start", kind.label()),
    data,
    headers,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  let url = body["authorization_url"].as_str().unwrap().parse::<Url>().unwrap();
  (cookie(&headers, kind.cookie()), url)
}

fn provider_callback_path(kind: Kind, url: &Url, code: &str, issuer: Option<&str>) -> String {
  let state = url
    .query_pairs()
    .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
    .unwrap();
  let mut pairs = form_urlencoded::Serializer::new(String::new());
  pairs.append_pair("state", &state).append_pair("code", code);
  if let Some(issuer) = issuer {
    pairs.append_pair("iss", issuer);
  }
  format!("{}?{}", kind.callback(), pairs.finish())
}

async fn exchange(app: &Router, url: &Url, code: &str, cookie: &str) -> (StatusCode, HeaderMap) {
  provider_exchange(app, Kind::GitHub, url, code, cookie, None).await
}

async fn provider_exchange(
  app: &Router,
  kind: Kind,
  url: &Url,
  code: &str,
  cookie: &str,
  issuer: Option<&str>,
) -> (StatusCode, HeaderMap) {
  let mut headers = headers(Some(cookie), None);
  headers.remove(header::ORIGIN);
  let (status, headers, _) = request(
    app,
    "GET",
    &provider_callback_path(kind, url, code, issuer),
    Value::Null,
    headers,
  )
  .await;
  assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
  (status, headers)
}

#[tokio::test]
async fn provider_signup_binds_browser_state_uses_pkce_and_preserves_explicit_viewer_approval() {
  for kind in [Kind::GitHub, Kind::Apell] {
    exercise_provider_signup(kind).await;
  }
}

async fn exercise_provider_signup(kind: Kind) {
  let (_directory, accounts, app) = fixture();
  let (status, _, viewer) = request(&app, "POST", VIEWER_START, json!({}), headers(None, None)).await;
  assert_eq!(status, StatusCode::OK);
  let user_code = viewer["user_code"].as_str().unwrap();
  let device_code = viewer["device_code"].as_str().unwrap();
  let (bind, url) = start_provider_flow(
    &app,
    kind,
    json!({"mode":"sign_in", "user_code": user_code, "device":"viewer"}),
    headers(None, None),
  )
  .await;
  let query = url.query_pairs().collect::<HashMap<_, _>>();
  assert_eq!(query["redirect_uri"], format!("{ORIGIN}{}", kind.callback()));
  match kind {
    Kind::GitHub => {
      assert_eq!(url.origin().ascii_serialization(), "https://github.com");
      assert_eq!(url.path(), "/login/oauth/authorize");
      assert_eq!(query["client_id"], "example-client");
      assert_eq!(query["scope"], "user:email");
      assert_eq!(query["prompt"], "select_account");
    }
    Kind::Apell => {
      assert_eq!(url.origin().ascii_serialization(), "https://account.example.test");
      assert_eq!(url.path(), "/oidc/auth");
      assert_eq!(query["scope"], "openid profile email");
      assert_ne!(query["nonce"], query["state"]);
    }
  }
  assert_eq!(query["code_challenge_method"], "S256");
  let key = flow_key(kind, &headers(Some(&bind), None)).unwrap();
  match &accounts.inner.federation.pending().unwrap().flows[&key].phase {
    Phase::Authorizing {
      state_hash, verifier, ..
    } => {
      assert_eq!(query["code_challenge"], code_challenge(verifier));
      assert!(bool::from(state_hash.ct_eq(&hash(query["state"].as_ref()))));
      assert_ne!(query["state"], **verifier);
      assert!(!url.as_str().contains(user_code));
      assert!(!url.as_str().contains(device_code));
    }
    _ => panic!("authorization disappeared"),
  }
  let fail_redirect = format!("/account?{}=failed", kind.label());
  let bogus_cookie = format!("{}=bogus", kind.cookie());
  let issuer = (kind == Kind::Apell).then_some(ISSUER);
  let (status, headers_after) = provider_exchange(&app, kind, &url, "new", &bogus_cookie, issuer).await;
  assert_eq!(status, StatusCode::SEE_OTHER);
  assert_eq!(headers_after[header::LOCATION], fail_redirect);
  assert_eq!(
    request(
      &app,
      "HEAD",
      &provider_callback_path(kind, &url, "new", issuer),
      Value::Null,
      headers(Some(&bind), None)
    )
    .await
    .0,
    StatusCode::METHOD_NOT_ALLOWED,
  );
  let (status, headers_after) = provider_exchange(&app, kind, &url, "new", &bind, issuer).await;
  assert_eq!(status, StatusCode::SEE_OTHER);
  assert_eq!(
    headers_after[header::LOCATION],
    format!("/account?{}=complete&user_code={user_code}&device=viewer", kind.label())
  );
  assert!(!headers_after.contains_key(header::SET_COOKIE));
  assert_eq!(
    provider_exchange(&app, kind, &url, "new", &bind, issuer).await.1[header::LOCATION],
    fail_redirect,
  );

  let mut foreign = headers(Some(&bind), None);
  foreign.insert(
    header::ORIGIN,
    HeaderValue::from_static("https://elsewhere.example.test"),
  );
  let complete = format!("/hub/v1/account/browser/{}/complete", kind.label());
  let signup = format!("/hub/v1/account/browser/{}/signup", kind.label());
  assert_eq!(
    request(&app, "POST", &complete, json!({}), foreign).await.0,
    StatusCode::FORBIDDEN
  );
  let (status, _, result) = request(&app, "POST", &complete, json!({}), headers(Some(&bind), None)).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(
    result,
    json!({"status":"signup_required", "suggested_username":"another-hub"})
  );
  let (status, _, _) = request(
    &app,
    "POST",
    &signup,
    json!({"username":"invalid slug"}),
    headers(Some(&bind), None),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  let (status, response, identity) = request(
    &app,
    "POST",
    &signup,
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
      &signup,
      json!({"username":"retry"}),
      headers(Some(&bind), None)
    )
    .await
    .0,
    StatusCode::BAD_REQUEST
  );

  check_viewer_approval(&accounts, &app, user_code, device_code, &cookie, &identity).await;
}

async fn check_viewer_approval(
  accounts: &Accounts,
  app: &Router,
  user_code: &str,
  device_code: &str,
  cookie: &str,
  identity: &Value,
) {
  let poll = json!({"device_code":device_code});
  assert_eq!(
    request(app, "POST", DEVICE_TOKEN, poll.clone(), headers(None, None))
      .await
      .0,
    StatusCode::ACCEPTED
  );
  let approval = json!({"user_code": user_code});
  let browser = headers(Some(cookie), Some(identity));
  assert_eq!(
    request(
      app,
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
    request(app, "POST", "/hub/v1/account/device/viewer/approve", approval, browser)
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
  let (status, _, viewer_login) = request(app, "POST", DEVICE_TOKEN, poll, headers(None, None)).await;
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
    identity["username"].as_str().unwrap()
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

#[tokio::test]
async fn apell_issuer_and_provider_are_bound_and_viewer_approval_stays_explicit() {
  let kind = Kind::Apell;
  let (_directory, accounts, app) = fixture();
  let (_, _, caps) = request(
    &app,
    "GET",
    "/hub/v1/account/capabilities",
    Value::Null,
    headers(None, None),
  )
  .await;
  assert_eq!(caps["apell_issuer"], ISSUER);
  assert_eq!(caps["github_configured"], true);
  let (email_cookie, identity) = email_session(&app, &accounts).await;
  let owner = headers(Some(&email_cookie), Some(&identity));
  let (bind, url) = start_provider_flow(&app, kind, json!({"mode":"sign_in"}), headers(None, None)).await;
  let query = url.query_pairs().collect::<HashMap<_, _>>();
  let nonce = &query["nonce"];
  assert_ne!(nonce, &query["state"]);
  let key = flow_key(kind, &headers(Some(&bind), None)).unwrap();
  assert!(
    matches!(&accounts.inner.federation.pending().unwrap().flows[&key].phase,
    Phase::Authorizing { nonce: saved, .. } if nonce == saved.as_str())
  );

  // Even a same-browser request carrying an Apell binder as a GitHub cookie
  // cannot send an Apell authorization code into the GitHub flow.
  let wrong_cookie = format!("{}={}", Kind::GitHub.cookie(), bind.split_once('=').unwrap().1);
  let (_, wrong_provider) = exchange(&app, &url, "existing", &wrong_cookie).await;
  assert_eq!(wrong_provider[header::LOCATION], "/account?github=failed");
  let (_, wrong_issuer) = provider_exchange(
    &app,
    kind,
    &url,
    "existing",
    &bind,
    Some("https://other.example.test/oidc"),
  )
  .await;
  assert_eq!(wrong_issuer[header::LOCATION], "/account?apell=failed");
  let (_, returned) = provider_exchange(&app, kind, &url, "existing", &bind, Some(ISSUER)).await;
  assert_eq!(returned[header::LOCATION], "/account?apell=complete");
  assert_eq!(
    request(&app, "POST", COMPLETE, json!({}), headers(Some(&wrong_cookie), None))
      .await
      .0,
    StatusCode::BAD_REQUEST
  );
  apell_link_prompt(&app, &bind).await;
  assert_eq!(
    request(&app, "GET", APELL_BASE, Value::Null, owner.clone()).await.2,
    json!({"connection":null})
  );

  let (bind, url) = start_provider_flow(&app, kind, json!({"mode":"link"}), owner.clone()).await;
  let (_, returned) = provider_exchange(&app, kind, &url, "existing", &bind, Some(ISSUER)).await;
  assert_eq!(returned[header::LOCATION], "/account?apell=complete");
  let same_session = format!("{bind}; {email_cookie}");
  assert_eq!(
    request(
      &app,
      "POST",
      APELL_COMPLETE,
      json!({}),
      headers(Some(&same_session), Some(&identity))
    )
    .await
    .2,
    json!({"status":"linked"})
  );
  assert_eq!(
    request(&app, "GET", APELL_BASE, Value::Null, owner.clone()).await.2,
    json!({"connection":{"apell_login":"Octocat"}})
  );
  assert_eq!(
    request(&app, "GET", BASE, Value::Null, owner).await.2,
    json!({"connection":null})
  );

  let (status, _, viewer) = request(&app, "POST", VIEWER_START, json!({}), headers(None, None)).await;
  assert_eq!(status, StatusCode::OK);
  let user_code = viewer["user_code"].as_str().unwrap();
  let device_code = viewer["device_code"].as_str().unwrap();
  let (bind, url) = start_provider_flow(
    &app,
    kind,
    json!({"mode":"sign_in", "user_code":user_code, "device":"viewer"}),
    headers(None, None),
  )
  .await;
  assert!(!url.as_str().contains(user_code));
  assert!(!url.as_str().contains(device_code));
  let (_, returned) = provider_exchange(&app, kind, &url, "existing", &bind, Some(ISSUER)).await;
  assert_eq!(
    returned[header::LOCATION],
    format!("/account?apell=complete&user_code={user_code}&device=viewer")
  );
  let (_, response, signed_in) = request(&app, "POST", APELL_COMPLETE, json!({}), headers(Some(&bind), None)).await;
  assert_eq!(
    signed_in,
    json!({"status":"signed_in", "user_id":identity["user_id"], "username":"alice"})
  );
  let signed_in_cookie = cookie(&response, BROWSER_COOKIE);
  check_viewer_approval(&accounts, &app, user_code, device_code, &signed_in_cookie, &identity).await;
}

async fn apell_link_prompt(app: &Router, bind: &str) -> (HeaderMap, Value) {
  let (status, response, prompt) = request(app, "POST", APELL_COMPLETE, json!({}), headers(Some(bind), None)).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(prompt["status"], "link_required");
  assert_eq!(prompt["username"], "alice");
  assert!(valid_secret(prompt["confirmation"].as_str().unwrap()));
  (response, prompt)
}

#[tokio::test]
async fn apell_verified_email_match_requires_consent_before_sign_in_and_viewer_approval() {
  let (_directory, accounts, app) = fixture();
  let (email_cookie, identity) = email_session(&app, &accounts).await;
  let owner = headers(Some(&email_cookie), Some(&identity));
  let (status, _, viewer) = request(&app, "POST", VIEWER_START, json!({}), headers(None, None)).await;
  assert_eq!(status, StatusCode::OK);
  let user_code = viewer["user_code"].as_str().unwrap();
  let device_code = viewer["device_code"].as_str().unwrap();
  let (bind, url) = start_provider_flow(
    &app,
    Kind::Apell,
    json!({"mode":"sign_in", "user_code":user_code, "device":"viewer"}),
    headers(None, None),
  )
  .await;
  let (_, returned) = provider_exchange(&app, Kind::Apell, &url, "existing", &bind, Some(ISSUER)).await;
  assert_eq!(
    returned[header::LOCATION],
    format!("/account?apell=complete&user_code={user_code}&device=viewer")
  );
  let (response, prompt) = apell_link_prompt(&app, &bind).await;
  assert!(!response.contains_key(header::SET_COOKIE));
  assert_eq!(apell_link_prompt(&app, &bind).await.1, prompt);
  assert_eq!(
    request(&app, "GET", APELL_BASE, Value::Null, owner.clone()).await.2,
    json!({"connection":null})
  );

  let consent = json!({"confirmation":prompt["confirmation"]});
  let mut foreign = headers(Some(&bind), None);
  foreign.insert(
    header::ORIGIN,
    HeaderValue::from_static("https://elsewhere.example.test"),
  );
  assert_eq!(
    request(&app, "POST", APELL_CONFIRM_LINK, consent.clone(), foreign)
      .await
      .0,
    StatusCode::FORBIDDEN
  );
  assert_eq!(
    request(&app, "POST", APELL_CONFIRM_LINK, consent.clone(), headers(None, None))
      .await
      .0,
    StatusCode::BAD_REQUEST
  );
  let signed_in_browser = format!("{bind}; {email_cookie}");
  assert_eq!(
    request(
      &app,
      "POST",
      APELL_CONFIRM_LINK,
      consent.clone(),
      headers(Some(&signed_in_browser), Some(&identity)),
    )
    .await
    .0,
    StatusCode::CONFLICT
  );
  assert_eq!(
    request(&app, "GET", APELL_BASE, Value::Null, owner.clone()).await.2,
    json!({"connection":null})
  );

  let (status, response, signed_in) = request(
    &app,
    "POST",
    APELL_CONFIRM_LINK,
    consent.clone(),
    headers(Some(&bind), None),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(
    signed_in,
    json!({"status":"signed_in", "user_id":identity["user_id"], "username":"alice"})
  );
  assert_eq!(
    cookie(&response, Kind::Apell.cookie()),
    format!("{}=", Kind::Apell.cookie())
  );
  let signed_in_cookie = cookie(&response, BROWSER_COOKIE);
  assert_eq!(
    request(&app, "GET", APELL_BASE, Value::Null, owner).await.2,
    json!({"connection":{"apell_login":"Octocat"}})
  );
  assert_eq!(
    request(&app, "POST", APELL_CONFIRM_LINK, consent, headers(Some(&bind), None))
      .await
      .0,
    StatusCode::BAD_REQUEST
  );
  check_viewer_approval(&accounts, &app, user_code, device_code, &signed_in_cookie, &identity).await;
}

#[tokio::test]
async fn apell_link_confirmation_only_accepts_the_current_unexpired_prompt() {
  let (_directory, accounts, app) = fixture();
  let (email_cookie, identity) = email_session(&app, &accounts).await;
  let (old_bind, old_url) =
    start_provider_flow(&app, Kind::Apell, json!({"mode":"sign_in"}), headers(None, None)).await;
  provider_exchange(&app, Kind::Apell, &old_url, "existing", &old_bind, Some(ISSUER)).await;
  let (_, old_prompt) = apell_link_prompt(&app, &old_bind).await;

  let (bind, url) = start_provider_flow(
    &app,
    Kind::Apell,
    json!({"mode":"sign_in"}),
    headers(Some(&old_bind), None),
  )
  .await;
  provider_exchange(&app, Kind::Apell, &url, "existing", &bind, Some(ISSUER)).await;
  let (_, prompt) = apell_link_prompt(&app, &bind).await;
  assert_ne!(old_prompt["confirmation"], prompt["confirmation"]);
  let (status, _, _) = request(
    &app,
    "POST",
    APELL_CONFIRM_LINK,
    json!({"confirmation":old_prompt["confirmation"]}),
    headers(Some(&bind), None),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);

  let key = flow_key(Kind::Apell, &headers(Some(&bind), None)).unwrap();
  accounts
    .inner
    .federation
    .pending()
    .unwrap()
    .flows
    .get_mut(&key)
    .unwrap()
    .started = Instant::now() - LIFETIME;
  let (status, _, _) = request(
    &app,
    "POST",
    APELL_CONFIRM_LINK,
    json!({"confirmation":prompt["confirmation"]}),
    headers(Some(&bind), None),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert_eq!(
    request(
      &app,
      "GET",
      APELL_BASE,
      Value::Null,
      headers(Some(&email_cookie), Some(&identity))
    )
    .await
    .2,
    json!({"connection":null})
  );
}
