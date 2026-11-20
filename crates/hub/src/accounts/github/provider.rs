use std::{ffi::OsString, time::Duration};

use reqwest::{Client, RequestBuilder, redirect::Policy};
use serde::{Deserialize, de::DeserializeOwned};
use url::{Url, form_urlencoded::Serializer};
use zeroize::Zeroizing;

use super::{CALLBACK, Identity, code_challenge};
use crate::accounts::mail;

const CLIENT_ID: &str = "TOKN_HUB_GITHUB_CLIENT_ID";
const CLIENT_SECRET: &str = "TOKN_HUB_GITHUB_CLIENT_SECRET";
const AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const USER_URL: &str = "https://api.github.com/user";
const EMAILS_URL: &str = "https://api.github.com/user/emails?per_page=100";
const EMAIL_SCOPE: &str = "user:email";
const MAX_RESPONSE: usize = 64 * 1024;

pub(super) struct Provider {
  client_id: String,
  client_secret: Zeroizing<String>,
  callback: Url,
  http: Client,
  #[cfg(test)]
  pub(super) exchange_for_test: Option<fn(code: &str) -> Result<Identity, ()>>,
}

#[derive(Deserialize)]
struct Token {
  access_token: String,
  token_type: String,
  scope: String,
}

#[derive(Deserialize)]
struct User {
  id: u64,
  login: String,
}

#[derive(Deserialize)]
struct Email {
  email: String,
  primary: bool,
  verified: bool,
}

impl Provider {
  pub(super) fn from_env(public_url: &str) -> Result<Option<Self>, String> {
    let (client_id, client_secret) = match (std::env::var_os(CLIENT_ID), std::env::var_os(CLIENT_SECRET)) {
      (None, None) => return Ok(None),
      (Some(id), Some(secret)) => (setting(id, CLIENT_ID)?, Zeroizing::new(setting(secret, CLIENT_SECRET)?)),
      _ => {
        return Err(format!(
          "Configure both {CLIENT_ID} and {CLIENT_SECRET} to enable GitHub sign-in"
        ));
      }
    };
    Self::new(public_url, client_id, client_secret).map(Some)
  }

  pub(super) fn new(public_url: &str, client_id: String, client_secret: Zeroizing<String>) -> Result<Self, String> {
    let mut callback = Url::parse(public_url).map_err(|_| "GitHub sign-in requires a public HTTPS Hub origin")?;
    if callback.scheme() != "https"
      || callback.domain().is_none()
      || !callback.username().is_empty()
      || callback.password().is_some()
      || callback.path() != "/"
      || callback.query().is_some()
      || callback.fragment().is_some()
    {
      return Err("GitHub sign-in requires a public HTTPS Hub origin".into());
    }
    callback.set_path(CALLBACK);
    let http = Client::builder()
      .https_only(true)
      .redirect(Policy::none())
      .no_proxy()
      .timeout(Duration::from_secs(10))
      .user_agent("Tokn-Hub")
      .build()
      .map_err(|_| "Could not initialize GitHub sign-in HTTP client")?;
    Ok(Self {
      client_id,
      client_secret,
      callback,
      http,
      #[cfg(test)]
      exchange_for_test: None,
    })
  }

  pub(super) fn authorization_url(&self, state: &str, verifier: &str) -> Url {
    let mut url = Url::parse(AUTHORIZE_URL).expect("GitHub authorize constant is valid");
    url
      .query_pairs_mut()
      .append_pair("client_id", &self.client_id)
      .append_pair("redirect_uri", self.callback.as_str())
      .append_pair("scope", EMAIL_SCOPE)
      .append_pair("state", state)
      .append_pair("code_challenge", &code_challenge(verifier))
      .append_pair("code_challenge_method", "S256")
      .append_pair("prompt", "select_account")
      .append_pair("allow_signup", "false");
    url
  }

  pub(super) async fn exchange(&self, code: &str, verifier: &str) -> Result<Identity, ()> {
    #[cfg(test)]
    if let Some(exchange) = self.exchange_for_test {
      return exchange(code);
    }

    let form = Serializer::new(String::new())
      .append_pair("client_id", &self.client_id)
      .append_pair("client_secret", &self.client_secret)
      .append_pair("code", code)
      .append_pair("redirect_uri", self.callback.as_str())
      .append_pair("code_verifier", verifier)
      .finish();
    let token: Token = json(
      self
        .http
        .post(TOKEN_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form),
    )
    .await?;

    let access_token = Zeroizing::new(token.access_token);
    if !token.token_type.eq_ignore_ascii_case("bearer")
      || !matches!(
        token.scope.split([',', ' ']).filter(|scope| !scope.is_empty()).next(),
        Some(EMAIL_SCOPE)
      )
      || token
        .scope
        .split([',', ' '])
        .any(|scope| !scope.is_empty() && scope != EMAIL_SCOPE)
      || !(1..=512).contains(&access_token.len())
      || !access_token.bytes().all(|byte| byte.is_ascii_graphic())
    {
      return Err(());
    }
    let request = |url| {
      self
        .http
        .get(url)
        .bearer_auth(access_token.as_str())
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
    };
    let (user, emails) = tokio::try_join!(json::<User>(request(USER_URL)), json::<Vec<Email>>(request(EMAILS_URL)),)?;
    if user.id == 0
      || user.login.is_empty()
      || user.login.len() > 100
      || !user
        .login
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
      return Err(());
    }
    let verified_email = emails
      .iter()
      .find(|email| email.primary && email.verified)
      .and_then(|email| mail::mailbox(&email.email));
    Ok(Identity {
      id: user.id,
      login: user.login,
      verified_email,
    })
  }
}

fn setting(value: OsString, name: &str) -> Result<String, String> {
  value
    .into_string()
    .ok()
    .filter(|value| (1..=512).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_graphic()))
    .ok_or_else(|| format!("{name} must contain a single non-empty GitHub application credential"))
}

async fn json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, ()> {
  let mut response = request.send().await.map_err(|_| ())?;
  if !response.status().is_success()
    || response
      .content_length()
      .is_some_and(|length| length > MAX_RESPONSE as u64)
  {
    return Err(());
  }
  let mut bytes = Zeroizing::new(Vec::new());
  while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
    if bytes.len() + chunk.len() > MAX_RESPONSE {
      return Err(());
    }
    bytes.extend_from_slice(&chunk);
  }
  serde_json::from_slice(&bytes).map_err(|_| ())
}
