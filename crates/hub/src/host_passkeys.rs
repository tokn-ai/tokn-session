//! Host-owned passkey enrollment and new-device authorization. The Hub only
//! carries Noise records; Hub administration credentials never enter this store.
use crate::{
  onboarding,
  secure::{HostAuthOperation, decode_public_key},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use url::Url;
use webauthn_rs::prelude::*;

pub(crate) const CEREMONY_SECONDS: u64 = 300;

pub(crate) struct HostPasskeys {
  state_file: PathBuf,
  host_id: String,
  name: String,
  origin: String,
  webauthn: Webauthn,
}

pub(crate) struct Pending {
  host_id: String,
  device_public_key: String,
  channel_binding: String,
  started_at: u64,
  expires_at: u64,
  ceremony: Ceremony,
}

enum Ceremony {
  Registration(PasskeyRegistration),
  Authentication(PasskeyAuthentication),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finish<T> {
  credential: T,
}

/// RP IDs are stable DNS names. Numeric Hub addresses can still carry OTP and
/// remembered-device traffic, but require an explicit browser origin for passkeys.
pub fn validate_origin(value: &str) -> Result<Url, String> {
  let origin = Url::parse(value).map_err(|_| "Invalid host passkey origin")?;
  let domain = origin
    .domain()
    .ok_or("Host passkeys require a stable browser hostname")?;
  if !origin.username().is_empty()
    || origin.password().is_some()
    || origin.query().is_some()
    || origin.fragment().is_some()
    || origin.path() != "/"
    || !(origin.scheme() == "https" || (origin.scheme() == "http" && domain == "localhost"))
  {
    return Err("Host passkey origin must be HTTPS, or HTTP localhost for development".into());
  }
  Ok(origin)
}

impl HostPasskeys {
  pub(crate) fn new(state_file: PathBuf, host_id: &str, name: &str, origin: &str) -> Result<Self, String> {
    onboarding::validate_uuid(host_id)?;
    let origin_url = validate_origin(origin)?;
    let origin = origin_url.origin().ascii_serialization();
    let webauthn = WebauthnBuilder::new(origin_url.domain().expect("validated domain"), &origin_url)
      .map_err(|error| format!("Invalid host passkey RP: {error}"))?
      .rp_name("Tokn hosts")
      .timeout(Duration::from_secs(CEREMONY_SECONDS))
      .build()
      .map_err(|error| format!("Could not configure host passkeys: {error}"))?;
    onboarding::host_passkeys(&state_file, &origin)?;
    Ok(Self {
      state_file,
      host_id: host_id.into(),
      name: name.into(),
      origin,
      webauthn,
    })
  }

  pub(crate) fn start(
    &self,
    operation: HostAuthOperation,
    payload: Value,
    device_public_key: &str,
    channel_binding: &str,
    now: u64,
  ) -> Result<(Pending, Value), String> {
    let _: Start = serde_json::from_value(payload).map_err(|_| "Passkey start requires an empty object")?;
    decode_public_key(device_public_key)?;
    if channel_binding.is_empty() {
      return Err("Passkey authentication requires an authenticated Noise channel".into());
    }
    let passkeys = onboarding::host_passkeys(&self.state_file, &self.origin)?;
    let (ceremony, options) = match operation {
      HostAuthOperation::RegisterStart => {
        if !onboarding::is_authorized(&self.state_file, device_public_key)? {
          return Err("Pair this device with an authenticator before adding a host passkey".into());
        }
        let excluded = passkeys.iter().map(|passkey| passkey.cred_id().clone()).collect();
        let (options, state) = self
          .webauthn
          .start_passkey_registration(
            Uuid::parse_str(&self.host_id).map_err(|_| "Invalid passkey host identity")?,
            &self.host_id,
            &self.name,
            Some(excluded),
          )
          .map_err(|_| "Could not start host passkey enrollment")?;
        (Ceremony::Registration(state), serde_json::to_value(options))
      }
      HostAuthOperation::LoginStart => {
        if passkeys.is_empty() {
          return Err("This host has no passkey; connect using its authenticator code first".into());
        }
        let (options, state) = self
          .webauthn
          .start_passkey_authentication(&passkeys)
          .map_err(|_| "Could not start host passkey login")?;
        (Ceremony::Authentication(state), serde_json::to_value(options))
      }
      _ => return Err("Start a new passkey ceremony before finishing it".into()),
    };
    onboarding::begin_passkey(&self.state_file, now)?;
    // The generated random WebAuthn challenge is a host-owned nonce associated
    // only with this machine, authenticated peer and Noise handshake transcript.
    // No client-supplied ceremony state or cross-channel completion is accepted.
    Ok((
      Pending {
        host_id: self.host_id.clone(),
        device_public_key: device_public_key.into(),
        channel_binding: channel_binding.into(),
        started_at: now,
        expires_at: now.saturating_add(CEREMONY_SECONDS),
        ceremony,
      },
      json!({"options": options.map_err(|_| "Could not encode host passkey options")?}),
    ))
  }

  /// Consuming Pending makes each random challenge single-use, including on
  /// failure. The caller must keep it exclusively on its original Noise channel.
  pub(crate) fn finish(
    &self,
    pending: Pending,
    operation: HostAuthOperation,
    payload: Value,
    device_public_key: &str,
    channel_binding: &str,
    now: u64,
  ) -> Result<Value, String> {
    if pending.host_id != self.host_id
      || pending.device_public_key != device_public_key
      || pending.channel_binding != channel_binding
    {
      return Err("Host passkey ceremony belongs to another device or channel".into());
    }
    if now < pending.started_at || now >= pending.expires_at {
      return Err("Host passkey ceremony expired; start again".into());
    }
    let registered = match (operation, pending.ceremony) {
      (HostAuthOperation::RegisterFinish, Ceremony::Registration(state)) => {
        let request: Finish<RegisterPublicKeyCredential> =
          serde_json::from_value(payload).map_err(|_| "Invalid host passkey registration response")?;
        let passkey = self
          .webauthn
          .finish_passkey_registration(&request.credential, &state)
          .map_err(|_| "Host passkey registration could not be verified")?;
        onboarding::register_host_passkey(&self.state_file, &self.origin, device_public_key, passkey)?;
        true
      }
      (HostAuthOperation::LoginFinish, Ceremony::Authentication(state)) => {
        let request: Finish<PublicKeyCredential> =
          serde_json::from_value(payload).map_err(|_| "Invalid host passkey authentication response")?;
        let result = self
          .webauthn
          .finish_passkey_authentication(&request.credential, &state)
          .map_err(|_| "Host passkey login could not be verified")?;
        onboarding::authorize_passkey_device(&self.state_file, &self.origin, device_public_key, &result, now)?;
        false
      }
      _ => return Err("Incorrect host passkey ceremony operation".into()),
    };
    Ok(json!({"authorized": true, "registered": registered, "device_public_key": device_public_key}))
  }
}

#[cfg(test)]
mod tests;
