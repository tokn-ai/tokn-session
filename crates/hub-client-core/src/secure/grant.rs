use super::identity::{decode_noise_public_key, decode_public_key};
use crate::protocol::{decode, encode};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const GRANT_VERSION: u8 = 1;
pub const MAX_GRANT_BYTES: usize = 32 * 1024;
const MAX_SESSION_KEYS: usize = 128;
const MAX_SESSION_KEY_BYTES: usize = 8192;
const GRANT_DOMAIN: &str = "tokn-hub-owner-grant-v1";

/// Selected sessions are exact keys, never implicit projects or descendants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GrantScope {
  All {},
  Sessions { session_keys: Vec<String> },
}

/// An owner's explicit authorization, bound to a recipient key rather than a
/// bearer secret. Session keys can reveal local paths; treat invitations as
/// sensitive metadata and transport grants only inside the encrypted channel.
/// The owner verification key is deliberately absent: each endpoint must be
/// provisioned with its own trusted key independently of the Hub/invitation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
  pub version: u8,
  pub grant_id: String,
  pub host_id: String,
  pub host_public_key: String,
  pub recipient_public_key: String,
  pub scope: GrantScope,
  pub allow_control: bool,
  pub expires_at: u64,
}

impl Grant {
  pub fn validate(&self) -> Result<(), String> {
    if self.version != GRANT_VERSION {
      return Err("Unsupported sharing grant version".into());
    }
    validate_id(&self.grant_id, "Grant ID")?;
    validate_id(&self.host_id, "Host ID")?;
    decode_noise_public_key(&self.host_public_key)?;
    decode_noise_public_key(&self.recipient_public_key)?;
    if self.expires_at == 0 {
      return Err("A sharing grant requires an explicit expiry".into());
    }
    if let GrantScope::Sessions { session_keys } = &self.scope {
      if self.allow_control {
        return Err("Selected-session grants cannot allow agent control".into());
      }
      if session_keys.is_empty() || session_keys.len() > MAX_SESSION_KEYS {
        return Err(format!(
          "A selected-session grant requires 1–{MAX_SESSION_KEYS} session keys"
        ));
      }
      let mut seen = HashSet::new();
      for key in session_keys {
        if key.is_empty() || key.len() > MAX_SESSION_KEY_BYTES || key.chars().any(char::is_control) {
          return Err(format!(
            "Session keys must contain 1–{MAX_SESSION_KEY_BYTES} bytes without control characters"
          ));
        }
        if !seen.insert(key) {
          return Err("Selected-session grant contains a duplicate session key".into());
        }
      }
    }
    if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_GRANT_BYTES - 128 {
      return Err("Sharing grant is too large".into());
    }
    Ok(())
  }

  /// Typed JSON fixes field order and types before signing, independent of the
  /// invitation's JSON whitespace or object-member order. Reject unknown fields
  /// while decoding so no unsigned semantics can be appended to a grant.
  pub(super) fn signing_bytes(&self) -> Result<Vec<u8>, String> {
    self.validate()?;
    serde_json::to_vec(&(GRANT_DOMAIN, self)).map_err(|e| e.to_string())
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedGrant {
  pub grant: Grant,
  pub signature: String,
}

impl SignedGrant {
  pub fn validate(&self) -> Result<(), String> {
    self.grant.validate()?;
    if self.signature.len() != 86 {
      return Err("Invalid sharing grant signature length".into());
    }
    let bytes = decode(&self.signature, 64)?;
    if bytes.len() != 64 || encode(&bytes) != self.signature {
      return Err("Invalid sharing grant signature encoding".into());
    }
    if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_GRANT_BYTES {
      return Err("Signed sharing grant is too large".into());
    }
    Ok(())
  }

  /// Parse a bounded invitation; parsing alone does not establish trust.
  pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
    if bytes.len() > MAX_GRANT_BYTES {
      return Err("Signed sharing grant is too large".into());
    }
    let signed: Self = serde_json::from_slice(bytes).map_err(|e| format!("Invalid sharing grant: {e}"))?;
    signed.validate()?;
    Ok(signed)
  }

  /// Verify every binding against endpoint-owned facts. `owner_public_key`
  /// comes from local configuration, never from the invitation or Hub. The
  /// recipient is the authenticated Noise peer on a host, and the local Noise
  /// identity on a client. Recheck expiry/revocation while streams remain open.
  pub fn verify(
    &self,
    owner_public_key: &str,
    host_id: &str,
    host_public_key: &str,
    recipient_public_key: &str,
    now: u64,
    revoked: &HashSet<String>,
  ) -> Result<(), String> {
    self.validate()?;
    let owner =
      VerifyingKey::from_bytes(&decode_public_key(owner_public_key)?).map_err(|_| "Invalid owner verification key")?;
    let signature =
      Signature::from_slice(&decode(&self.signature, 64)?).map_err(|_| "Invalid sharing grant signature")?;
    owner
      .verify_strict(&self.grant.signing_bytes()?, &signature)
      .map_err(|_| "Sharing grant signature is not from the trusted owner")?;
    if self.grant.host_id != host_id || self.grant.host_public_key != host_public_key {
      return Err("Sharing grant belongs to a different host".into());
    }
    if self.grant.recipient_public_key != recipient_public_key {
      return Err("Sharing grant belongs to a different recipient".into());
    }
    if self.grant.expires_at <= now {
      return Err("Sharing grant has expired".into());
    }
    if revoked.contains(&self.grant.grant_id) {
      return Err("Sharing grant has been revoked".into());
    }
    Ok(())
  }
}

fn validate_id(value: &str, label: &str) -> Result<(), String> {
  if value.is_empty()
    || value.len() > 128
    || !value
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
  {
    return Err(format!(
      "{label} must contain 1–128 ASCII letters, digits, underscores, or hyphens"
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::secure::{NoiseIdentity, OwnerIdentity};

  fn fixture() -> (OwnerIdentity, SignedGrant) {
    let owner = OwnerIdentity::generate();
    let signed = owner
      .sign_grant(Grant {
        version: GRANT_VERSION,
        grant_id: "grant_1".into(),
        host_id: "host_1".into(),
        host_public_key: NoiseIdentity::generate().unwrap().public_key(),
        recipient_public_key: NoiseIdentity::generate().unwrap().public_key(),
        scope: GrantScope::Sessions {
          session_keys: vec!["session_a".into()],
        },
        allow_control: false,
        expires_at: 1000,
      })
      .unwrap();
    (owner, signed)
  }

  fn verify(signed: &SignedGrant, owner: &OwnerIdentity) -> Result<(), String> {
    signed.verify(
      &owner.public_key(),
      "host_1",
      &signed.grant.host_public_key,
      &signed.grant.recipient_public_key,
      999,
      &HashSet::new(),
    )
  }

  #[test]
  fn signatures_authenticate_every_grant_field_and_the_independent_owner() {
    let (owner, signed) = fixture();
    verify(&signed, &owner).unwrap();
    assert!(verify(&signed, &OwnerIdentity::generate()).is_err());
    let mut changes = vec![];
    let mut changed = signed.clone();
    changed.grant.grant_id.push('x');
    changes.push(changed);
    let mut changed = signed.clone();
    changed.grant.host_id.push('x');
    changes.push(changed);
    let mut changed = signed.clone();
    changed.grant.host_public_key = NoiseIdentity::generate().unwrap().public_key();
    changes.push(changed);
    let mut changed = signed.clone();
    changed.grant.recipient_public_key = NoiseIdentity::generate().unwrap().public_key();
    changes.push(changed);
    let mut changed = signed.clone();
    changed.grant.scope = GrantScope::All {};
    changes.push(changed);
    let mut changed = signed.clone();
    changed.grant.allow_control = true;
    changes.push(changed);
    let mut changed = signed.clone();
    changed.grant.expires_at += 1;
    changes.push(changed);
    for changed in changes {
      assert!(verify(&changed, &owner).is_err());
    }
  }

  #[test]
  fn valid_signature_does_not_bypass_host_recipient_expiry_or_revocation() {
    let (owner, signed) = fixture();
    let host = &signed.grant.host_public_key;
    let recipient = &signed.grant.recipient_public_key;
    let wrong = NoiseIdentity::generate().unwrap().public_key();
    let empty = HashSet::new();
    let verify_at =
      |id, host, recipient, now, revoked| signed.verify(&owner.public_key(), id, host, recipient, now, revoked);
    assert!(verify_at("other_host", host, recipient, 999, &empty).is_err());
    assert!(verify_at("host_1", &wrong, recipient, 999, &empty).is_err());
    assert!(verify_at("host_1", host, &wrong, 999, &empty).is_err());
    assert!(verify_at("host_1", host, recipient, 1000, &empty).is_err());
    let revoked = HashSet::from([signed.grant.grant_id.clone()]);
    assert!(verify_at("host_1", host, recipient, 999, &revoked).is_err());
  }

  #[test]
  fn invitation_json_is_typed_bounded_and_canonicalized() {
    let (owner, signed) = fixture();
    let mut json = serde_json::to_value(&signed).unwrap();
    let reordered = serde_json::to_vec_pretty(&json).unwrap();
    verify(&SignedGrant::from_json(&reordered).unwrap(), &owner).unwrap();
    json["grant"]["extra_permission"] = true.into();
    assert!(SignedGrant::from_json(&serde_json::to_vec(&json).unwrap()).is_err());
    assert!(SignedGrant::from_json(&vec![b' '; MAX_GRANT_BYTES + 1]).is_err());
    assert!(serde_json::from_str::<GrantScope>(r#"{"type":"all","extra_permission":true}"#).is_err());
    let mut duplicate = signed.grant.clone();
    duplicate.scope = GrantScope::Sessions {
      session_keys: vec!["a".into(), "a".into()],
    };
    assert!(owner.sign_grant(duplicate).is_err());
    let mut oversized = signed.grant;
    oversized.scope = GrantScope::Sessions {
      session_keys: (0..128).map(|index| format!("{index}{}", "a".repeat(1024))).collect(),
    };
    assert!(owner.sign_grant(oversized).is_err());
  }
}
