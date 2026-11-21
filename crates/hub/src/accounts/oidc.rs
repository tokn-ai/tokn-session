//! Confidential Apell Account client. Signed provider identities never carry Hub privileges.

mod keys;
#[cfg(test)]
mod tests;

use std::{
  ffi::OsString,
  time::{Duration, Instant},
};

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, Validation};
use reqwest::{Client, RequestBuilder, header, redirect::Policy};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256, Sha384};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Semaphore};
use url::{Url, form_urlencoded};
use zeroize::Zeroizing;

use super::{
  AccountError,
  federation::{APELL_CALLBACK, code_challenge},
  mail::mailbox,
  storage::apell::Identity,
};

const ISSUER: &str = "TOKN_HUB_APELL_ISSUER";
const CLIENT_ID: &str = "TOKN_HUB_APELL_CLIENT_ID";
const CLIENT_SECRET: &str = "TOKN_HUB_APELL_CLIENT_SECRET";
const CACHE_LIFETIME: Duration = Duration::from_secs(300);
const MAX_RESPONSE_BYTES: usize = 128 * 1024;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_REQUESTS: usize = 8;

#[cfg(test)]
type FixtureExchange = fn(&str, &str) -> Result<Identity, ()>;

pub(super) struct Oidc {
  issuer: Url,
  discovery_url: Url,
  client_id: String,
  client_secret: Zeroizing<String>,
  callback: Url,
  http: Client,
  discovery: Mutex<Option<(Provider, Instant)>>,
  keys: Mutex<keys::Cache>,
  requests: Semaphore,
  #[cfg(test)]
  exchange_for_test: Option<FixtureExchange>,
}

#[derive(Deserialize)]
struct Discovery {
  issuer: String,
  authorization_endpoint: String,
  token_endpoint: String,
  jwks_uri: String,
  userinfo_endpoint: String,
  id_token_signing_alg_values_supported: Vec<String>,
  code_challenge_methods_supported: Vec<String>,
  response_types_supported: Vec<String>,
  token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Clone)]
struct Provider {
  authorization_endpoint: Url,
  token_endpoint: Url,
  jwks_uri: Url,
  userinfo_endpoint: Url,
  algorithms: Vec<Algorithm>,
}

#[derive(Deserialize)]
struct Tokens {
  token_type: String,
  access_token: String,
  id_token: String,
}

#[derive(Deserialize)]
struct IdClaims {
  sub: String,
  aud: Audience,
  exp: u64,
  iat: u64,
  nonce: String,
  azp: Option<String>,
  at_hash: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
  Single(String),
  Multiple(Vec<String>),
}

impl Audience {
  fn is_for(&self, client_id: &str) -> bool {
    match self {
      Self::Single(audience) => audience == client_id,
      Self::Multiple(audiences) => audiences.len() == 1 && audiences[0] == client_id,
    }
  }
}

#[derive(Deserialize)]
struct UserInfo {
  sub: String,
  preferred_username: Option<String>,
  username: Option<String>,
  email: Option<String>,
  #[serde(default)]
  email_verified: bool,
}

impl UserInfo {
  fn verified(self, issuer: &Url, subject: String) -> Result<Identity, AccountError> {
    if self.sub != subject {
      return Err(invalid_identity());
    }
    let valid_login =
      |value: &String| !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control);
    Ok(Identity {
      issuer: issuer.to_string(),
      subject,
      login: self
        .preferred_username
        .filter(valid_login)
        .or_else(|| self.username.filter(valid_login)),
      verified_email: self
        .email
        .filter(|_| self.email_verified)
        .and_then(|email| mailbox(&email)),
    })
  }
}

impl Oidc {
  pub(super) fn from_env(public_url: &str) -> Result<Option<Self>, String> {
    let (issuer, client_id, secret) = match (
      std::env::var_os(ISSUER),
      std::env::var_os(CLIENT_ID),
      std::env::var_os(CLIENT_SECRET),
    ) {
      (None, None, None) => return Ok(None),
      (Some(issuer), Some(id), Some(secret)) => (
        setting(issuer, ISSUER, 2048)?,
        setting(id, CLIENT_ID, 512)?,
        Zeroizing::new(setting(secret, CLIENT_SECRET, 512)?),
      ),
      _ => {
        return Err(format!(
          "Configure {ISSUER}, {CLIENT_ID}, and {CLIENT_SECRET} to enable Apell Account sign-in"
        ));
      }
    };
    Self::new(&issuer, public_url, client_id, secret).map(Some)
  }

  fn new(issuer: &str, public_url: &str, client_id: String, client_secret: Zeroizing<String>) -> Result<Self, String> {
    let issuer = parse_https(issuer)
      .filter(|url| url.as_str() == issuer && url.path() != "/" && !url.path().ends_with('/'))
      .ok_or("Apell Account issuer must be the exact HTTPS issuer URL, including its path and no trailing slash")?;
    let mut callback = parse_https(public_url)
      .filter(|url| url.path() == "/")
      .ok_or("Apell Account sign-in requires a public HTTPS Hub origin")?;
    callback.set_path(APELL_CALLBACK);
    let mut discovery_url = issuer.clone();
    discovery_url
      .path_segments_mut()
      .map_err(|_| "Invalid Apell Account issuer URL")?
      .extend([".well-known", "openid-configuration"]);
    let http = Client::builder()
      .https_only(true)
      .no_proxy()
      .redirect(Policy::none())
      .retry(reqwest::retry::never())
      .connect_timeout(Duration::from_secs(5))
      .timeout(Duration::from_secs(8))
      .user_agent("Tokn-Hub")
      .build()
      .map_err(|_| "Could not initialize Apell Account sign-in client")?;
    Ok(Self {
      issuer,
      discovery_url,
      client_id,
      client_secret,
      callback,
      http,
      discovery: Mutex::default(),
      keys: Mutex::default(),
      requests: Semaphore::new(MAX_REQUESTS),
      #[cfg(test)]
      exchange_for_test: None,
    })
  }

  #[cfg(test)]
  pub(super) fn for_test(issuer: &str, provider: FixtureExchange) -> Self {
    let mut oidc = Self::new(
      issuer,
      "https://hub.example.test",
      "fixture-client".into(),
      Zeroizing::new("fixture-secret".into()),
    )
    .unwrap();
    let endpoint = |path: &str| Url::parse(&format!("{}{path}", oidc.issuer)).unwrap();
    oidc.discovery = Mutex::new(Some((
      Provider {
        authorization_endpoint: endpoint("/auth"),
        token_endpoint: endpoint("/token"),
        jwks_uri: endpoint("/jwks"),
        userinfo_endpoint: endpoint("/me"),
        algorithms: vec![Algorithm::ES384],
      },
      Instant::now(),
    )));
    oidc.exchange_for_test = Some(provider);
    oidc
  }

  pub(super) fn issuer(&self) -> &str {
    self.issuer.as_str()
  }

  pub(super) async fn authorization_url(&self, state: &str, verifier: &str, nonce: &str) -> Result<Url, AccountError> {
    let _permit = self.requests.try_acquire().map_err(|_| AccountError::slow_down())?;
    let mut url = self.provider().await?.authorization_endpoint;
    url.query_pairs_mut().extend_pairs([
      ("client_id", self.client_id.as_str()),
      ("redirect_uri", self.callback.as_str()),
      ("response_type", "code"),
      ("response_mode", "query"),
      ("scope", "openid profile email"),
      ("state", state),
      ("nonce", nonce),
      ("code_challenge", &code_challenge(verifier)),
      ("code_challenge_method", "S256"),
    ]);
    Ok(url)
  }

  pub(super) async fn exchange(&self, code: &str, verifier: &str, nonce: &str) -> Result<Identity, AccountError> {
    if !visible_ascii(code, 1024) {
      return Err(invalid_identity());
    }
    #[cfg(test)]
    if let Some(exchange) = self.exchange_for_test {
      return exchange(code, nonce).map_err(|_| invalid_identity());
    }
    let _permit = self.requests.try_acquire().map_err(|_| AccountError::slow_down())?;
    let provider = self.provider().await?;
    let form = form_urlencoded::Serializer::new(String::new())
      .extend_pairs([
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", self.callback.as_str()),
      ])
      .finish();
    let tokens: Tokens = read_json(
      self
        .http
        .post(provider.token_endpoint.clone())
        .basic_auth(
          form_encode(&self.client_id),
          Some(Zeroizing::new(form_encode(&self.client_secret)).as_str()),
        )
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form),
    )
    .await?;
    let access_token = Zeroizing::new(tokens.access_token);
    let id_token = Zeroizing::new(tokens.id_token);
    if !tokens.token_type.eq_ignore_ascii_case("bearer")
      || !visible_ascii(&access_token, MAX_TOKEN_BYTES)
      || !visible_ascii(&id_token, MAX_TOKEN_BYTES)
    {
      return Err(invalid_identity());
    }
    let subject = self.verify_identity(&provider, &id_token, &access_token, nonce).await?;
    let user: UserInfo = read_json(
      self
        .http
        .get(provider.userinfo_endpoint)
        .bearer_auth(access_token.as_str()),
    )
    .await?;
    user.verified(&self.issuer, subject)
  }

  async fn provider(&self) -> Result<Provider, AccountError> {
    let mut cache = self.discovery.lock().await;
    if let Some((provider, _)) = cache.as_ref().filter(|(_, time)| time.elapsed() < CACHE_LIFETIME) {
      return Ok(provider.clone());
    }
    let document = read_json(self.http.get(self.discovery_url.clone())).await?;
    let provider = self.verify_discovery(document)?;
    *cache = Some((provider.clone(), Instant::now()));
    Ok(provider)
  }

  fn verify_discovery(&self, document: Discovery) -> Result<Provider, AccountError> {
    let includes = |values: &[String], name: &str| values.iter().any(|value| value == name);
    if document.issuer != self.issuer.as_str()
      || !includes(&document.response_types_supported, "code")
      || !includes(&document.code_challenge_methods_supported, "S256")
      || !includes(&document.token_endpoint_auth_methods_supported, "client_secret_basic")
    {
      return Err(unavailable());
    }
    let algorithms = [
      ("ES384", Algorithm::ES384),
      ("ES256", Algorithm::ES256),
      ("RS256", Algorithm::RS256),
    ]
    .into_iter()
    .filter_map(|(name, algorithm)| {
      includes(&document.id_token_signing_alg_values_supported, name).then_some(algorithm)
    })
    .collect::<Vec<_>>();
    if algorithms.is_empty() {
      return Err(unavailable());
    }
    let prefix = format!("{}/", self.issuer.path());
    let endpoint = |value: &str| {
      parse_https(value)
        .filter(|url| url.origin() == self.issuer.origin() && url.path().starts_with(&prefix))
        .ok_or_else(unavailable)
    };
    Ok(Provider {
      authorization_endpoint: endpoint(&document.authorization_endpoint)?,
      token_endpoint: endpoint(&document.token_endpoint)?,
      jwks_uri: endpoint(&document.jwks_uri)?,
      userinfo_endpoint: endpoint(&document.userinfo_endpoint)?,
      algorithms,
    })
  }

  async fn verify_identity(
    &self,
    provider: &Provider,
    id_token: &str,
    access_token: &str,
    nonce: &str,
  ) -> Result<String, AccountError> {
    let header = jsonwebtoken::decode_header(id_token).map_err(|_| invalid_identity())?;
    if !provider.algorithms.contains(&header.alg)
      || header.jwk.is_some()
      || header.jku.is_some()
      || header.x5u.is_some()
      || header.x5c.is_some()
      || header.crit.is_some()
      || header.enc.is_some()
      || header.zip.is_some()
      || header
        .typ
        .as_deref()
        .is_some_and(|typ| !typ.eq_ignore_ascii_case("jwt"))
    {
      return Err(invalid_identity());
    }
    let kid = header
      .kid
      .as_deref()
      .filter(|kid| visible_ascii(kid, 128))
      .ok_or_else(invalid_identity)?;
    let key = self
      .keys
      .lock()
      .await
      .get(&self.http, &provider.jwks_uri, kid, header.alg)
      .await?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[self.issuer.as_str()]);
    validation.set_audience(&[&self.client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.validate_nbf = true;
    validation.leeway = 0;
    let claims = jsonwebtoken::decode::<IdClaims>(id_token, &key, &validation)
      .map_err(|_| invalid_identity())?
      .claims;
    let now = crate::secure_client::unix_time().map_err(|_| unavailable())?;
    if !visible_ascii(&claims.sub, 255)
      || !claims.aud.is_for(&self.client_id)
      || claims.azp.as_ref().is_some_and(|party| party != &self.client_id)
      || claims.exp <= now
      || claims.iat == 0
      || claims.iat > now.saturating_add(60)
      || claims.iat >= claims.exp
      || !constant_time_equal(&claims.nonce, nonce)
      || claims
        .at_hash
        .as_ref()
        .is_some_and(|hash| !constant_time_equal(hash, &access_token_hash(header.alg, access_token)))
    {
      return Err(invalid_identity());
    }
    Ok(claims.sub)
  }
}

fn setting(value: OsString, name: &str, max: usize) -> Result<String, String> {
  value
    .into_string()
    .ok()
    .filter(|value| visible_ascii(value, max))
    .ok_or_else(|| format!("{name} must contain a single non-empty value"))
}

fn parse_https(value: &str) -> Option<Url> {
  if !visible_ascii(value, 2048) || value.contains(['?', '#', '\\']) {
    return None;
  }
  let authority = value.strip_prefix("https://")?.split('/').next()?;
  if authority.is_empty() || authority.contains('@') {
    return None;
  }
  let url = Url::parse(value).ok()?;
  let canonical = url.as_str() == value || (url.path() == "/" && url.as_str().strip_suffix('/') == Some(value));
  (url.scheme() == "https" && url.domain().is_some() && canonical).then_some(url)
}

fn form_encode(value: &str) -> String {
  form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn visible_ascii(value: &str, max: usize) -> bool {
  !value.is_empty() && value.len() <= max && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn constant_time_equal(first: &str, second: &str) -> bool {
  bool::from(first.as_bytes().ct_eq(second.as_bytes()))
}

fn access_token_hash(algorithm: Algorithm, token: &str) -> String {
  if algorithm == Algorithm::ES384 {
    URL_SAFE_NO_PAD.encode(&Sha384::digest(token.as_bytes())[..24])
  } else {
    URL_SAFE_NO_PAD.encode(&Sha256::digest(token.as_bytes())[..16])
  }
}

async fn read_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, AccountError> {
  let mut response = request
    .header(header::ACCEPT, "application/json")
    .send()
    .await
    .map_err(|_| unavailable())?;
  let is_json = response
    .headers()
    .get(header::CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| {
      value
        .split(';')
        .next()
        .is_some_and(|kind| matches!(kind.trim(), "application/json" | "application/jwk-set+json"))
    });
  if response.status() != StatusCode::OK
    || !is_json
    || response
      .content_length()
      .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
  {
    return Err(unavailable());
  }
  let mut bytes = Zeroizing::new(Vec::new());
  while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
    if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
      return Err(unavailable());
    }
    bytes.extend_from_slice(&chunk);
  }
  serde_json::from_slice(&bytes).map_err(|_| unavailable())
}

fn unavailable() -> AccountError {
  AccountError::new(
    StatusCode::BAD_GATEWAY,
    "Apell Account sign-in is temporarily unavailable. Try again.",
  )
}

fn invalid_identity() -> AccountError {
  AccountError::new(
    StatusCode::UNAUTHORIZED,
    "Apell Account sign-in could not be verified. Try again.",
  )
}
