//! Interactive and headless credentials for Hub account operations.
use clap::Subcommand;
use reqwest::{Client, Response, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
  collections::BTreeMap,
  io::{IsTerminal, Read},
  path::{Path, PathBuf},
  process::{Command as ProcessCommand, Stdio},
  time::Duration,
};
use tokio::{task::spawn_blocking, time::Instant};
use tokn_hub_client_core::address::parse_machine_address;
use tokn_session_hub::storage;
use tokn_session_storage::{read_private_json, update_private_json};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

const MAX_TOKEN_BYTES: usize = 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_STORE_BYTES: u64 = 64 * 1024;
const CREDENTIAL_FILE: &str = "account-credentials.json";

#[derive(Subcommand)]
pub(crate) enum Command {
  /// Sign in through the Hub website or supply a scoped API key.
  Login {
    /// Hub HTTPS origin. Loopback HTTP is available for development.
    #[arg(long)]
    hub: Url,
    /// Read an API key from standard input.
    #[arg(long)]
    with_token: bool,
    /// Print the approval URL for use on another device.
    #[arg(long, conflicts_with = "with_token")]
    no_browser: bool,
  },
  /// Verify the saved account credential with the Hub.
  Status {
    /// Hub origin. Defaults to the most recently selected Hub.
    #[arg(long)]
    hub: Option<Url>,
  },
  /// Remove the saved credential and revoke a browser-issued CLI login.
  Logout {
    /// Hub origin. Defaults to the most recently selected Hub.
    #[arg(long)]
    hub: Option<Url>,
  },
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

pub(crate) async fn run(command: Command) -> Result<(), String> {
  let path = storage::default_state_dir()?.join(CREDENTIAL_FILE);
  match command {
    Command::Login {
      hub,
      with_token,
      no_browser,
    } => {
      let hub = tokn_hub_remote::canonical_hub(hub.as_str())?;
      let client = account_client()?;
      let (account, credential) = if with_token {
        let token = spawn_blocking(read_token)
          .await
          .map_err(|_| "Could not read API key")??;
        let account = current_account(&client, &hub, &token).await?;
        (
          account,
          Credential {
            kind: CredentialKind::ApiKey,
            token: token.to_string(),
          },
        )
      } else {
        device_login(&client, &hub, no_browser).await?
      };
      let origin = hub.to_string();
      let replaced = spawn_blocking(move || save_credential(&path, origin, credential))
        .await
        .map_err(|_| "Could not save the Hub account credential")??;
      if let Some(previous) = replaced
        && matches!(previous.kind, CredentialKind::DeviceLogin)
        && revoke_device_login(&client, &hub, &previous.token).await.is_err()
      {
        eprintln!("Could not revoke the previous CLI login. Its credential will expire on the Hub.");
      }
      println!("Signed in to {hub} as {}", account.username);
      Ok(())
    }
    Command::Status { hub } => {
      let (hub, credential) = select_account(path, hub).await?;
      let account = current_account(&account_client()?, &hub, &credential.token).await?;
      println!("Signed in to {hub} as {}", account.username);
      Ok(())
    }
    Command::Logout { hub } => {
      let (hub, credential) = select_account(path.clone(), hub).await?;
      if matches!(credential.kind, CredentialKind::DeviceLogin) {
        revoke_device_login(&account_client()?, &hub, &credential.token).await?;
      }
      let origin = hub.to_string();
      let removed = spawn_blocking(move || forget_credential(&path, &origin, &credential))
        .await
        .map_err(|_| "Could not remove the Hub account credential")??;
      if !removed {
        return Err("The saved login changed during logout. Check auth status and retry if needed.".into());
      }
      println!("Signed out of {hub}");
      Ok(())
    }
  }
}

pub(crate) async fn saved_token(hub: &Url) -> Result<Option<Zeroizing<String>>, String> {
  let origin = tokn_hub_remote::canonical_hub(hub.as_str())?.to_string();
  spawn_blocking(move || {
    let path = storage::default_state_dir()?.join(CREDENTIAL_FILE);
    let accounts = read_credentials(&path)?;
    accounts
      .credentials
      .get(&origin)
      .map(|entry| checked_token(&entry.token))
      .transpose()
  })
  .await
  .map_err(|_| "Could not load the Hub account credential")?
}

fn read_credentials(path: &Path) -> Result<SavedAccounts, String> {
  Ok(read_private_json(path, MAX_STORE_BYTES)?.unwrap_or_default())
}

fn save_credential(path: &Path, origin: String, credential: Credential) -> Result<Option<Credential>, String> {
  let mut replaced = None;
  update_private_json(path, MAX_STORE_BYTES, |previous: Option<SavedAccounts>| {
    let mut accounts = previous.unwrap_or_default();
    accounts.selected = Some(origin.clone());
    if let Some(previous) = accounts.credentials.remove(&origin)
      && previous.token != credential.token
    {
      replaced = Some(previous);
    }
    accounts.credentials.insert(origin, credential);
    Ok(accounts)
  })?;
  Ok(replaced)
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

async fn select_account(path: PathBuf, hub: Option<Url>) -> Result<(Url, Credential), String> {
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

fn account_client() -> Result<Client, String> {
  Client::builder()
    .no_proxy()
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .connect_timeout(Duration::from_secs(5))
    .timeout(Duration::from_secs(10))
    .build()
    .map_err(|_| "Could not create a Hub account connection".into())
}

fn endpoint(hub: &Url, path: &str) -> Url {
  let mut endpoint = hub.clone();
  endpoint.set_path(path);
  endpoint
}

fn account_error(status: StatusCode) -> String {
  match status {
    StatusCode::UNAUTHORIZED => "The Hub account credential is invalid or expired. Run auth login.".into(),
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

fn validate_token(value: &str) -> Result<(), String> {
  if !(1..=MAX_TOKEN_BYTES).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
    return Err("Invalid Hub account credential".into());
  }
  Ok(())
}

fn checked_token(value: &str) -> Result<Zeroizing<String>, String> {
  validate_token(value)?;
  Ok(Zeroizing::new(value.to_owned()))
}

fn read_token() -> Result<Zeroizing<String>, String> {
  let stdin = std::io::stdin();
  if stdin.is_terminal() {
    return Err("Pipe the API key to standard input with --with-token.".into());
  }
  let mut input = Zeroizing::new(String::new());
  stdin
    .lock()
    .take(MAX_TOKEN_BYTES as u64 + 3)
    .read_to_string(&mut input)
    .map_err(|_| "Could not read API key from standard input")?;
  if input.len() > MAX_TOKEN_BYTES + 2 {
    return Err("Hub API key exceeds its size limit".into());
  }
  checked_token(input.trim_end_matches(['\r', '\n']))
}

async fn current_account(client: &Client, hub: &Url, token: &str) -> Result<AccountIdentity, String> {
  validate_token(token)?;
  let response = client
    .get(endpoint(hub, "/hub/v1/account/me"))
    .bearer_auth(token)
    .header(header::ACCEPT, "application/json")
    .send()
    .await
    .map_err(|_| "Could not reach the Hub account service")?;
  let account: AccountIdentity = read_json(response).await?;
  account.validate()?;
  Ok(account)
}

async fn revoke_device_login(client: &Client, hub: &Url, token: &str) -> Result<(), String> {
  let response = client
    .post(endpoint(hub, "/hub/v1/account/logout"))
    .bearer_auth(token)
    .header(header::CONTENT_TYPE, "application/json")
    .body("{}")
    .send()
    .await
    .map_err(|_| "Could not revoke CLI login through the Hub")?;
  if !response.status().is_success() && response.status() != StatusCode::UNAUTHORIZED {
    return Err(account_error(response.status()));
  }
  Ok(())
}

async fn device_login(client: &Client, hub: &Url, no_browser: bool) -> Result<(AccountIdentity, Credential), String> {
  let response = client
    .post(endpoint(hub, "/hub/v1/account/device/start"))
    .header(header::CONTENT_TYPE, "application/json")
    .body("{}")
    .send()
    .await
    .map_err(|_| "Could not start sign-in through the Hub")?;
  let authorization: DeviceAuthorization = read_json(response).await?;
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
  let mut interval = Duration::from_secs(authorization.interval);
  let verification = endpoint(hub, "/account");
  eprintln!("Open {verification} and confirm CLI code {}.", authorization.user_code);
  if !no_browser {
    let mut browser_url = verification;
    browser_url
      .query_pairs_mut()
      .append_pair("user_code", &authorization.user_code);
    open_browser(&browser_url);
  }

  let issued = tokio::time::timeout_at(deadline, async {
    loop {
      tokio::time::sleep(interval).await;
      let body = json!({ "device_code": authorization.device_code }).to_string();
      let response = client
        .post(endpoint(hub, "/hub/v1/account/device/token"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;
      match response {
        Ok(response) if response.status() == StatusCode::ACCEPTED => {}
        Ok(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
          interval += Duration::from_secs(5);
        }
        Ok(response) => break read_json::<IssuedCredential>(response).await,
        Err(error) if error.is_timeout() => interval *= 2,
        Err(_) => break Err("Could not complete sign-in through the Hub".into()),
      }
    }
  })
  .await
  .map_err(|_| "Hub CLI sign-in expired. Run auth login again.")??;
  let credential = Credential {
    kind: CredentialKind::DeviceLogin,
    token: issued.token,
  };
  issued.account.validate()?;
  validate_token(&credential.token)?;
  Ok((issued.account, credential))
}

fn open_browser(url: &Url) {
  #[cfg(target_os = "macos")]
  let mut opener = ProcessCommand::new("/usr/bin/open");
  #[cfg(target_os = "windows")]
  let mut opener = {
    let mut command = ProcessCommand::new("rundll32.exe");
    command.arg("url.dll,FileProtocolHandler");
    command
  };
  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  let mut opener = ProcessCommand::new("xdg-open");
  let opened = opener
    .arg(url.as_str())
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn();
  if let Ok(mut process) = opened {
    std::thread::spawn(move || {
      let _ = process.wait();
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn credential(token: &str) -> Credential {
    Credential {
      kind: CredentialKind::DeviceLogin,
      token: token.into(),
    }
  }

  #[test]
  fn a_stale_logout_cannot_remove_a_newer_credential_or_another_hub() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(CREDENTIAL_FILE);
    let first = "https://first.example/";
    let second = "https://second.example/";
    save_credential(&path, first.into(), credential("old")).unwrap();
    save_credential(&path, second.into(), credential("other")).unwrap();
    let replaced = save_credential(&path, first.into(), credential("new"))
      .unwrap()
      .unwrap();
    assert_eq!(replaced.token, "old");

    assert!(!forget_credential(&path, first, &credential("old")).unwrap());
    assert!(forget_credential(&path, first, &credential("new")).unwrap());
    let accounts = read_credentials(&path).unwrap();
    assert!(!accounts.credentials.contains_key(first));
    assert_eq!(accounts.selected.as_deref(), Some(second));
    assert_eq!(accounts.credentials.get(second).unwrap().token, "other");
  }
}
