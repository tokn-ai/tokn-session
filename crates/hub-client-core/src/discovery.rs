//! Public discovery references. A matching group never authorizes a machine.
use crate::{
  address::{parse_machine_address, validate_machine_name},
  protocol,
};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashSet;

pub const MAX_DISCOVERY_MACHINES: usize = 64;
pub const MAX_DISCOVERY_RESPONSE: usize = 32 * 1024;
pub const DISCOVERY_ADDRESSES_HEADER: &str = "x-tokn-discovery-addresses";
const DOMAIN: &[u8] = b"tokn-hub-discovery-group-v1";

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

  pub(crate) fn derive(seed: &[u8], hub_url: &str, namespace: &str) -> Result<Self, String> {
    validate_namespace(namespace)?;
    let mut hub = url::Url::parse(hub_url).map_err(|_| "Invalid discovery Hub URL")?;
    if hub.host_str().is_none()
      || !hub.username().is_empty()
      || hub.password().is_some()
      || hub.path() != "/"
      || hub.query().is_some()
      || hub.fragment().is_some()
    {
      return Err("Invalid discovery Hub URL".into());
    }
    let loopback = match hub.host() {
      Some(url::Host::Domain("localhost")) => true,
      Some(url::Host::Ipv4(address)) => address.is_loopback(),
      Some(url::Host::Ipv6(address)) => address.is_loopback(),
      _ => false,
    };
    let scheme = match hub.scheme() {
      "https" | "wss" => "https",
      "http" | "ws" if loopback => "http",
      _ => return Err("Discovery requires HTTPS or loopback HTTP".into()),
    };
    hub.set_scheme(scheme).map_err(|_| "Invalid discovery Hub URL")?;
    let context = serde_json::to_vec(&(hub.origin().ascii_serialization(), namespace))
      .map_err(|_| "Could not encode discovery scope")?;
    let mut digest = [0; 32];
    Hkdf::<Sha256>::new(Some(DOMAIN), seed)
      .expand(&context, &mut digest)
      .map_err(|_| "Could not derive discovery group")?;
    Ok(Self {
      namespace: namespace.into(),
      secret_digest: protocol::encode(&digest),
    })
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
}

impl DiscoveryResult {
  pub fn validate_for(&self, group: &DiscoveryGroup) -> Result<(), String> {
    group.validate()?;
    if self.namespace != group.namespace
      || self.secret_digest != group.secret_digest
      || self.machines.len() > MAX_DISCOVERY_MACHINES
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
  fn groups_bind_seed_namespace_and_hub_origin() {
    let first = DiscoveryGroup::derive(&[1; 32], "https://hub.example", "personal").unwrap();
    assert_eq!(
      first,
      DiscoveryGroup::derive(&[1; 32], "https://hub.example/", "personal").unwrap()
    );
    assert_ne!(
      first,
      DiscoveryGroup::derive(&[2; 32], "https://hub.example", "personal").unwrap()
    );
    assert_ne!(
      first,
      DiscoveryGroup::derive(&[1; 32], "https://hub.example", "work").unwrap()
    );
    assert_ne!(
      first,
      DiscoveryGroup::derive(&[1; 32], "https://other.example", "personal").unwrap()
    );
    first.validate().unwrap();
    assert!(DiscoveryGroup::derive(&[1; 32], "https://hub.example", "Personal").is_err());
    assert!(DiscoveryGroup::derive(&[1; 32], "http://hub.example", "personal").is_err());
    assert!(DiscoveryGroup::derive(&[1; 32], "https://hub.example/path", "personal").is_err());
  }

  #[test]
  fn discovery_responses_reject_scope_changes_duplicate_ids_and_oversized_lists() {
    let group = DiscoveryGroup::derive(&[1; 32], "https://hub.example", "personal").unwrap();
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
    };
    result.validate_for(&group).unwrap();
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
    result.namespace = "work".into();
    assert!(result.validate_for(&group).is_err());
  }

  #[test]
  fn discovery_normalizes_supported_transport_schemes_and_loopback_origins() {
    for (socket, http) in [
      ("wss://hub.example", "https://hub.example"),
      ("wss://hub.example:8443", "https://hub.example:8443"),
      ("ws://localhost:5559", "http://localhost:5559"),
      ("ws://127.0.0.2:5559", "http://127.0.0.2:5559"),
      ("ws://[::1]:5559", "http://[::1]:5559"),
    ] {
      assert_eq!(
        DiscoveryGroup::derive(&[1; 32], socket, "personal").unwrap(),
        DiscoveryGroup::derive(&[1; 32], http, "personal").unwrap(),
      );
    }
    for invalid in [
      "http://hub.example",
      "ws://hub.example",
      "ftp://hub.example",
      "https://user@hub.example",
      "https://user:password@hub.example",
      "https://hub.example/path",
      "https://hub.example/?query",
      "https://hub.example/#fragment",
    ] {
      assert!(
        DiscoveryGroup::derive(&[1; 32], invalid, "personal").is_err(),
        "{invalid}"
      );
    }
  }
}
