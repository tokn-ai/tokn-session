//! Canonical human-readable routing addresses. Resolving an address never
//! authenticates a Noise key or authorizes access to host content.
use serde::{Deserialize, Serialize};

/// Parse an exact `username:machine_name` address. UI callers may trim input
/// before calling; persisted addresses and protocol values are canonical.
pub fn parse_machine_address(value: &str) -> Result<(String, String), String> {
  let (username, machine_name) = value
    .split_once(':')
    .ok_or("Enter a machine address as username:host")?;
  if !valid_slug(username) || !valid_slug(machine_name) {
    return Err(
      "Machine addresses require lowercase letters, digits, and internal hyphens, with 1–63 characters per name".into(),
    );
  }
  Ok((username.into(), machine_name.into()))
}

fn valid_slug(value: &str) -> bool {
  let alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
  (1..=63).contains(&value.len())
    && value.bytes().all(|byte| alphanumeric(byte) || byte == b'-')
    && value.as_bytes().first().copied().is_some_and(alphanumeric)
    && value.as_bytes().last().copied().is_some_and(alphanumeric)
}

pub fn validate_machine_name(value: &str) -> Result<(), String> {
  if value.trim().is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
    return Err("Machine name must contain 1–128 bytes and no control characters".into());
  }
  Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedMachine {
  pub host_id: String,
  pub machine_address: String,
  pub name: String,
  pub online: bool,
}

impl ResolvedMachine {
  /// Check the directory response against the exact address requested. It
  /// supplies a routing UUID and display metadata, never a host encryption pin.
  pub fn validate_for(&self, machine_address: &str) -> Result<(), String> {
    parse_machine_address(machine_address)?;
    parse_machine_address(&self.machine_address)?;
    if self.machine_address != machine_address {
      return Err("Hub resolved a different machine address".into());
    }
    if !crate::protocol::valid_host_uuid(&self.host_id) {
      return Err("Hub returned an invalid machine UUID".into());
    }
    validate_machine_name(&self.name)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn addresses_accept_only_canonical_bounded_slugs() {
    for address in ["a:b", "alice:workstation", "user-1:host-2"] {
      let (username, machine_name) = parse_machine_address(address).unwrap();
      assert_eq!(format!("{username}:{machine_name}"), address);
    }
    assert!(parse_machine_address(&format!("{}:{}", "a".repeat(63), "b".repeat(63))).is_ok());
    for invalid in [
      "",
      "alice",
      ":host",
      "alice:",
      "alice:host:extra",
      "Alice:host",
      "alice:Host",
      " alice:host",
      "alice:host ",
      "alice:host\n",
      "alice:-host",
      "alice:host-",
      "-alice:host",
      "alice-:host",
      "alice:host_name",
      "alice:host.example",
      "alice:host/path",
      "alice:host%20",
      "alice:host@key",
      "álîce:host",
    ] {
      assert!(parse_machine_address(invalid).is_err(), "{invalid:?}");
    }
    assert!(parse_machine_address(&format!("{}:host", "a".repeat(64))).is_err());
    assert!(parse_machine_address(&format!("alice:{}", "b".repeat(64))).is_err());
  }

  #[test]
  fn resolution_requires_matching_address_canonical_uuid_and_bounded_name() {
    let resolved = ResolvedMachine {
      host_id: "11111111-1111-4111-8111-111111111111".into(),
      machine_address: "alice:workstation".into(),
      name: "Workstation".into(),
      online: false,
    };
    resolved.validate_for("alice:workstation").unwrap();
    assert!(resolved.validate_for("bob:workstation").is_err());
    for host_id in [
      "not-a-uuid",
      "11111111-1111-1111-8111-111111111111",
      "11111111-1111-4111-8111-11111111111A",
    ] {
      assert!(
        ResolvedMachine {
          host_id: host_id.into(),
          ..resolved.clone()
        }
        .validate_for("alice:workstation")
        .is_err()
      );
    }
    for name in ["".into(), " ".into(), "name\n".into(), "x".repeat(129)] {
      assert!(
        ResolvedMachine {
          name,
          ..resolved.clone()
        }
        .validate_for("alice:workstation")
        .is_err()
      );
    }
  }
}
