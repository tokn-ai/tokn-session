//! Hub accounts own directory entries and authenticate account API requests.

mod mail;
mod storage;

#[cfg(test)]
mod tests;

use std::{
  collections::HashMap,
  sync::{Arc, Mutex, MutexGuard},
  time::{Duration, Instant},
};

use axum::{
  Json, Router,
  extract::{DefaultBodyLimit, Path, Request, State},
  http::{HeaderMap, HeaderValue, StatusCode, header},
  middleware::{self, Next},
  response::{IntoResponse, Response},
  routing::{delete, get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

use crate::store::Store;
use mail::Sendmail;
use storage::{Credential, CredentialKind};

const MAIL_LIFETIME: Duration = Duration::from_secs(600);
const MAIL_RESEND: Duration = Duration::from_secs(60);
const MAX_MAIL_REQUESTS_PER_MINUTE: u32 = 16;
const MAX_MAIL_ATTEMPTS: u8 = 5;
const DEVICE_LIFETIME: Duration = Duration::from_secs(600);
const DEVICE_INTERVAL: u64 = 5;
const DEVICE_POLL_TOLERANCE: Duration = Duration::from_millis(500);
const MAX_DEVICE_REQUESTS_PER_MINUTE: u32 = 64;
const MAX_DEVICE_APPROVALS_PER_MINUTE: u32 = 128;
const MAX_DEVICE_ATTEMPTS_PER_ACCOUNT: u32 = 10;
const USER_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const BROWSER_COOKIE: &str = "__Host-tokn-hub-account";
const LOCAL_BROWSER_COOKIE: &str = "tokn_hub_account_dev";
const ACCOUNT_ID_HEADER: &str = "x-tokn-account-id";
const BEARER_TOKEN_PREFIX: &str = "tkna_";
const COOKIE_TOKEN_PREFIX: &str = "tknw_";

pub const MACHINES_READ: &str = "machines:read";
pub const MACHINES_REGISTER: &str = "machines:register";

type Hash = [u8; 32];

#[derive(Clone, Debug, Serialize)]
pub struct AccountIdentity {
  pub user_id: String,
  pub username: String,
}

#[derive(Debug)]
pub struct AccountError {
  pub status: StatusCode,
  message: &'static str,
  username_required: bool,
  account_session_changed: bool,
}

impl AccountError {
  fn new(status: StatusCode, message: &'static str) -> Self {
    Self {
      status,
      message,
      username_required: false,
      account_session_changed: false,
    }
  }

  fn unauthorized() -> Self {
    Self::new(StatusCode::UNAUTHORIZED, "Sign in to your Hub account")
  }

  fn session_changed() -> Self {
    Self {
      account_session_changed: true,
      ..Self::new(StatusCode::CONFLICT, "Your browser Hub account session changed")
    }
  }

  fn forbidden() -> Self {
    Self::new(
      StatusCode::FORBIDDEN,
      "This credential does not allow that account action",
    )
  }

  fn unavailable() -> Self {
    Self::new(
      StatusCode::SERVICE_UNAVAILABLE,
      "Hub account email delivery is unavailable",
    )
  }

  fn internal(error: impl std::fmt::Display) -> Self {
    eprintln!("Hub account operation failed: {error}");
    Self::new(
      StatusCode::INTERNAL_SERVER_ERROR,
      "Hub accounts are temporarily unavailable",
    )
  }

  fn slow_down() -> Self {
    Self::new(
      StatusCode::TOO_MANY_REQUESTS,
      "Too many attempts. Wait before trying again",
    )
  }
}

impl IntoResponse for AccountError {
  fn into_response(self) -> Response {
    let body = if self.username_required {
      json!({"error": self.message, "username_required": true})
    } else if self.account_session_changed {
      json!({"error": self.message, "account_session_changed": true})
    } else {
      json!({"error": self.message})
    };
    (self.status, Json(body)).into_response()
  }
}

#[derive(Clone)]
pub struct Accounts {
  inner: Arc<Inner>,
}

struct Inner {
  store: Store,
  origin: String,
  local_http: bool,
  mailer: Option<Sendmail>,
  code_key: Hash,
  pending: Mutex<Pending>,
}

struct AccountToken {
  hash: Hash,
  from_cookie: bool,
  expected_user_id: Option<String>,
}

#[derive(Clone, Copy)]
enum BrowserAccess {
  Restore,
  Read,
  Write,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenTransport {
  Bearer,
  Cookie,
}

impl TokenTransport {
  fn prefix(self) -> &'static str {
    match self {
      Self::Bearer => BEARER_TOKEN_PREFIX,
      Self::Cookie => COOKIE_TOKEN_PREFIX,
    }
  }

  fn valid_token(self, token: &str) -> bool {
    token.strip_prefix(self.prefix()).is_some_and(valid_secret)
  }
}

#[derive(Default)]
struct Pending {
  emails: HashMap<String, EmailCode>,
  devices: HashMap<Hash, DeviceCode>,
  mail_requests: Budget,
  device_requests: Budget,
  device_approvals: Budget,
  approval_attempts: HashMap<String, Budget>,
}

struct EmailCode {
  hash: Hash,
  issued_at: Instant,
  attempts: u8,
}

struct DeviceCode {
  user_code: String,
  expires_at: Instant,
  next_poll: Instant,
  user_id: Option<String>,
}

#[derive(Default)]
struct Budget {
  window: Option<Instant>,
  used: u32,
}

impl Budget {
  fn reserve(&mut self, now: Instant, maximum: u32) -> Result<(), AccountError> {
    if self
      .window
      .is_none_or(|window| now.duration_since(window) >= MAIL_RESEND)
    {
      self.window = Some(now);
      self.used = 0;
    }
    if self.used >= maximum {
      return Err(AccountError::slow_down());
    }
    self.used += 1;
    Ok(())
  }
}

impl Accounts {
  pub fn new(store: Store, public_url: &str) -> Result<Self, String> {
    Self::with_mailer(store, public_url, Sendmail::from_env()?)
  }

  fn with_mailer(store: Store, public_url: &str, mailer: Option<Sendmail>) -> Result<Self, String> {
    let public_url = Url::parse(public_url).map_err(|_| "Invalid Hub URL for account authentication")?;
    let origin = public_url.origin().ascii_serialization();
    let local_http = public_url.scheme() == "http" && public_url.domain() == Some("localhost");
    storage::initialize(&store)?;
    let mut code_key = Hash::default();
    OsRng.fill_bytes(&mut code_key);
    Ok(Self {
      inner: Arc::new(Inner {
        store,
        origin,
        local_http,
        mailer,
        code_key,
        pending: Mutex::new(Pending::default()),
      }),
    })
  }

  fn email_configured(&self) -> bool {
    self.inner.mailer.is_some()
  }

  pub fn router(&self) -> Router {
    Router::new()
      .route("/hub/v1/account/capabilities", get(capabilities))
      .route("/hub/v1/account/email/start", post(email_start))
      .route("/hub/v1/account/email/verify", post(email_verify))
      .route("/hub/v1/account/browser/email/verify", post(browser_email_verify))
      .route("/hub/v1/account/browser/refresh", post(browser_refresh))
      .route("/hub/v1/account/device/start", post(device_start))
      .route("/hub/v1/account/device/approve", post(device_approve))
      .route("/hub/v1/account/device/token", post(device_token))
      .route("/hub/v1/account/me", get(me))
      .route("/hub/v1/account/logout", post(logout))
      .route("/hub/v1/account/keys", get(keys).post(create_key))
      .route("/hub/v1/account/keys/{key_id}", delete(revoke_key))
      .layer(DefaultBodyLimit::max(4096))
      .layer(middleware::from_fn_with_state(self.clone(), account_headers))
      .with_state(self.clone())
  }

  pub async fn authenticate(&self, headers: &HeaderMap, required_scope: &str) -> Result<AccountIdentity, AccountError> {
    self.check_origin(headers, false)?;
    self
      .authenticate_scope(
        AccountToken {
          hash: bearer_hash(headers)?,
          from_cookie: false,
          expected_user_id: None,
        },
        required_scope,
      )
      .await
  }

  pub async fn authenticate_machine_reader(&self, headers: &HeaderMap) -> Result<AccountIdentity, AccountError> {
    self.check_origin(headers, false)?;
    self
      .authenticate_scope(self.account_token(headers, BrowserAccess::Read)?, MACHINES_READ)
      .await
  }

  async fn authenticate_scope(
    &self,
    token: AccountToken,
    required_scope: &str,
  ) -> Result<AccountIdentity, AccountError> {
    let credential = self.credential(token).await?;
    if !credential.permits(required_scope) {
      return Err(AccountError::forbidden());
    }
    Ok(credential.identity)
  }

  async fn credential(&self, token: AccountToken) -> Result<Credential, AccountError> {
    self
      .blocking(move |state| storage::credential(&state.inner.store, &token))
      .await
  }

  fn account_token(&self, headers: &HeaderMap, access: BrowserAccess) -> Result<AccountToken, AccountError> {
    if headers.contains_key(header::AUTHORIZATION) {
      return Ok(AccountToken {
        hash: bearer_hash(headers)?,
        from_cookie: false,
        expected_user_id: None,
      });
    }
    let token = cookie_token(headers, self.cookie_name())?;
    if matches!(access, BrowserAccess::Write) {
      self.check_origin(headers, true)?;
    }
    let expected_user_id = if matches!(access, BrowserAccess::Restore) {
      None
    } else {
      Some(expected_account_id(headers)?.to_owned())
    };
    Ok(AccountToken {
      hash: hash(token),
      from_cookie: true,
      expected_user_id,
    })
  }

  fn cookie_name(&self) -> &'static str {
    if self.inner.local_http {
      LOCAL_BROWSER_COOKIE
    } else {
      BROWSER_COOKIE
    }
  }

  fn browser_cookie(&self, value: &str, max_age: i64) -> Result<HeaderValue, AccountError> {
    let (path, security) = if self.inner.local_http {
      ("/hub/v1/account", "")
    } else {
      ("/", "; Secure")
    };
    let mut cookie = HeaderValue::from_str(&format!(
      "{}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Strict{security}",
      self.cookie_name()
    ))
    .map_err(|_| AccountError::internal("Invalid Hub account cookie"))?;
    cookie.set_sensitive(true);
    Ok(cookie)
  }

  fn check_origin(&self, headers: &HeaderMap, required: bool) -> Result<(), AccountError> {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    match origins.next() {
      Some(origin) if origin.to_str().ok() == Some(self.inner.origin.as_str()) && origins.next().is_none() => Ok(()),
      None if !required => Ok(()),
      _ => Err(AccountError::new(
        StatusCode::FORBIDDEN,
        "Request origin does not match the Hub",
      )),
    }
  }

  fn pending(&self) -> Result<MutexGuard<'_, Pending>, AccountError> {
    self.inner.pending.lock().map_err(AccountError::internal)
  }

  async fn blocking<T: Send + 'static>(
    &self,
    operation: impl FnOnce(Accounts) -> Result<T, AccountError> + Send + 'static,
  ) -> Result<T, AccountError> {
    let state = self.clone();
    tokio::task::spawn_blocking(move || operation(state))
      .await
      .map_err(AccountError::internal)?
  }

  fn mail_hash(&self, email: &str, code: &str) -> Hash {
    Sha256::new()
      .chain_update(self.inner.code_key)
      .chain_update(email.as_bytes())
      .chain_update(code.as_bytes())
      .finalize()
      .into()
  }

  fn issue_email_code(&self, email: &str) -> Result<(String, Hash), AccountError> {
    let now = Instant::now();
    let mut pending = self.pending()?;
    pending
      .emails
      .retain(|_, code| now.duration_since(code.issued_at) < MAIL_LIFETIME);
    if pending
      .emails
      .get(email)
      .is_some_and(|code| now.duration_since(code.issued_at) < MAIL_RESEND)
    {
      return Err(AccountError::slow_down());
    }
    pending.mail_requests.reserve(now, MAX_MAIL_REQUESTS_PER_MINUTE)?;
    let code = format!("{:06}", OsRng.gen_range(0..1_000_000));
    let hash = self.mail_hash(email, &code);
    pending.emails.insert(
      email.to_owned(),
      EmailCode {
        hash,
        issued_at: now,
        attempts: 0,
      },
    );
    Ok((code, hash))
  }

  fn verify_email_code(
    &self,
    email: &str,
    code: &str,
    username: Option<&str>,
    transport: TokenTransport,
  ) -> Result<Login, AccountError> {
    let invalid = || AccountError::new(StatusCode::UNAUTHORIZED, "Email code is invalid or expired");
    let mut pending = self.pending()?;
    let challenge = pending.emails.get_mut(email).ok_or_else(invalid)?;
    if challenge.issued_at.elapsed() >= MAIL_LIFETIME || challenge.attempts >= MAX_MAIL_ATTEMPTS {
      return Err(invalid());
    }
    if code.len() != 6
      || !code.bytes().all(|byte| byte.is_ascii_digit())
      || !bool::from(self.mail_hash(email, code).ct_eq(&challenge.hash))
    {
      challenge.attempts += 1;
      return Err(invalid());
    }
    let login = storage::email_login(&self.inner.store, email, username, transport)?;
    challenge.attempts = MAX_MAIL_ATTEMPTS;
    Ok(login)
  }
}

async fn capabilities(State(state): State<Accounts>) -> Json<serde_json::Value> {
  Json(json!({"enabled": true, "email_configured": state.email_configured()}))
}

async fn account_headers(State(state): State<Accounts>, request: Request, next: Next) -> Response {
  let mut response = match state.check_origin(request.headers(), false) {
    Ok(()) => next.run(request).await,
    Err(error) => error.into_response(),
  };
  response
    .headers_mut()
    .insert(header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
  response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmailRequest {
  email: String,
}

async fn email_start(
  State(state): State<Accounts>,
  Json(request): Json<EmailRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AccountError> {
  let mailer = state.inner.mailer.as_ref().ok_or_else(|| {
    AccountError::new(
      StatusCode::SERVICE_UNAVAILABLE,
      "Sign-in email is unavailable. Ask the Hub administrator to configure email delivery.",
    )
  })?;
  let email = mail::mailbox(&request.email)
    .ok_or_else(|| AccountError::new(StatusCode::BAD_REQUEST, "Enter a valid email address"))?;
  let (code, hash) = state
    .blocking({
      let email = email.clone();
      move |state| state.issue_email_code(&email)
    })
    .await?;
  if mailer.deliver(&email, &code).await.is_err() {
    state
      .blocking(move |state| {
        let mut pending = state.pending()?;
        if let Some(challenge) = pending.emails.get_mut(&email)
          && bool::from(hash.ct_eq(&challenge.hash))
        {
          // Retain the resend cooldown even if the mail process fails.
          challenge.attempts = MAX_MAIL_ATTEMPTS;
        }
        Ok(())
      })
      .await?;
    return Err(AccountError::unavailable());
  }
  Ok((StatusCode::ACCEPTED, Json(json!({}))))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyRequest {
  email: String,
  code: String,
  username: Option<String>,
}

#[derive(Serialize)]
struct Login {
  #[serde(flatten)]
  identity: AccountIdentity,
  token: String,
}

async fn email_verify(
  State(state): State<Accounts>,
  Json(request): Json<VerifyRequest>,
) -> Result<Json<Login>, AccountError> {
  verify_login(&state, request, TokenTransport::Bearer).await.map(Json)
}

async fn browser_email_verify(
  State(state): State<Accounts>,
  headers: HeaderMap,
  Json(request): Json<VerifyRequest>,
) -> Result<Response, AccountError> {
  state.check_origin(&headers, true)?;
  let login = verify_login(&state, request, TokenTransport::Cookie).await?;
  let cookie = state.browser_cookie(&login.token, storage::BROWSER_COOKIE_TTL)?;
  Ok(([(header::SET_COOKIE, cookie)], Json(login.identity)).into_response())
}

async fn browser_refresh(State(state): State<Accounts>, headers: HeaderMap) -> Result<Response, AccountError> {
  state.check_origin(&headers, true)?;
  if headers.contains_key(header::AUTHORIZATION) {
    return Err(AccountError::unauthorized());
  }
  let value = cookie_token(&headers, state.cookie_name())?;
  let token = AccountToken {
    hash: hash(value),
    from_cookie: true,
    expected_user_id: None,
  };
  let (identity, renewed) = state
    .blocking(move |state| storage::refresh_browser(&state.inner.store, &token))
    .await?;
  let mut response = Json(identity).into_response();
  if renewed {
    response.headers_mut().insert(
      header::SET_COOKIE,
      state.browser_cookie(value, storage::BROWSER_COOKIE_TTL)?,
    );
  }
  Ok(response)
}

async fn verify_login(
  state: &Accounts,
  request: VerifyRequest,
  transport: TokenTransport,
) -> Result<Login, AccountError> {
  let email = mail::mailbox(&request.email)
    .ok_or_else(|| AccountError::new(StatusCode::BAD_REQUEST, "Enter a valid email address"))?;
  state
    .blocking(move |state| state.verify_email_code(&email, request.code.trim(), request.username.as_deref(), transport))
    .await
}

#[derive(Serialize)]
struct DeviceStart {
  device_code: String,
  user_code: String,
  expires_in: u64,
  interval: u64,
}

async fn device_start(State(state): State<Accounts>) -> Result<Json<DeviceStart>, AccountError> {
  let now = Instant::now();
  let mut pending = state.pending()?;
  pending.devices.retain(|_, device| device.expires_at > now);
  pending.device_requests.reserve(now, MAX_DEVICE_REQUESTS_PER_MINUTE)?;

  let user_code = loop {
    let candidate = new_user_code();
    if !pending.devices.values().any(|device| device.user_code == candidate) {
      break candidate;
    }
  };
  let device_code = random_secret();
  pending.devices.insert(
    hash(&device_code),
    DeviceCode {
      user_code: user_code.clone(),
      expires_at: now + DEVICE_LIFETIME,
      next_poll: now,
      user_id: None,
    },
  );
  Ok(Json(DeviceStart {
    device_code,
    user_code,
    expires_in: DEVICE_LIFETIME.as_secs(),
    interval: DEVICE_INTERVAL,
  }))
}

fn new_user_code() -> String {
  let mut code = String::with_capacity(9);
  for index in 0..8 {
    if index == 4 {
      code.push('-');
    }
    code.push(USER_CODE_ALPHABET[OsRng.gen_range(0..USER_CODE_ALPHABET.len())] as char);
  }
  code
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveRequest {
  user_code: String,
}

async fn device_approve(
  State(state): State<Accounts>,
  headers: HeaderMap,
  Json(request): Json<ApproveRequest>,
) -> Result<StatusCode, AccountError> {
  let credential = state
    .credential(state.account_token(&headers, BrowserAccess::Write)?)
    .await?;
  if credential.kind != CredentialKind::Browser {
    return Err(AccountError::forbidden());
  }
  let now = Instant::now();
  let mut pending = state.pending()?;
  pending.approval_attempts.retain(|_, budget| {
    budget
      .window
      .is_some_and(|start| now.duration_since(start) < MAIL_RESEND)
  });
  pending.device_approvals.reserve(now, MAX_DEVICE_APPROVALS_PER_MINUTE)?;
  pending
    .approval_attempts
    .entry(credential.identity.user_id.clone())
    .or_default()
    .reserve(now, MAX_DEVICE_ATTEMPTS_PER_ACCOUNT)?;
  let code = request.user_code.trim().to_ascii_uppercase();
  let device = pending
    .devices
    .values_mut()
    .find(|device| device.expires_at > now && device.user_code == code && device.user_id.is_none())
    .ok_or_else(|| AccountError::new(StatusCode::NOT_FOUND, "Device code is invalid or expired"))?;
  device.user_id = Some(credential.identity.user_id);
  Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceTokenRequest {
  device_code: String,
}

async fn device_token(
  State(state): State<Accounts>,
  Json(request): Json<DeviceTokenRequest>,
) -> Result<Response, AccountError> {
  state
    .blocking(move |state| {
      let invalid = || AccountError::new(StatusCode::BAD_REQUEST, "Device login is invalid or expired");
      if !valid_secret(&request.device_code) {
        return Err(invalid());
      }
      let key = hash(&request.device_code);
      let now = Instant::now();
      let mut pending = state.pending()?;
      let device = pending
        .devices
        .get_mut(&key)
        .filter(|device| device.expires_at > now)
        .ok_or_else(invalid)?;
      if device.next_poll > now + DEVICE_POLL_TOLERANCE {
        return Err(AccountError::slow_down());
      }
      device.next_poll = now + Duration::from_secs(DEVICE_INTERVAL);
      let Some(user_id) = device.user_id.as_deref() else {
        return Ok(StatusCode::ACCEPTED.into_response());
      };
      let login = storage::device_login(&state.inner.store, user_id)?;
      pending.devices.remove(&key);
      Ok(Json(login).into_response())
    })
    .await
}

async fn me(State(state): State<Accounts>, headers: HeaderMap) -> Result<Json<AccountIdentity>, AccountError> {
  Ok(Json(
    state
      .credential(state.account_token(&headers, BrowserAccess::Restore)?)
      .await?
      .identity,
  ))
}

async fn logout(State(state): State<Accounts>, headers: HeaderMap) -> Result<Response, AccountError> {
  let token = state.account_token(&headers, BrowserAccess::Write)?;
  let from_cookie = token.from_cookie;
  state
    .blocking(move |state| storage::logout(&state.inner.store, &token))
    .await?;
  let mut response = StatusCode::NO_CONTENT.into_response();
  if from_cookie {
    response
      .headers_mut()
      .insert(header::SET_COOKIE, state.browser_cookie("", 0)?);
  }
  Ok(response)
}

async fn keys(State(state): State<Accounts>, headers: HeaderMap) -> Result<Json<serde_json::Value>, AccountError> {
  let token = state.account_token(&headers, BrowserAccess::Read)?;
  let keys = state
    .blocking(move |state| storage::keys(&state.inner.store, &token))
    .await?;
  Ok(Json(json!({"keys": keys})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyRequest {
  name: String,
  scopes: Vec<String>,
  expires_in: Option<u64>,
}

async fn create_key(
  State(state): State<Accounts>,
  headers: HeaderMap,
  Json(request): Json<KeyRequest>,
) -> Result<(StatusCode, Json<storage::CreatedKey>), AccountError> {
  let token = state.account_token(&headers, BrowserAccess::Write)?;
  let key = state
    .blocking(move |state| storage::create_key(&state.inner.store, &token, request))
    .await?;
  Ok((StatusCode::CREATED, Json(key)))
}

async fn revoke_key(
  State(state): State<Accounts>,
  headers: HeaderMap,
  Path(key_id): Path<String>,
) -> Result<StatusCode, AccountError> {
  let token = state.account_token(&headers, BrowserAccess::Write)?;
  state
    .blocking(move |state| storage::revoke_key(&state.inner.store, &token, &key_id))
    .await?;
  Ok(StatusCode::NO_CONTENT)
}

fn random_secret() -> String {
  let mut bytes = Hash::default();
  OsRng.fill_bytes(&mut bytes);
  URL_SAFE_NO_PAD.encode(bytes)
}

fn valid_secret(secret: &str) -> bool {
  secret.len() == 43
    && secret
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn hash(secret: &str) -> Hash {
  Sha256::digest(secret.as_bytes()).into()
}

fn bearer_hash(headers: &HeaderMap) -> Result<Hash, AccountError> {
  let mut headers = headers.get_all(header::AUTHORIZATION).iter();
  let token = headers
    .next()
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.strip_prefix("Bearer "))
    .filter(|token| TokenTransport::Bearer.valid_token(token))
    .ok_or_else(AccountError::unauthorized)?;
  if headers.next().is_some() {
    return Err(AccountError::unauthorized());
  }
  Ok(hash(token))
}

fn cookie_token<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, AccountError> {
  let mut token = None;
  for value in headers.get_all(header::COOKIE) {
    let value = value.to_str().map_err(|_| AccountError::unauthorized())?;
    for (cookie_name, cookie_value) in value.split(';').filter_map(|cookie| cookie.trim().split_once('=')) {
      if cookie_name == name
        && (!TokenTransport::Cookie.valid_token(cookie_value) || token.replace(cookie_value).is_some())
      {
        return Err(AccountError::unauthorized());
      }
    }
  }
  token.ok_or_else(AccountError::unauthorized)
}

fn expected_account_id(headers: &HeaderMap) -> Result<&str, AccountError> {
  let mut values = headers.get_all(ACCOUNT_ID_HEADER).iter();
  let value = values
    .next()
    .and_then(|value| value.to_str().ok())
    .ok_or_else(AccountError::session_changed)?;
  if values.next().is_some() {
    return Err(AccountError::session_changed());
  }
  Ok(value)
}
