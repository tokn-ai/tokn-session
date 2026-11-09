//! Exact, bounded routing presence. An online connector grants no device access.
use crate::protocol::valid_host_uuid;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_PRESENCE_HOSTS: usize = 128;
pub const MAX_PRESENCE_RESPONSE: usize = 24 * 1024;
pub const CONNECTOR_VERSION_HEADER: &str = "x-tokn-connector-version";
pub const HUB_VERSION_HEADER: &str = "x-tokn-hub-version";
pub const PRESENCE_VERSIONS_HEADER: &str = "x-tokn-presence-versions";

pub fn valid_software_version(version: &str) -> bool {
  version.len() <= 64
    && version.as_bytes().first().is_some_and(u8::is_ascii_digit)
    && version
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresenceRequest {
  pub host_ids: Vec<String>,
}

fn validate_host_ids(host_ids: &[String]) -> Result<(), String> {
  if host_ids.len() > MAX_PRESENCE_HOSTS {
    return Err("Presence request exceeds its machine limit".into());
  }
  let mut seen = HashSet::new();
  for host_id in host_ids {
    if !valid_host_uuid(host_id) || !seen.insert(host_id.as_str()) {
      return Err("Presence requests require unique canonical machine UUIDs".into());
    }
  }
  Ok(())
}

impl PresenceRequest {
  pub fn validate(&self) -> Result<(), String> {
    validate_host_ids(&self.host_ids)
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachinePresence {
  pub host_id: String,
  pub online: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub connector_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresenceResult {
  pub machines: Vec<MachinePresence>,
}

impl PresenceResult {
  pub fn validate_for(&self, host_ids: &[String]) -> Result<(), String> {
    validate_host_ids(host_ids)?;
    if self.machines.len() != host_ids.len() {
      return Err("Hub presence response contains a different machine set".into());
    }
    let requested: HashSet<_> = host_ids.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    for machine in &self.machines {
      if !valid_host_uuid(&machine.host_id)
        || !requested.contains(machine.host_id.as_str())
        || !seen.insert(machine.host_id.as_str())
      {
        return Err("Hub presence response contains a different machine set".into());
      }
      if machine
        .connector_version
        .as_deref()
        .is_some_and(|version| !machine.online || !valid_software_version(version))
      {
        return Err("Hub presence response contains an invalid connector version".into());
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  const FIRST: &str = "550e8400-e29b-41d4-a716-446655440000";
  const SECOND: &str = "550e8400-e29b-41d4-a716-446655440001";

  #[test]
  fn requests_require_bounded_unique_canonical_machine_ids() {
    assert!(PresenceRequest { host_ids: vec![] }.validate().is_ok());
    assert!(
      PresenceRequest {
        host_ids: vec![FIRST.into(), SECOND.into()]
      }
      .validate()
      .is_ok()
    );
    for host_ids in [
      vec![FIRST.into(), FIRST.into()],
      vec![FIRST.to_uppercase()],
      vec!["not-a-uuid".into()],
      vec!["550e8400-e29b-11d4-a716-446655440000".into()],
      (0..=MAX_PRESENCE_HOSTS)
        .map(|id| format!("550e8400-e29b-41d4-a716-{id:012x}"))
        .collect(),
    ] {
      assert!(PresenceRequest { host_ids }.validate().is_err());
    }
  }

  #[test]
  fn responses_must_match_the_exact_requested_set() {
    let result = PresenceResult {
      machines: vec![
        MachinePresence {
          host_id: FIRST.into(),
          online: true,
          connector_version: Some("0.1.1-rc.1+abc".into()),
        },
        MachinePresence {
          host_id: SECOND.into(),
          online: false,
          connector_version: None,
        },
      ],
    };
    assert!(result.validate_for(&[SECOND.into(), FIRST.into()]).is_ok());
    assert!(result.validate_for(&[FIRST.into()]).is_err());
    assert!(result.validate_for(&[FIRST.into(), FIRST.into()]).is_err());
    let duplicate = PresenceResult {
      machines: vec![result.machines[0].clone(); 2],
    };
    assert!(duplicate.validate_for(&[FIRST.into(), SECOND.into()]).is_err());
    let invalid = PresenceResult {
      machines: vec![MachinePresence {
        host_id: FIRST.to_uppercase(),
        online: true,
        connector_version: None,
      }],
    };
    assert!(invalid.validate_for(&[FIRST.into()]).is_err());
    assert!(PresenceResult { machines: vec![] }.validate_for(&[]).is_ok());
  }

  #[test]
  fn connector_versions_require_a_live_host_and_fit_the_batch_budget() {
    let mut result = PresenceResult {
      machines: (0..MAX_PRESENCE_HOSTS)
        .map(|id| MachinePresence {
          host_id: format!("550e8400-e29b-41d4-a716-{id:012x}"),
          online: true,
          connector_version: Some(format!("1{}", "a".repeat(63))),
        })
        .collect(),
    };
    let ids: Vec<_> = result.machines.iter().map(|machine| machine.host_id.clone()).collect();
    assert!(result.validate_for(&ids).is_ok());
    assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_PRESENCE_RESPONSE);
    result.machines[0].online = false;
    assert!(result.validate_for(&ids).is_err());
    result.machines[0].online = true;
    for version in [
      "".to_owned(),
      "v1.0".into(),
      "1.0 with space".into(),
      "1.0-é".into(),
      "1".repeat(65),
    ] {
      result.machines[0].connector_version = Some(version);
      assert!(result.validate_for(&ids).is_err());
    }
  }

  #[test]
  fn wire_contract_rejects_extra_fields_and_non_boolean_status() {
    for value in [
      json!({"machines":[{"host_id":FIRST,"online":"true"}]}),
      json!({"machines":[{"host_id":FIRST,"online":true,"name":"Workstation"}]}),
      json!({"machines":[],"public_key":"unexpected"}),
    ] {
      assert!(serde_json::from_value::<PresenceResult>(value).is_err());
    }
    assert!(serde_json::from_value::<PresenceRequest>(json!({"host_ids":[],"all":true})).is_err());
    assert!(serde_json::from_value::<PresenceRequest>(json!({"host_ids":"all"})).is_err());
  }
}
