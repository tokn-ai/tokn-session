//! Public discovery references. A matching group never authorizes a machine.
use crate::{
  address::{parse_machine_address, validate_machine_name},
  protocol,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_DISCOVERY_MACHINES: usize = 64;
pub const MAX_DISCOVERY_RESPONSE: usize = 32 * 1024;
pub const DISCOVERY_ADDRESSES_HEADER: &str = "x-tokn-discovery-addresses";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryGroup {
  pub namespace: String,
  pub secret_digest: String,
}

pub fn validate_namespace(namespace: &str) -> Result<(), String> {
  crate::address::parse_machine_address(&format!("{namespace}:machine"))
    .map(|_| ())
    .map_err(|_| "Discovery namespaces require 1–63 lowercase letters, digits, or internal hyphens".into())
}

impl DiscoveryGroup {
  pub fn validate(&self) -> Result<(), String> {
    validate_namespace(&self.namespace)?;
    let digest = protocol::decode(&self.secret_digest, 32)?;
    if digest.len() != 32 || protocol::encode(&digest) != self.secret_digest {
      return Err("Invalid discovery secret digest".into());
    }
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveredMachine {
  pub host_id: String,
  pub name: String,
  pub online: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub machine_address: Option<String>,
}

impl DiscoveredMachine {
  pub fn validate(&self) -> Result<(), String> {
    if !protocol::valid_host_uuid(&self.host_id) {
      return Err("Invalid discovered machine identity".into());
    }
    validate_machine_name(&self.name)
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryResult {
  pub namespace: String,
  pub secret_digest: String,
  pub machines: Vec<DiscoveredMachine>,
  /// The exact capability is no longer assigned to this account namespace.
  /// This retires advisory discovery metadata, never a machine's trusted key.
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub retired: bool,
}

impl DiscoveryResult {
  pub fn validate_for(&self, group: &DiscoveryGroup) -> Result<(), String> {
    group.validate()?;
    if self.namespace != group.namespace
      || self.secret_digest != group.secret_digest
      || self.machines.len() > MAX_DISCOVERY_MACHINES
      || (self.retired && !self.machines.is_empty())
    {
      return Err("Hub returned a different or oversized discovery group".into());
    }
    let mut identities = HashSet::new();
    for machine in &self.machines {
      machine.validate()?;
      if let Some(address) = &machine.machine_address {
        let (username, _) = parse_machine_address(address)?;
        if username != group.namespace {
          return Err("Hub returned an address outside the discovery namespace".into());
        }
      }
      if !identities.insert(&machine.host_id) {
        return Err("Hub returned duplicate discovered machines".into());
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn discovery_responses_reject_scope_changes_duplicate_ids_and_oversized_lists() {
    let group = DiscoveryGroup {
      namespace: "personal".into(),
      secret_digest: protocol::encode(&[1; 32]),
    };
    let machine = DiscoveredMachine {
      host_id: "11111111-2222-4333-8444-555555555555".into(),
      name: "Laptop".into(),
      online: false,
      machine_address: Some("personal:laptop".into()),
    };
    let mut result = DiscoveryResult {
      namespace: group.namespace.clone(),
      secret_digest: group.secret_digest.clone(),
      machines: vec![machine.clone()],
      retired: false,
    };
    result.validate_for(&group).unwrap();
    result.retired = true;
    assert!(result.validate_for(&group).is_err());
    result.retired = false;
    result.machines[0].machine_address = Some("other:laptop".into());
    assert!(result.validate_for(&group).is_err());
    result.machines[0].machine_address = None;
    result.validate_for(&group).unwrap();
    assert!(
      !serde_json::to_value(&result).unwrap()["machines"][0]
        .as_object()
        .unwrap()
        .contains_key("machine_address")
    );
    result.machines.push(machine.clone());
    assert!(result.validate_for(&group).is_err());
    result.machines = vec![machine; MAX_DISCOVERY_MACHINES + 1];
    assert!(result.validate_for(&group).is_err());
    result.machines.clear();
    result.validate_for(&group).unwrap();
    assert!(serde_json::to_value(&result).unwrap().get("retired").is_none());
    let unmarked: DiscoveryResult = serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
    assert!(!unmarked.retired);
    result.retired = true;
    result.validate_for(&group).unwrap();
    assert_eq!(serde_json::to_value(&result).unwrap()["retired"], true);
    result.namespace = "work".into();
    assert!(result.validate_for(&group).is_err());
  }
}
