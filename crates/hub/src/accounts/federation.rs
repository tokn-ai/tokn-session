//! External providers authenticate the Hub browser; native clients still redeem their own device codes.

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
  AccountError, AccountIdentity, Accounts, BrowserAccess, Budget, Hash, hash,
  oidc::Oidc,
  random_secret,
  storage::{self, CredentialKind, SignIn, apell, github},
  valid_secret,
};
use provider::Provider;

const GITHUB_CALLBACK: &str = "/hub/v1/account/browser/github/callback";
pub(super) const APELL_CALLBACK: &str = "/hub/v1/account/browser/apell/callback";
const LIFETIME: Duration = Duration::from_secs(600);
const MAX_FLOWS: usize = 256;
const MAX_STARTS_PER_MINUTE: u32 = 64;

#[derive(Default)]
pub(super) struct Federation {
  github: Option<Provider>,
  apell: Option<Oidc>,
  pending: Mutex<Pending>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
  GitHub,
  Apell,
}

impl Kind {
  fn label(self) -> &'static str {
    match self {
      Self::GitHub => "github",
      Self::Apell => "apell",
    }
  }

  fn cookie(self) -> &'static str {
    match self {
      Self::GitHub => "__Host-tokn-hub-github",
      Self::Apell => "__Host-tokn-hub-apell",
    }
  }

  fn callback(self) -> &'static str {
    match self {
      Self::GitHub => GITHUB_CALLBACK,
      Self::Apell => APELL_CALLBACK,
    }
  }

  fn unavailable(self) -> AccountError {
    AccountError::new(
      StatusCode::NOT_FOUND,
      match self {
        Self::GitHub => "GitHub sign-in is unavailable on this Hub",
        Self::Apell => "Apell sign-in is unavailable on this Hub",
      },
    )
  }

  fn expired(self) -> AccountError {
    AccountError::new(
      StatusCode::BAD_REQUEST,
      match self {
        Self::GitHub => "GitHub sign-in expired. Start again",
        Self::Apell => "Apell sign-in expired. Start again",
      },
    )
  }
}

enum External<'a> {
  GitHub(&'a Provider),
  Apell(&'a Oidc),
}

impl External<'_> {
  async fn authorization_url(&self, state: &str, verifier: &str, nonce: &str) -> Result<url::Url, AccountError> {
    match self {
      Self::GitHub(provider) => Ok(provider.authorization_url(state, verifier)),
      Self::Apell(provider) => provider.authorization_url(state, verifier, nonce).await,
    }
  }

  fn response_issuer_matches(&self, issuer: Option<&str>) -> bool {
    match self {
      Self::GitHub(_) => issuer.is_none(),
      Self::Apell(provider) => issuer.is_none_or(|issuer| issuer == provider.issuer()),
    }
  }

  async fn exchange(&self, code: &str, verifier: &str, nonce: &str) -> Option<Identity> {
    match self {
      Self::GitHub(provider) => provider.exchange(code, verifier).await.ok().map(Identity::GitHub),
      Self::Apell(provider) => provider.exchange(code, verifier, nonce).await.ok().map(Identity::Apell),
    }
  }
}

enum Identity {
  GitHub(github::Identity),
  Apell(apell::Identity),
}

impl Identity {
  fn sign_in(&self, store: &super::Store) -> Result<SignIn, AccountError> {
    match self {
      Self::GitHub(identity) => github::sign_in(store, identity),
      Self::Apell(identity) => apell::sign_in(store, identity),
    }
  }

  fn sign_up(&self, store: &super::Store, username: &str) -> Result<super::Login, AccountError> {
    match self {
      Self::GitHub(identity) => github::sign_up(store, identity, username),
      Self::Apell(identity) => apell::sign_up(store, identity, username),
    }
  }

  fn link(&self, store: &super::Store, token: &super::AccountToken) -> Result<(), AccountError> {
    match self {
      Self::GitHub(identity) => github::link(store, token, identity),
      Self::Apell(identity) => apell::link(store, token, identity),
    }
  }

  fn suggested_username(&self) -> Option<&str> {
    match self {
      Self::GitHub(identity) => Some(&identity.login),
      Self::Apell(identity) => identity.login.as_deref(),
    }
  }
}

#[derive(Default)]
struct Pending {
  flows: HashMap<Hash, Flow>,
  starts: Budget,
}

struct Flow {
  kind: Kind,
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
    nonce: Zeroizing<String>,
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

impl Federation {
  pub(super) fn from_env(public_url: &str) -> Result<Self, String> {
    Ok(Self {
      github: Provider::from_env(public_url)?,
      apell: Oidc::from_env(public_url)?,
      pending: Mutex::default(),
    })
  }

  pub(super) fn github_configured(&self) -> bool {
    self.github.is_some()
  }

  pub(super) fn apell_issuer(&self) -> Option<&str> {
    self.apell.as_ref().map(Oidc::issuer)
  }

  fn provider(&self, kind: Kind) -> Option<External<'_>> {
    match kind {
      Kind::GitHub => self.github.as_ref().map(External::GitHub),
      Kind::Apell => self.apell.as_ref().map(External::Apell),
    }
  }

  fn pending(&self) -> Result<MutexGuard<'_, Pending>, AccountError> {
    self.pending.lock().map_err(AccountError::internal)
  }
}

impl Pending {
  fn live(&mut self, kind: Kind, key: &Hash) -> Option<&mut Flow> {
    if self
      .flows
      .get(key)
      .is_some_and(|flow| flow.started.elapsed() >= LIFETIME)
    {
      self.flows.remove(key);
    }
    self.flows.get_mut(key).filter(|flow| flow.kind == kind)
  }
}

pub(super) fn router() -> Router<Accounts> {
  [Kind::GitHub, Kind::Apell]
    .into_iter()
    .fold(Router::new(), |router, kind| {
      let base = format!("/hub/v1/account/browser/{}", kind.label());
      router
        .route(&base, get(move |state, headers| connection(kind, state, headers)))
        .route(
          &format!("{base}/start"),
          post(move |state, headers, request| start(kind, state, headers, request)),
        )
        .route(
          kind.callback(),
          get(move |state, headers, query| callback(kind, state, headers, query))
            .head(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .route(
          &format!("{base}/complete"),
          post(move |state, headers| complete(kind, state, headers)),
        )
        .route(
          &format!("{base}/signup"),
          post(move |state, headers, request| signup(kind, state, headers, request)),
        )
    })
}

pub(super) fn code_challenge(verifier: &str) -> String {
  URL_SAFE_NO_PAD.encode(hash(verifier))
}

fn require_browser(headers: &HeaderMap) -> Result<(), AccountError> {
  if headers.contains_key(header::AUTHORIZATION) {
    return Err(AccountError::forbidden());
  }
  Ok(())
}

fn flow_cookie(kind: Kind, value: &str, age: u64) -> Result<HeaderValue, AccountError> {
  let mut header = HeaderValue::from_str(&format!(
    "{}={value}; Path=/; Max-Age={age}; HttpOnly; SameSite=Lax; Secure",
    kind.cookie()
  ))
  .map_err(|_| AccountError::internal("Invalid federated sign-in cookie"))?;
  header.set_sensitive(true);
  Ok(header)
}

fn flow_key(kind: Kind, headers: &HeaderMap) -> Result<Hash, AccountError> {
  let mut token = None;
  for value in headers.get_all(header::COOKIE) {
    let value = value.to_str().map_err(|_| kind.expired())?;
    for (name, value) in value.split(';').filter_map(|item| item.trim().split_once('=')) {
      if name == kind.cookie() && (!valid_secret(value) || token.replace(value).is_some()) {
        return Err(kind.expired());
      }
    }
  }
  token.map(hash).ok_or_else(|| kind.expired())
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
  kind: Kind,
  State(accounts): State<Accounts>,
  headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AccountError> {
  accounts
    .inner
    .federation
    .provider(kind)
    .ok_or_else(|| kind.unavailable())?;
  require_browser(&headers)?;
  let token = accounts.account_token(&headers, BrowserAccess::Read)?;
  accounts
    .blocking(move |accounts| {
      let store = &accounts.inner.store;
      let value = match kind {
        Kind::GitHub => json!({ "connection": github::connection(store, &token)? }),
        Kind::Apell => {
          let issuer = accounts
            .inner
            .federation
            .apell_issuer()
            .ok_or_else(|| kind.unavailable())?;
          json!({ "connection": apell::connection(store, &token, issuer)? })
        }
      };
      Ok(Json(value))
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
  kind: Kind,
  State(accounts): State<Accounts>,
  headers: HeaderMap,
  Json(request): Json<Start>,
) -> Result<Response, AccountError> {
  accounts.check_origin(&headers, true)?;
  require_browser(&headers)?;
  let federation = &accounts.inner.federation;
  let provider = federation.provider(kind).ok_or_else(|| kind.unavailable())?;
  let approval = device_approval(request.user_code, request.device)?;
  let previous = flow_key(kind, &headers).ok();
  let operation = accounts
    .blocking(move |accounts| match request.mode {
      Mode::SignIn => {
        ensure_signed_out(&accounts, &headers)?;
        Ok(Operation::SignIn)
      }
      Mode::Link => {
        let token = accounts.account_token(&headers, BrowserAccess::Write)?;
        let identity = storage::credential(&accounts.inner.store, &token)?;
        if identity.kind != CredentialKind::Browser {
          return Err(AccountError::forbidden());
        }
        Ok(Operation::Link {
          session_hash: token.hash,
        })
      }
    })
    .await?;
  federation
    .pending()?
    .starts
    .reserve(Instant::now(), MAX_STARTS_PER_MINUTE)?;
  let binder = Zeroizing::new(random_secret());
  let state = Zeroizing::new(random_secret());
  let verifier = Zeroizing::new(random_secret());
  let nonce = Zeroizing::new(random_secret());
  let url = provider.authorization_url(&state, &verifier, &nonce).await?;
  let now = Instant::now();
  let mut pending = federation.pending()?;
  pending
    .flows
    .retain(|_, flow| now.duration_since(flow.started) < LIFETIME);
  if let Some(previous) = previous
    && pending.flows.get(&previous).is_some_and(|flow| flow.kind == kind)
  {
    pending.flows.remove(&previous);
  }
  if pending.flows.len() >= MAX_FLOWS {
    return Err(AccountError::slow_down());
  }
  pending.flows.insert(
    hash(&binder),
    Flow {
      kind,
      started: now,
      operation,
      approval,
      phase: Phase::Authorizing {
        state_hash: hash(&state),
        verifier,
        nonce,
      },
    },
  );
  Ok(
    (
      [(header::SET_COOKIE, flow_cookie(kind, &binder, LIFETIME.as_secs())?)],
      Json(json!({"authorization_url": url.as_str()})),
    )
      .into_response(),
  )
}

struct CallbackQuery {
  state: Zeroizing<String>,
  code: Option<Zeroizing<String>>,
  issuer: Option<String>,
}

fn callback_query(query: &str) -> Option<CallbackQuery> {
  if query.len() > 4096 {
    return None;
  }
  let (mut state, mut code, mut error, mut issuer) = (None, None, false, None);
  for (key, value) in form_urlencoded::parse(query.as_bytes()) {
    match key.as_ref() {
      "state" => {
        if !valid_secret(&value) || state.replace(Zeroizing::new(value.into_owned())).is_some() {
          return None;
        }
      }
      "code" => {
        if !(1..=1024).contains(&value.len())
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
      "iss" => {
        if !(1..=512).contains(&value.len()) || issuer.replace(value.into_owned()).is_some() {
          return None;
        }
      }
      _ => {}
    }
  }
  if code.is_some() == error {
    return None;
  }
  Some(CallbackQuery {
    state: state?,
    code,
    issuer,
  })
}

fn return_to_account(
  kind: Kind,
  success: bool,
  approval: Option<&DeviceApproval>,
  clear: bool,
) -> Result<Response, AccountError> {
  let mut query = form_urlencoded::Serializer::new(String::new());
  query.append_pair(kind.label(), if success { "complete" } else { "failed" });
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
    response
      .headers_mut()
      .insert(header::SET_COOKIE, flow_cookie(kind, "", 0)?);
  }
  Ok(response)
}

async fn callback(
  kind: Kind,
  State(accounts): State<Accounts>,
  headers: HeaderMap,
  RawQuery(query): RawQuery,
) -> Result<Response, AccountError> {
  let federation = &accounts.inner.federation;
  let Some(provider) = federation.provider(kind) else {
    return return_to_account(kind, false, None, false);
  };
  let (Ok(key), Some(query)) = (flow_key(kind, &headers), query.as_deref().and_then(callback_query)) else {
    return return_to_account(kind, false, None, false);
  };
  if !provider.response_issuer_matches(query.issuer.as_deref()) {
    return return_to_account(kind, false, None, false);
  }
  let (verifier, nonce) = {
    let mut pending = federation.pending()?;
    let Some(flow) = pending.live(kind, &key) else {
      return return_to_account(kind, false, None, false);
    };
    if !matches!(&flow.phase, Phase::Authorizing { state_hash, .. } if bool::from(state_hash.ct_eq(&hash(&query.state))))
    {
      return return_to_account(kind, false, None, false);
    }
    let Phase::Authorizing { verifier, nonce, .. } = std::mem::replace(&mut flow.phase, Phase::Exchanging) else {
      unreachable!("only authorizing flows can exchange their code");
    };
    (verifier, nonce)
  };
  let identity = match query.code {
    Some(code) => provider.exchange(&code, &verifier, &nonce).await,
    None => None,
  };
  let mut pending = federation.pending()?;
  let Some(flow) = pending.live(kind, &key) else {
    return return_to_account(kind, false, None, false);
  };
  if let Some(identity) = identity {
    flow.phase = Phase::Ready(identity);
    return_to_account(kind, true, flow.approval.as_ref(), false)
  } else {
    let removed = pending.flows.remove(&key).expect("flow still exists under lock");
    return_to_account(kind, false, removed.approval.as_ref(), true)
  }
}

async fn complete(kind: Kind, State(accounts): State<Accounts>, headers: HeaderMap) -> Result<Response, AccountError> {
  accounts.check_origin(&headers, true)?;
  require_browser(&headers)?;
  let key = flow_key(kind, &headers)?;
  accounts
    .blocking(move |accounts| {
      let federation = &accounts.inner.federation;
      federation.provider(kind).ok_or_else(|| kind.unavailable())?;
      let mut pending = federation.pending()?;
      let Some(flow) = pending.live(kind, &key) else {
        return Err(kind.expired());
      };
      if let Phase::Signup(identity) = &flow.phase {
        ensure_signed_out(&accounts, &headers)?;
        return Ok(Json(signup_prompt(identity)).into_response());
      }
      if !matches!(flow.phase, Phase::Ready(_)) {
        return Err(kind.expired());
      }
      let flow = pending.flows.remove(&key).expect("flow still exists under lock");
      let Phase::Ready(identity) = flow.phase else {
        unreachable!("verified ready")
      };
      let (outcome, cookie) = match flow.operation {
        Operation::SignIn => {
          ensure_signed_out(&accounts, &headers)?;
          match identity.sign_in(&accounts.inner.store)? {
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
          identity.link(&accounts.inner.store, &token)?;
          (Complete::Linked, None)
        }
      };
      finish_response(kind, outcome, cookie)
    })
    .await
}

fn signup_prompt(identity: &Identity) -> Complete {
  let suggested = identity.suggested_username().unwrap_or_default().to_ascii_lowercase();
  let suggested_username =
    if tokn_hub_client_core::address::parse_machine_address(&format!("{suggested}:machine")).is_ok() {
      suggested
    } else {
      String::new()
    };
  Complete::SignupRequired { suggested_username }
}

async fn signup(
  kind: Kind,
  State(accounts): State<Accounts>,
  headers: HeaderMap,
  Json(request): Json<Signup>,
) -> Result<Response, AccountError> {
  accounts.check_origin(&headers, true)?;
  require_browser(&headers)?;
  let key = flow_key(kind, &headers)?;
  accounts
    .blocking(move |accounts| {
      let federation = &accounts.inner.federation;
      federation.provider(kind).ok_or_else(|| kind.unavailable())?;
      let mut pending = federation.pending()?;
      let Some(flow) = pending.live(kind, &key) else {
        return Err(kind.expired());
      };
      let Phase::Signup(identity) = &flow.phase else {
        return Err(kind.expired());
      };
      ensure_signed_out(&accounts, &headers)?;
      let login = identity.sign_up(&accounts.inner.store, request.username.trim())?;
      let cookie = accounts.browser_login_cookie(&login)?;
      pending.flows.remove(&key);
      finish_response(kind, login.identity, Some(cookie))
    })
    .await
}

fn finish_response(
  kind: Kind,
  outcome: impl Serialize,
  account: Option<HeaderValue>,
) -> Result<Response, AccountError> {
  let mut response = Json(outcome).into_response();
  let headers = response.headers_mut();
  headers.insert(header::SET_COOKIE, flow_cookie(kind, "", 0)?);
  if let Some(account) = account {
    headers.append(header::SET_COOKIE, account);
  }
  Ok(response)
}
