//! GitHub OAuth authenticates the browser; Apell and CLI still redeem their own device codes.

mod provider;
#[cfg(test)]
mod tests;

use std::{
  collections::HashMap,
  sync::{Mutex, MutexGuard},
  time::{Duration, Instant},
};

use axum::{
  Json, Router,
  extract::{RawQuery, State},
  http::{HeaderMap, HeaderValue, StatusCode, header},
  response::{IntoResponse, Redirect, Response},
  routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use subtle::ConstantTimeEq;
use url::form_urlencoded;
use zeroize::Zeroizing;

use super::{
  AccountError, AccountIdentity, Accounts, BrowserAccess, Budget, Hash, hash, random_secret,
  storage::{
    self, CredentialKind,
    github::{self as db, Identity, SignIn},
  },
  valid_secret,
};
use provider::Provider;

pub(super) const CALLBACK: &str = "/hub/v1/account/browser/github/callback";
const FLOW_COOKIE: &str = "__Host-tokn-hub-github";
const LIFETIME: Duration = Duration::from_secs(600);
const MAX_FLOWS: usize = 256;
const MAX_STARTS_PER_MINUTE: u32 = 64;

pub(super) struct GitHub {
  provider: Provider,
  pending: Mutex<Pending>,
}

#[derive(Default)]
struct Pending {
  flows: HashMap<Hash, Flow>,
  starts: Budget,
}

struct Flow {
  started: Instant,
  operation: Operation,
  approval: Option<DeviceApproval>,
  phase: Phase,
}

enum Operation {
  SignIn,
  Link { session_hash: Hash },
}

enum Phase {
  Authorizing {
    state_hash: Hash,
    verifier: Zeroizing<String>,
  },
  Exchanging,
  Ready(Identity),
  Signup(Identity),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
  SignIn,
  Link,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClientKind {
  Cli,
  Viewer,
}

struct DeviceApproval {
  user_code: String,
  device: ClientKind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
  mode: Mode,
  user_code: Option<String>,
  device: Option<ClientKind>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Signup {
  username: String,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Complete {
  SignedIn {
    #[serde(flatten)]
    identity: AccountIdentity,
  },
  Linked,
  SignupRequired {
    suggested_username: String,
  },
  LinkRequired,
}

impl GitHub {
  pub(super) fn from_env(public_url: &str) -> Result<Option<Self>, String> {
    Ok(Provider::from_env(public_url)?.map(Self::new))
  }

  fn new(provider: Provider) -> Self {
    Self {
      provider,
      pending: Mutex::default(),
    }
  }

  fn pending(&self) -> Result<MutexGuard<'_, Pending>, AccountError> {
    self.pending.lock().map_err(AccountError::internal)
  }
}

impl Pending {
  fn live(&mut self, key: &Hash) -> Option<&mut Flow> {
    if self
      .flows
      .get(key)
      .is_some_and(|flow| flow.started.elapsed() >= LIFETIME)
    {
      self.flows.remove(key);
    }
    self.flows.get_mut(key)
  }
}

pub(super) fn router() -> Router<Accounts> {
  Router::new()
    .route("/hub/v1/account/browser/github", get(connection))
    .route("/hub/v1/account/browser/github/start", post(start))
    .route(
      CALLBACK,
      get(callback).head(|| async { StatusCode::METHOD_NOT_ALLOWED }),
    )
    .route("/hub/v1/account/browser/github/complete", post(complete))
    .route("/hub/v1/account/browser/github/signup", post(signup))
}

fn unavailable() -> AccountError {
  AccountError::new(StatusCode::NOT_FOUND, "GitHub sign-in is unavailable on this Hub")
}

fn expired() -> AccountError {
  AccountError::new(StatusCode::BAD_REQUEST, "GitHub sign-in expired. Start again")
}

fn code_challenge(verifier: &str) -> String {
  URL_SAFE_NO_PAD.encode(hash(verifier))
}

fn require_browser(headers: &HeaderMap) -> Result<(), AccountError> {
  if headers.contains_key(header::AUTHORIZATION) {
    return Err(AccountError::forbidden());
  }
  Ok(())
}

fn flow_cookie(value: &str, age: u64) -> Result<HeaderValue, AccountError> {
  let mut header = HeaderValue::from_str(&format!(
    "{FLOW_COOKIE}={value}; Path=/; Max-Age={age}; HttpOnly; SameSite=Lax; Secure"
  ))
  .map_err(|_| AccountError::internal("Invalid GitHub sign-in cookie"))?;
  header.set_sensitive(true);
  Ok(header)
}

fn flow_key(headers: &HeaderMap) -> Result<Hash, AccountError> {
  let mut token = None;
  for value in headers.get_all(header::COOKIE) {
    let value = value.to_str().map_err(|_| expired())?;
    for (name, value) in value.split(';').filter_map(|item| item.trim().split_once('=')) {
      if name == FLOW_COOKIE && (!valid_secret(value) || token.replace(value).is_some()) {
        return Err(expired());
      }
    }
  }
  token.map(hash).ok_or_else(expired)
}

fn ensure_signed_out(accounts: &Accounts, headers: &HeaderMap) -> Result<(), AccountError> {
  if let Ok(token) = accounts.account_token(headers, BrowserAccess::Restore) {
    match storage::credential(&accounts.inner.store, &token) {
      Ok(_) => {
        return Err(AccountError::new(
          StatusCode::CONFLICT,
          "Sign out of your current Hub account first",
        ));
      }
      Err(error) if error.status == StatusCode::UNAUTHORIZED => {}
      Err(error) => return Err(error),
    }
  }
  Ok(())
}

async fn connection(
  State(accounts): State<Accounts>,
  headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AccountError> {
  accounts.inner.github.as_ref().ok_or_else(unavailable)?;
  require_browser(&headers)?;
  let token = accounts.account_token(&headers, BrowserAccess::Read)?;
  accounts
    .blocking(move |accounts| {
      Ok(Json(
        json!({ "connection": db::connection(&accounts.inner.store, &token)? }),
      ))
    })
    .await
}

fn device_approval(
  user_code: Option<String>,
  device: Option<ClientKind>,
) -> Result<Option<DeviceApproval>, AccountError> {
  let invalid = || AccountError::new(StatusCode::BAD_REQUEST, "Invalid device approval code");
  let Some(user_code) = user_code else {
    return if device.is_none() { Ok(None) } else { Err(invalid()) };
  };
  let mut bytes = user_code.bytes();
  if user_code.len() != 9
    || !bytes.by_ref().take(4).all(|byte| byte.is_ascii_alphanumeric())
    || bytes.next() != Some(b'-')
    || !bytes.all(|byte| byte.is_ascii_alphanumeric())
  {
    return Err(invalid());
  }
  Ok(Some(DeviceApproval {
    user_code: user_code.to_ascii_uppercase(),
    device: device.unwrap_or(ClientKind::Cli),
  }))
}

async fn start(
  State(accounts): State<Accounts>,
  headers: HeaderMap,
  Json(request): Json<Start>,
) -> Result<Response, AccountError> {
  accounts.check_origin(&headers, true)?;
  require_browser(&headers)?;
  let approval = device_approval(request.user_code, request.device)?;
  accounts
    .blocking(move |accounts| {
      let github = accounts.inner.github.as_ref().ok_or_else(unavailable)?;
      let operation = match request.mode {
        Mode::SignIn => {
          ensure_signed_out(&accounts, &headers)?;
          Operation::SignIn
        }
        Mode::Link => {
          let token = accounts.account_token(&headers, BrowserAccess::Write)?;
          let identity = storage::credential(&accounts.inner.store, &token)?;
          if identity.kind != CredentialKind::Browser {
            return Err(AccountError::forbidden());
          }
          Operation::Link {
            session_hash: token.hash,
          }
        }
      };
      let mut pending = github.pending()?;
      let now = Instant::now();
      pending.starts.reserve(now, MAX_STARTS_PER_MINUTE)?;
      pending
        .flows
        .retain(|_, flow| now.duration_since(flow.started) < LIFETIME);
      if let Ok(previous) = flow_key(&headers) {
        pending.flows.remove(&previous);
      }
      if pending.flows.len() >= MAX_FLOWS {
        return Err(AccountError::slow_down());
      }
      let binder = Zeroizing::new(random_secret());
      let state = Zeroizing::new(random_secret());
      let verifier = Zeroizing::new(random_secret());
      let url = github.provider.authorization_url(&state, &verifier);
      pending.flows.insert(
        hash(&binder),
        Flow {
          started: now,
          operation,
          approval,
          phase: Phase::Authorizing {
            state_hash: hash(&state),
            verifier,
          },
        },
      );
      Ok(
        (
          [(header::SET_COOKIE, flow_cookie(&binder, LIFETIME.as_secs())?)],
          Json(json!({"authorization_url": url.as_str()})),
        )
          .into_response(),
      )
    })
    .await
}

struct CallbackQuery {
  state: Zeroizing<String>,
  code: Option<Zeroizing<String>>,
}

fn callback_query(query: &str) -> Option<CallbackQuery> {
  if query.len() > 4096 {
    return None;
  }
  let (mut state, mut code, mut error) = (None, None, false);
  for (key, value) in form_urlencoded::parse(query.as_bytes()) {
    match key.as_ref() {
      "state" => {
        if !valid_secret(&value) || state.replace(Zeroizing::new(value.into_owned())).is_some() {
          return None;
        }
      }
      "code" => {
        if !(1..=512).contains(&value.len())
          || !value.bytes().all(|byte| byte.is_ascii_graphic())
          || code.replace(Zeroizing::new(value.into_owned())).is_some()
        {
          return None;
        }
      }
      "error" => {
        if error {
          return None;
        }
        error = true;
      }
      _ => {}
    }
  }
  if code.is_some() == error {
    return None;
  }
  Some(CallbackQuery { state: state?, code })
}

fn return_to_account(success: bool, approval: Option<&DeviceApproval>, clear: bool) -> Result<Response, AccountError> {
  let mut query = form_urlencoded::Serializer::new(String::new());
  query.append_pair("github", if success { "complete" } else { "failed" });
  if let Some(approval) = approval {
    query.append_pair("user_code", &approval.user_code);
    if let ClientKind::Viewer = approval.device {
      query.append_pair("device", "viewer");
    }
  }
  let mut response = Redirect::to(&format!("/account?{}", query.finish())).into_response();
  response
    .headers_mut()
    .insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
  if clear {
    response.headers_mut().insert(header::SET_COOKIE, flow_cookie("", 0)?);
  }
  Ok(response)
}

async fn callback(
  State(accounts): State<Accounts>,
  headers: HeaderMap,
  RawQuery(query): RawQuery,
) -> Result<Response, AccountError> {
  let Some(github) = accounts.inner.github.as_ref() else {
    return return_to_account(false, None, false);
  };
  let (Ok(key), Some(query)) = (flow_key(&headers), query.as_deref().and_then(callback_query)) else {
    return return_to_account(false, None, false);
  };
  let verifier = {
    let mut pending = github.pending()?;
    let Some(flow) = pending.live(&key) else {
      return return_to_account(false, None, false);
    };
    if !matches!(&flow.phase, Phase::Authorizing { state_hash, .. } if bool::from(state_hash.ct_eq(&hash(&query.state))))
    {
      return return_to_account(false, None, false);
    }
    let Phase::Authorizing { verifier, .. } = std::mem::replace(&mut flow.phase, Phase::Exchanging) else {
      unreachable!("only authorizing flows can exchange their code");
    };
    verifier
  };
  let identity = match query.code {
    Some(code) => github.provider.exchange(&code, &verifier).await.ok(),
    None => None,
  };
  let mut pending = github.pending()?;
  let Some(flow) = pending.live(&key) else {
    return return_to_account(false, None, false);
  };
  if let Some(identity) = identity {
    flow.phase = Phase::Ready(identity);
    return_to_account(true, flow.approval.as_ref(), false)
  } else {
    let removed = pending.flows.remove(&key).expect("flow still exists under lock");
    return_to_account(false, removed.approval.as_ref(), true)
  }
}

async fn complete(State(accounts): State<Accounts>, headers: HeaderMap) -> Result<Response, AccountError> {
  accounts.check_origin(&headers, true)?;
  require_browser(&headers)?;
  let key = flow_key(&headers)?;
  accounts
    .blocking(move |accounts| {
      let github = accounts.inner.github.as_ref().ok_or_else(unavailable)?;
      let mut pending = github.pending()?;
      let Some(flow) = pending.live(&key) else {
        return Err(expired());
      };
      if let Phase::Signup(identity) = &flow.phase {
        ensure_signed_out(&accounts, &headers)?;
        return Ok(Json(signup_prompt(identity)).into_response());
      }
      if !matches!(flow.phase, Phase::Ready(_)) {
        return Err(expired());
      }
      let flow = pending.flows.remove(&key).expect("flow still exists under lock");
      let Phase::Ready(identity) = flow.phase else {
        unreachable!("verified ready")
      };
      let (outcome, cookie) = match flow.operation {
        Operation::SignIn => {
          ensure_signed_out(&accounts, &headers)?;
          match db::sign_in(&accounts.inner.store, &identity)? {
            SignIn::SignedIn(login) => {
              let cookie = accounts.browser_login_cookie(&login)?;
              (
                Complete::SignedIn {
                  identity: login.identity,
                },
                Some(cookie),
              )
            }
            SignIn::SignupRequired => {
              let outcome = signup_prompt(&identity);
              pending.flows.insert(
                key,
                Flow {
                  phase: Phase::Signup(identity),
                  ..flow
                },
              );
              return Ok(Json(outcome).into_response());
            }
            SignIn::LinkRequired => (Complete::LinkRequired, None),
          }
        }
        Operation::Link { session_hash } => {
          let token = accounts.account_token(&headers, BrowserAccess::Write)?;
          if !bool::from(token.hash.ct_eq(&session_hash)) {
            return Err(AccountError::session_changed());
          }
          db::link(&accounts.inner.store, &token, &identity)?;
          (Complete::Linked, None)
        }
      };
      finish_response(outcome, cookie)
    })
    .await
}

fn signup_prompt(identity: &Identity) -> Complete {
  let suggested = identity.login.to_ascii_lowercase();
  let suggested_username =
    if tokn_hub_client_core::address::parse_machine_address(&format!("{suggested}:machine")).is_ok() {
      suggested
    } else {
      String::new()
    };
  Complete::SignupRequired { suggested_username }
}

async fn signup(
  State(accounts): State<Accounts>,
  headers: HeaderMap,
  Json(request): Json<Signup>,
) -> Result<Response, AccountError> {
  accounts.check_origin(&headers, true)?;
  require_browser(&headers)?;
  let key = flow_key(&headers)?;
  accounts
    .blocking(move |accounts| {
      let github = accounts.inner.github.as_ref().ok_or_else(unavailable)?;
      let mut pending = github.pending()?;
      let Some(flow) = pending.live(&key) else {
        return Err(expired());
      };
      let Phase::Signup(identity) = &flow.phase else {
        return Err(expired());
      };
      ensure_signed_out(&accounts, &headers)?;
      let login = db::sign_up(&accounts.inner.store, identity, request.username.trim())?;
      let cookie = accounts.browser_login_cookie(&login)?;
      pending.flows.remove(&key);
      finish_response(login.identity, Some(cookie))
    })
    .await
}

fn finish_response(outcome: impl Serialize, account: Option<HeaderValue>) -> Result<Response, AccountError> {
  let mut response = Json(outcome).into_response();
  let headers = response.headers_mut();
  headers.insert(header::SET_COOKIE, flow_cookie("", 0)?);
  if let Some(account) = account {
    headers.append(header::SET_COOKIE, account);
  }
  Ok(response)
}
