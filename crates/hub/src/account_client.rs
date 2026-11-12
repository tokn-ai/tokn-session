//! Native account credentials and cancellable browser approval shared by CLI and app.
use reqwest::{Client, Response, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
  collections::BTreeMap,
  path::{Path, PathBuf},
  time::Duration,
};
use tokio::{task::spawn_blocking, time::Instant};
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::address::parse_machine_address;
use tokn_session_storage::{read_private_json, update_private_json};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

const MAX_TOKEN_BYTES: usize = 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_STORE_BYTES: u64 = 64 * 1024;
const CREDENTIAL_FILE: &str = "account-credentials.json";
const CANCELED: &str = "Hub sign-in canceled";

#[derive(Clone)]
pub struct AccountClient {
  path: PathBuf,
  http: Client,
}

/// Safe progress for presentation. The device credential remains inside Rust.
#[derive(Clone, Serialize)]
pub struct LoginChallenge {
  pub verification_url: String,
  pub user_code: String,
  pub expires_at: u64,
}

/// Native-only result. Account credentials are never serialized to the viewer.
pub struct SignedInAccount {
  pub user_id: uuid::Uuid,
  pub username: String,
  pub token: Zeroizing<String>,
  pub warning: Option<String>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CredentialKind {
  DeviceLogin,
  ApiKey,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Credential {
  kind: CredentialKind,
  token: String,
}

impl Drop for Credential {
  fn drop(&mut self) {
    self.token.zeroize();
  }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedAccounts {
  selected: Option<String>,
  credentials: BTreeMap<String, Credential>,
}

struct CredentialUpdate {
  previous: Option<Credential>,
  previous_selected: Option<String>,
}

#[derive(Deserialize)]
struct AccountIdentity {
  user_id: uuid::Uuid,
  username: String,
}

impl AccountIdentity {
  fn validate(&self) -> Result<(), String> {
    if self.user_id.is_nil() || parse_machine_address(&format!("{}:machine", self.username)).is_err() {
      return Err("Hub returned an invalid account identity".into());
    }
    Ok(())
  }

  fn signed_in(self, credential: &Credential) -> SignedInAccount {
    SignedInAccount {
      user_id: self.user_id,
      username: self.username,
      token: Zeroizing::new(credential.token.clone()),
      warning: None,
    }
  }
}

#[derive(Deserialize)]
struct DeviceAuthorization {
  device_code: String,
  user_code: String,
  expires_in: u64,
  interval: u64,
}

impl Drop for DeviceAuthorization {
  fn drop(&mut self) {
    self.device_code.zeroize();
  }
}

#[derive(Deserialize)]
struct IssuedCredential {
  #[serde(flatten)]
  account: AccountIdentity,
  token: String,
}

impl AccountClient {
  pub fn new(state_dir: PathBuf) -> Result<Self, String> {
    let http = Client::builder()
      .no_proxy()
      .redirect(reqwest::redirect::Policy::none())
      .retry(reqwest::retry::never())
      .connect_timeout(Duration::from_secs(5))
      .timeout(Duration::from_secs(10))
      .build()
      .map_err(|_| "Could not create a Hub account connection")?;
    Ok(Self {
      path: state_dir.join(CREDENTIAL_FILE),
      http,
    })
  }

  /// Reuse a valid credential. Only absence or an explicit 401 starts sign-in.
  pub async fn ensure_signed_in<F>(
    &self,
    hub: &Url,
    cancel: &CancellationToken,
    on_challenge: F,
  ) -> Result<SignedInAccount, String>
  where
    F: Fn(LoginChallenge) + Send + Sync,
  {
    let hub = tokn_hub_remote::canonical_hub(hub.as_str())?;
    check_canceled(cancel)?;
    let credential = self.saved_credential(&hub).await?;
    if let Some(credential) = credential {
      let account = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(CANCELED.into()),
        result = self.current_account(&hub, &credential.token) => result?,
      };
      if let Some(account) = account {
        check_canceled(cancel)?;
        return Ok(account.signed_in(&credential));
      }
    }
    self.sign_in(&hub, cancel, on_challenge).await
  }

  /// Explicitly request a fresh native credential through browser approval.
  pub async fn sign_in<F>(
    &self,
    hub: &Url,
    cancel: &CancellationToken,
    on_challenge: F,
  ) -> Result<SignedInAccount, String>
  where
    F: Fn(LoginChallenge) + Send + Sync,
  {
    let hub = tokn_hub_remote::canonical_hub(hub.as_str())?;
    check_canceled(cancel)?;
    let (account, credential) = self.device_login(&hub, cancel, on_challenge).await?;
    self.persist_login(&hub, account, credential, cancel).await
  }

  pub async fn login_with_token(
    &self,
    hub: &Url,
    token: Zeroizing<String>,
    cancel: &CancellationToken,
  ) -> Result<SignedInAccount, String> {
    let hub = tokn_hub_remote::canonical_hub(hub.as_str())?;
    check_canceled(cancel)?;
    let account = tokio::select! {
      biased;
      _ = cancel.cancelled() => return Err(CANCELED.into()),
      result = self.current_account(&hub, &token) => result?,
    }
    .ok_or_else(|| account_error(StatusCode::UNAUTHORIZED))?;
    self
      .persist_login(
        &hub,
        account,
        Credential {
          kind: CredentialKind::ApiKey,
          token: token.to_string(),
        },
        cancel,
      )
      .await
  }

  pub async fn status(&self, hub: Option<&Url>) -> Result<(Url, SignedInAccount), String> {
    let (hub, credential) = self.select_account(hub).await?;
    let account = self
      .current_account(&hub, &credential.token)
      .await?
      .ok_or_else(|| account_error(StatusCode::UNAUTHORIZED))?;
    Ok((hub, account.signed_in(&credential)))
  }

  pub async fn logout(&self, hub: Option<&Url>) -> Result<Url, String> {
    let (hub, credential) = self.select_account(hub).await?;
    if matches!(credential.kind, CredentialKind::DeviceLogin) {
      self.revoke_device_login(&hub, &credential.token).await?;
    }
    let origin = hub.to_string();
    let path = self.path.clone();
    let removed = spawn_blocking(move || forget_credential(&path, &origin, &credential))
      .await
      .map_err(|_| "Could not remove the Hub account credential")??;
    if !removed {
      return Err("The saved login changed during logout. Check auth status and retry if needed.".into());
    }
    Ok(hub)
  }

  async fn saved_credential(&self, hub: &Url) -> Result<Option<Credential>, String> {
    let origin = hub.to_string();
    let path = self.path.clone();
    spawn_blocking(move || {
      let mut accounts = read_credentials(&path)?;
      let credential = accounts.credentials.remove(&origin);
      if let Some(credential) = &credential {
        validate_token(&credential.token)?;
      }
      Ok(credential)
    })
    .await
    .map_err(|_| "Could not load the Hub account credential")?
  }

  async fn select_account(&self, hub: Option<&Url>) -> Result<(Url, Credential), String> {
    let path = self.path.clone();
    let hub = hub.cloned();
    spawn_blocking(move || {
      let mut accounts = read_credentials(&path)?;
      let hub = hub
        .as_ref()
        .map(Url::as_str)
        .or(accounts.selected.as_deref())
        .ok_or("No saved Hub account. Run auth login --hub <URL>.")?;
      let hub = tokn_hub_remote::canonical_hub(hub)?;
      let credential = accounts
        .credentials
        .remove(hub.as_str())
        .ok_or("No saved account for this Hub. Run auth login.")?;
      validate_token(&credential.token)?;
      Ok((hub, credential))
    })
    .await
    .map_err(|_| "Could not load the Hub account credential")?
  }

  async fn current_account(&self, hub: &Url, token: &str) -> Result<Option<AccountIdentity>, String> {
    validate_token(token)?;
    let response = self
      .http
      .get(endpoint(hub, "/hub/v1/account/me"))
      .bearer_auth(token)
      .header(header::ACCEPT, "application/json")
      .send()
      .await
      .map_err(|_| "Could not reach the Hub account service")?;
    if response.status() == StatusCode::UNAUTHORIZED {
      return Ok(None);
    }
    let account: AccountIdentity = read_json(response).await?;
    account.validate()?;
    Ok(Some(account))
  }

  async fn revoke_device_login(&self, hub: &Url, token: &str) -> Result<(), String> {
    let response = self
      .http
      .post(endpoint(hub, "/hub/v1/account/logout"))
      .bearer_auth(token)
      .header(header::CONTENT_TYPE, "application/json")
      .body("{}")
      .send()
      .await
      .map_err(|_| "Could not revoke native login through the Hub")?;
    if !response.status().is_success() && response.status() != StatusCode::UNAUTHORIZED {
      return Err(account_error(response.status()));
    }
    Ok(())
  }

  async fn persist_login(
    &self,
    hub: &Url,
    account: AccountIdentity,
    credential: Credential,
    cancel: &CancellationToken,
  ) -> Result<SignedInAccount, String> {
    let fresh_device = matches!(credential.kind, CredentialKind::DeviceLogin);
    let mut signed_in = account.signed_in(&credential);
    let path = self.path.clone();
    let origin = hub.to_string();
    let cancellation = cancel.clone();
    let outcome = spawn_blocking(move || save_credential(&path, origin, credential, Some(&cancellation)))
      .await
      .map_err(|_| "Could not save the Hub account credential")?;
    let update = match outcome {
      Ok(update) => update,
      Err(error) => {
        // The new approval must not remain usable after failed/canceled storage.
        if fresh_device {
          let _ = self.revoke_device_login(hub, &signed_in.token).await;
        }
        return Err(error);
      }
    };
    if cancel.is_cancelled() {
      let path = self.path.clone();
      let origin = hub.to_string();
      let token = Zeroizing::new(signed_in.token.to_string());
      let _ = spawn_blocking(move || restore_credential(&path, &origin, &token, update)).await;
      if fresh_device {
        let _ = self.revoke_device_login(hub, &signed_in.token).await;
      }
      return Err(CANCELED.into());
    }
    if let Some(previous) = update.previous
      && previous.token != signed_in.token.as_str()
      && matches!(previous.kind, CredentialKind::DeviceLogin)
      && self.revoke_device_login(hub, &previous.token).await.is_err()
    {
      signed_in.warning =
        Some("Could not revoke the previous native login. Its credential will expire on the Hub.".into());
    }
    Ok(signed_in)
  }

  async fn device_login<F>(
    &self,
    hub: &Url,
    cancel: &CancellationToken,
    on_challenge: F,
  ) -> Result<(AccountIdentity, Credential), String>
  where
    F: Fn(LoginChallenge) + Send + Sync,
  {
    let response = tokio::select! {
      biased;
      _ = cancel.cancelled() => return Err(CANCELED.into()),
      response = self.http.post(endpoint(hub, "/hub/v1/account/device/start"))
        .header(header::CONTENT_TYPE, "application/json").body("{}").send() =>
        response.map_err(|_| "Could not start sign-in through the Hub")?,
    };
    let authorization: DeviceAuthorization = tokio::select! {
      biased;
      _ = cancel.cancelled() => return Err(CANCELED.into()),
      result = read_json(response) => result?,
    };
    if !(1..=900).contains(&authorization.expires_in)
      || !(1..=60).contains(&authorization.interval)
      || authorization.interval >= authorization.expires_in
      || !(1..=32).contains(&authorization.user_code.len())
      || !authorization
        .user_code
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
      return Err("Hub returned an invalid device sign-in challenge".into());
    }
    validate_token(&authorization.device_code)?;
    let deadline = Instant::now() + Duration::from_secs(authorization.expires_in);
    let expires_at = crate::secure_client::unix_time()?
      .checked_add(authorization.expires_in)
      .ok_or("Invalid sign-in expiry")?;
    let mut verification = endpoint(hub, "/account");
    verification
      .query_pairs_mut()
      .append_pair("user_code", &authorization.user_code);
    on_challenge(LoginChallenge {
      verification_url: verification.to_string(),
      user_code: authorization.user_code.clone(),
      expires_at,
    });
    let mut interval = Duration::from_secs(authorization.interval);
    let issued = tokio::select! {
      biased;
      _ = cancel.cancelled() => return Err(CANCELED.into()),
      issued = tokio::time::timeout_at(deadline, async {
        loop {
          tokio::time::sleep(interval).await;
          let response = self.http
            .post(endpoint(hub, "/hub/v1/account/device/token"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(json!({ "device_code": authorization.device_code }).to_string())
            .send().await;
          match response {
            Ok(response) if response.status() == StatusCode::ACCEPTED => {}
            Ok(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
              interval = (interval + Duration::from_secs(5)).min(Duration::from_secs(60));
            }
            Ok(response) => break read_json::<IssuedCredential>(response).await,
            Err(error) if error.is_timeout() => interval = (interval * 2).min(Duration::from_secs(60)),
            Err(_) => break Err("Could not complete sign-in through the Hub".into()),
          }
        }
      }) => issued.map_err(|_| "Hub sign-in expired. Try again.")??,
    };
    let credential = Credential {
      kind: CredentialKind::DeviceLogin,
      token: issued.token,
    };
    issued.account.validate()?;
    validate_token(&credential.token)?;
    Ok((issued.account, credential))
  }
}

fn check_canceled(cancel: &CancellationToken) -> Result<(), String> {
  if cancel.is_cancelled() {
    Err(CANCELED.into())
  } else {
    Ok(())
  }
}

fn read_credentials(path: &Path) -> Result<SavedAccounts, String> {
  Ok(read_private_json(path, MAX_STORE_BYTES)?.unwrap_or_default())
}

fn save_credential(
  path: &Path,
  origin: String,
  credential: Credential,
  cancel: Option<&CancellationToken>,
) -> Result<CredentialUpdate, String> {
  let mut update = CredentialUpdate {
    previous: None,
    previous_selected: None,
  };
  update_private_json(path, MAX_STORE_BYTES, |previous: Option<SavedAccounts>| {
    if let Some(cancel) = cancel {
      check_canceled(cancel)?;
    }
    let mut accounts = previous.unwrap_or_default();
    update.previous_selected = accounts.selected.replace(origin.clone());
    update.previous = accounts.credentials.insert(origin, credential);
    Ok(accounts)
  })?;
  Ok(update)
}

fn restore_credential(path: &Path, origin: &str, token: &str, update: CredentialUpdate) -> Result<(), String> {
  update_private_json(path, MAX_STORE_BYTES, |previous_state: Option<SavedAccounts>| {
    let mut accounts = previous_state.unwrap_or_default();
    if accounts
      .credentials
      .get(origin)
      .is_some_and(|current| current.token == token)
    {
      match update.previous {
        Some(previous) => {
          accounts.credentials.insert(origin.into(), previous);
        }
        None => {
          accounts.credentials.remove(origin);
        }
      }
      if accounts.selected.as_deref() == Some(origin) {
        accounts.selected = update
          .previous_selected
          .filter(|hub| accounts.credentials.contains_key(hub));
      }
    }
    Ok(accounts)
  })
  .map(|_| ())
}

fn forget_credential(path: &Path, origin: &str, previous: &Credential) -> Result<bool, String> {
  let mut removed = false;
  update_private_json(path, MAX_STORE_BYTES, |previous_state: Option<SavedAccounts>| {
    let mut accounts = previous_state.unwrap_or_default();
    if accounts
      .credentials
      .get(origin)
      .is_some_and(|current| current.token == previous.token)
    {
      accounts.credentials.remove(origin);
      if accounts.selected.as_deref() == Some(origin) {
        accounts.selected = accounts.credentials.keys().next().cloned();
      }
      removed = true;
    }
    Ok(accounts)
  })?;
  Ok(removed)
}

fn endpoint(hub: &Url, path: &str) -> Url {
  let mut endpoint = hub.clone();
  endpoint.set_path(path);
  endpoint
}

fn account_error(status: StatusCode) -> String {
  match status {
    StatusCode::UNAUTHORIZED => "The Hub account credential is invalid or expired. Sign in again.".into(),
    StatusCode::NOT_FOUND | StatusCode::SERVICE_UNAVAILABLE => "Account sign-in is unavailable on this Hub.".into(),
    StatusCode::TOO_MANY_REQUESTS => "Too many Hub account requests. Wait and try again.".into(),
    _ => format!("The Hub account request failed (HTTP {}).", status.as_u16()),
  }
}

async fn read_json<T: DeserializeOwned>(mut response: Response) -> Result<T, String> {
  if response.status() != StatusCode::OK {
    return Err(account_error(response.status()));
  }
  let json_type = response
    .headers()
    .get(header::CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .is_some_and(|value| {
      value
        .split(';')
        .next()
        .is_some_and(|mime| mime.trim() == "application/json")
    });
  if !json_type {
    return Err("Hub returned an unsupported account response type".into());
  }
  if response
    .content_length()
    .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
  {
    return Err("Hub account response exceeds its size limit".into());
  }
  let mut bytes = Zeroizing::new(Vec::new());
  while let Some(chunk) = response
    .chunk()
    .await
    .map_err(|_| "Could not read the Hub account response")?
  {
    if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
      return Err("Hub account response exceeds its size limit".into());
    }
    bytes.extend_from_slice(&chunk);
  }
  serde_json::from_slice(&bytes).map_err(|_| "Hub returned an invalid account response".into())
}

pub fn validate_token(value: &str) -> Result<(), String> {
  if !(1..=MAX_TOKEN_BYTES).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
    return Err("Invalid Hub account credential".into());
  }
  Ok(())
}

#[cfg(test)]
mod tests;
