//! Bind an existing connector key to an account name, then keep using that key.
use crate::{
  connector::{ConnectorConfig, load_key},
  onboarding::{canonical_hub, read_optional, save_configuration, validate_uuid},
  protocol,
  store::AccountMachineClaim,
};
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
  path::{Path, PathBuf},
  time::Duration,
};
use tokn_hub_client_core::{address::parse_machine_address, discovery::DiscoveryGroup};
use url::Url;
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaimRequest {
  pub host_id: String,
  pub public_key: String,
  pub machine_name: String,
  pub name: String,
  pub allow_control: bool,
  signature: String,
}

impl ClaimRequest {
  fn proof(&self, hub: &str, user_id: &str) -> Vec<u8> {
    serde_json::to_vec(&(
      "tokn-hub-account-host-v1",
      hub,
      user_id,
      &self.host_id,
      &self.public_key,
      &self.machine_name,
      &self.name,
      self.allow_control,
    ))
    .expect("serialize host claim")
  }

  pub(crate) fn verify(&self, hub: &str, user_id: &str) -> Result<(), String> {
    let key = VerifyingKey::from_bytes(&crate::secure::decode_public_key(&self.public_key)?)
      .map_err(|_| "Invalid machine enrollment key")?;
    let signature =
      Signature::from_slice(&protocol::decode(&self.signature, 64)?).map_err(|_| "Invalid machine claim signature")?;
    key
      .verify_strict(&self.proof(hub, user_id), &signature)
      .map_err(|_| "Machine claim did not prove ownership of its enrollment key".into())
  }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostAccount {
  hub_origin: String,
  claim: AccountMachineClaim,
}

pub(crate) fn membership_group(config: &ConnectorConfig) -> Result<Option<DiscoveryGroup>, String> {
  let Some(paired) = &config.paired else { return Ok(None) };
  let Some(path) = paired.state_file.parent() else {
    return Ok(None);
  };
  Ok(read_membership(path, config.hub_url.as_str(), &paired.host_id)?.map(|claim| claim.group))
}

/// Show the Hub-assigned name for a local host's saved identity.
pub fn saved_address(state_dir: &Path, hub_url: &str, host_id: &str) -> Result<Option<String>, String> {
  Ok(read_membership(state_dir, hub_url, host_id)?.map(|claim| claim.machine_address))
}

fn read_membership(state_dir: &Path, hub_url: &str, host_id: &str) -> Result<Option<AccountMachineClaim>, String> {
  let Some(saved) = read_optional::<HostAccount>(&state_dir.join("host-account.json"))? else {
    return Ok(None);
  };
  let hub = Url::parse(hub_url).map_err(|_| "Invalid saved Hub URL")?;
  if saved.hub_origin != canonical_hub(&hub)? {
    return Ok(None);
  }
  validate_claim(&saved.claim, host_id)?;
  Ok(Some(saved.claim))
}

fn membership_path(config: &ConnectorConfig) -> Option<PathBuf> {
  Some(config.paired.as_ref()?.state_file.parent()?.join("host-account.json"))
}

fn validate_claim(claim: &AccountMachineClaim, host_id: &str) -> Result<(), String> {
  let (username, _) = parse_machine_address(&claim.machine_address)?;
  claim.group.validate()?;
  if claim.host_id != host_id || username != claim.group.namespace {
    return Err("Hub returned a different machine assignment".into());
  }
  Ok(())
}

#[derive(Deserialize)]
struct Account {
  user_id: String,
  username: String,
}

/// Claim a machine with the account credential only during setup. The local
/// receipt carries the discovery capability used by future paired clients.
pub async fn claim(config: &ConnectorConfig, machine_name: &str, token: &str) -> Result<String, String> {
  config.validate()?;
  parse_machine_address(&format!("account:{machine_name}"))?;
  let paired = config
    .paired
    .as_ref()
    .ok_or("Account registration needs paired-device hosting")?;
  let path = membership_path(config).ok_or("Could not resolve machine account storage")?;
  let hub_origin = canonical_hub(&config.hub_url)?;
  let hub = Url::parse(&hub_origin).map_err(|_| "Invalid Hub URL")?;
  let client = reqwest::Client::builder()
    .no_proxy()
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .connect_timeout(Duration::from_secs(5))
    .timeout(Duration::from_secs(12))
    .build()
    .map_err(|_| "Could not prepare Hub account request")?;
  let account: Account = bounded_json(
    client
      .get(hub.join("/hub/v1/account/me").map_err(|_| "Invalid Hub URL")?)
      .bearer_auth(token)
      .send()
      .await
      .map_err(|_| "Could not contact the Hub account service")?,
  )
  .await?;
  validate_uuid(&account.user_id)?;
  let address = format!("{}:{machine_name}", account.username);
  parse_machine_address(&address)?;
  let key_file = config.key_file.clone();
  let key = tokio::task::spawn_blocking(move || load_key(&key_file))
    .await
    .map_err(|_| "Could not load machine enrollment key")??;
  let mut request = ClaimRequest {
    host_id: paired.host_id.clone(),
    public_key: protocol::encode(key.verifying_key().as_bytes()),
    machine_name: machine_name.into(),
    name: config.name.clone(),
    allow_control: config.allow_control,
    signature: String::new(),
  };
  request.signature = protocol::encode(&key.sign(&request.proof(&hub_origin, &account.user_id)).to_bytes());
  let claim: AccountMachineClaim = bounded_json(
    client
      .post(
        hub
          .join("/hub/v1/account/machines/claim")
          .map_err(|_| "Invalid Hub URL")?,
      )
      .bearer_auth(token)
      .header(reqwest::header::CONTENT_TYPE, "application/json")
      .body(serde_json::to_vec(&request).map_err(|_| "Could not encode machine claim")?)
      .send()
      .await
      .map_err(|_| "Could not submit the machine claim")?,
  )
  .await?;
  validate_claim(&claim, &paired.host_id)?;
  if claim.machine_address != address {
    return Err("Hub returned a different machine address".into());
  }
  tokio::task::spawn_blocking(move || save_membership(&path, hub_origin, claim))
    .await
    .map_err(|_| "Could not save machine account assignment")??;
  Ok(address)
}

fn save_membership(path: &Path, hub_origin: String, claim: AccountMachineClaim) -> Result<(), String> {
  save_configuration(path, &HostAccount { hub_origin, claim })
}

async fn bounded_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, String> {
  let status = response.status();
  let mut bytes = Zeroizing::new(Vec::new());
  while let Some(chunk) = response
    .chunk()
    .await
    .map_err(|_| "Could not read Hub account response")?
  {
    if bytes.len() + chunk.len() > 4096 {
      return Err("Hub account response is too large".into());
    }
    bytes.extend_from_slice(&chunk);
  }
  if !status.is_success() {
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let message = value
      .get("error")
      .and_then(serde_json::Value::as_str)
      .filter(|message| message.len() <= 256 && !message.chars().any(char::is_control));
    return Err(
      message
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Hub account request failed ({status})")),
    );
  }
  serde_json::from_slice(&bytes).map_err(|_| "Hub returned an invalid account response".into())
}
