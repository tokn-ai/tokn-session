//! Hub account device approval shared by native and portable clients.
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::Zeroize;

pub const CLI_START: &str = "/hub/v1/account/device/start";
pub const VIEWER_START: &str = "/hub/v1/account/device/viewer/start";
pub const DEVICE_TOKEN: &str = "/hub/v1/account/device/token";
const MAX_TOKEN_BYTES: usize = 1024;

/// The account permission set associated with a browser-approved challenge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DevicePurpose {
  Cli,
  Viewer,
}

/// Only the public approval fields cross into the viewer.
#[derive(Clone, Serialize)]
pub struct LoginChallenge {
  pub verification_url: String,
  pub user_code: String,
  pub expires_at: u64,
}

#[derive(Deserialize, Serialize)]
pub struct DeviceAuthorization {
  pub device_code: String,
  pub user_code: String,
  pub expires_in: u64,
  pub interval: u64,
}

impl DeviceAuthorization {
  pub fn challenge(&self, hub: &Url, now: u64, purpose: DevicePurpose) -> Result<LoginChallenge, String> {
    if !(1..=900).contains(&self.expires_in)
      || !(1..=60).contains(&self.interval)
      || self.interval >= self.expires_in
      || !(1..=32).contains(&self.user_code.len())
      || !self
        .user_code
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
      return Err("Hub returned an invalid device sign-in challenge".into());
    }
    validate_token(&self.device_code)?;
    let mut verification = hub.clone();
    verification.set_path("/account");
    verification.set_fragment(None);
    verification.set_query(None);
    verification.query_pairs_mut().append_pair("user_code", &self.user_code);
    if purpose == DevicePurpose::Viewer {
      verification.query_pairs_mut().append_pair("device", "viewer");
    }
    Ok(LoginChallenge {
      verification_url: verification.into(),
      user_code: self.user_code.clone(),
      expires_at: now.checked_add(self.expires_in).ok_or("Invalid sign-in expiry")?,
    })
  }
}

impl Drop for DeviceAuthorization {
  fn drop(&mut self) {
    self.device_code.zeroize();
  }
}

pub fn validate_token(value: &str) -> Result<(), String> {
  if !(1..=MAX_TOKEN_BYTES).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
    return Err("Invalid Hub account credential".into());
  }
  Ok(())
}
