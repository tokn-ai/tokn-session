use crate::{connection, host};
use futures_util::lock::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use tokn_hub_client_core::{
  address::ResolvedMachine,
  discovery::{DiscoveryGroup, DiscoveryResult, MAX_DISCOVERY_RESPONSE},
  pairing::ClientPairing,
  presence::{MAX_PRESENCE_RESPONSE, PresenceRequest, PresenceResult},
  protocol,
  secure::{NoiseIdentity, decode_public_key},
};
use url::Url;
use zeroize::Zeroizing;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedHost {
  pub host_id: String,
  pub host_public_key: String,
  pub machine_address: Option<String>,
  pub name: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub discovery_group: Option<DiscoveryGroup>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
  version: u8,
  identity: String,
  hosts: Vec<SavedHost>,
  selected_host_id: Option<String>,
}
pub struct Store {
  pub hub: Url,
  pub identity: NoiseIdentity,
  saved: RefCell<Saved>,
}
thread_local! {
  static STORES:RefCell<BTreeMap<String,Rc<Store>>>=RefCell::new(BTreeMap::new());
  static GATE:Rc<Mutex<()>>=Rc::new(Mutex::new(()));
}
pub fn canonical_hub(value: &str) -> Result<Url, String> {
  if value.len() > 4096 {
    return Err("Hub address is too long".into());
  }
  let url = Url::parse(value.trim()).map_err(|_| "Enter a valid Hub URL")?;
  let local = match url.host() {
    Some(url::Host::Domain("localhost")) => true,
    Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
    Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
    _ => false,
  };
  if url.path() != "/"
    || !url.username().is_empty()
    || url.password().is_some()
    || url.query().is_some()
    || url.fragment().is_some()
    || !(url.scheme() == "https" || url.scheme() == "http" && local)
    || url.port() == Some(0)
  {
    return Err("Hub must be an HTTPS origin; HTTP is available only on loopback".into());
  }
  Ok(url)
}
pub fn endpoint(hub: &Url, id: &str) -> Result<String, String> {
  if !protocol::valid_host_uuid(id) {
    return Err("Enter a canonical machine UUID".into());
  }
  let mut url = hub.clone();
  url
    .set_scheme(if hub.scheme() == "https" { "wss" } else { "ws" })
    .map_err(|_| "Invalid Hub scheme")?;
  url.set_path(&format!("/hub/v1/secure/{id}"));
  Ok(url.into())
}
fn key(hub: &Url) -> String {
  format!("hub-{:x}", Sha256::digest(hub.as_str().as_bytes()))
}
pub async fn load(value: &str) -> Result<Rc<Store>, String> {
  let hub = canonical_hub(value)?;
  let gate = GATE.with(Clone::clone);
  let _guard = gate.lock().await;
  if let Some(store) = STORES.with(|stores| stores.borrow().get(hub.as_str()).cloned()) {
    return Ok(store);
  }
  if STORES.with(|stores| stores.borrow().len()) >= 16 {
    return Err("Saved Hub limit reached".into());
  }
  let value: Value = host::call("secrets.get", json!({"key":key(&hub)})).await?;
  let saved = match value.get("value") {
    Some(Value::String(secret)) => {
      let raw = Zeroizing::new(secret.clone());
      if raw.len() > 16 * 1024 {
        return Err("Saved device trust exceeds its limit".into());
      }
      serde_json::from_str::<Saved>(&raw).map_err(|_| "Saved device trust is invalid; it was not replaced")?
    }
    Some(Value::Null) => {
      let identity = NoiseIdentity::from_secret(host::entropy().await?)?;
      let saved = Saved {
        version: 1,
        identity: identity.export_secret(),
        hosts: Vec::new(),
        selected_host_id: None,
      };
      save(&hub, &saved).await?;
      saved
    }
    _ => return Err("Invalid secure storage response".into()),
  };
  validate(&saved)?;
  let identity = NoiseIdentity::import_secret(&saved.identity)?;
  let store = Rc::new(Store {
    hub: hub.clone(),
    identity,
    saved: RefCell::new(saved),
  });
  STORES.with(|stores| stores.borrow_mut().insert(hub.into(), store.clone()));
  Ok(store)
}
fn validate(saved: &Saved) -> Result<(), String> {
  if saved.version != 1 || saved.hosts.len() > 16 {
    return Err("Saved device trust is invalid".into());
  }
  let mut ids = std::collections::BTreeSet::new();
  for host in &saved.hosts {
    if !protocol::valid_host_uuid(&host.host_id) || !ids.insert(&host.host_id) {
      return Err("Saved machine identity is invalid".into());
    }
    decode_public_key(&host.host_public_key)?;
    if let Some(group) = &host.discovery_group {
      group.validate()?;
    }
    if let Some(address) = &host.machine_address {
      tokn_hub_client_core::address::parse_machine_address(address)?;
    }
    if let Some(name) = &host.name {
      tokn_hub_client_core::address::validate_machine_name(name)?;
    }
  }
  if saved.selected_host_id.as_ref().is_some_and(|id| !ids.contains(id)) {
    return Err("Saved selected machine is invalid".into());
  }
  Ok(())
}
async fn save(hub: &Url, saved: &Saved) -> Result<(), String> {
  validate(saved)?;
  let encoded = Zeroizing::new(serde_json::to_string(saved).map_err(|_| "Could not encode device trust")?);
  if encoded.len() > 16 * 1024 {
    return Err("Saved device trust exceeds its limit".into());
  }
  host::call::<Value>("secrets.set", json!({"key":key(hub),"value":encoded.as_str()}))
    .await
    .map(|_| ())
}
impl Store {
  pub fn hosts(&self) -> Vec<SavedHost> {
    self.saved.borrow().hosts.clone()
  }
  pub fn selected(&self) -> Option<String> {
    self.saved.borrow().selected_host_id.clone()
  }
  pub async fn select(&self, id: &str) -> Result<(), String> {
    let gate = GATE.with(Clone::clone);
    let _guard = gate.lock().await;
    let mut saved = clone_saved(&self.saved.borrow());
    saved.selected_host_id = Some(id.into());
    save(&self.hub, &saved).await?;
    *self.saved.borrow_mut() = saved;
    Ok(())
  }
  async fn put(&self, host: SavedHost) -> Result<(), String> {
    let gate = GATE.with(Clone::clone);
    let _guard = gate.lock().await;
    let mut saved = clone_saved(&self.saved.borrow());
    if let Some(existing) = saved.hosts.iter_mut().find(|entry| entry.host_id == host.host_id) {
      if existing.host_public_key != host.host_public_key {
        return Err("Machine key differs from its remembered identity".into());
      }
      *existing = host;
    } else {
      saved.hosts.push(host);
    }
    save(&self.hub, &saved).await?;
    *self.saved.borrow_mut() = saved;
    Ok(())
  }

  async fn remember_metadata(&self, machine: ResolvedMachine) -> Result<SavedHost, String> {
    let gate = GATE.with(Clone::clone);
    let _guard = gate.lock().await;
    let mut saved = clone_saved(&self.saved.borrow());
    let host = saved
      .hosts
      .iter_mut()
      .find(|host| host.host_id == machine.host_id)
      .ok_or("Pair this machine before saving its address")?;
    host.machine_address = Some(machine.machine_address);
    host.name = Some(machine.name);
    let remembered = host.clone();
    save(&self.hub, &saved).await?;
    *self.saved.borrow_mut() = saved;
    Ok(remembered)
  }

  pub async fn remember_discovery_group(
    &self,
    host_id: &str,
    host_key: &str,
    group: Option<DiscoveryGroup>,
  ) -> Result<(), String> {
    if let Some(group) = &group {
      group.validate()?;
    }
    let gate = GATE.with(Clone::clone);
    let _guard = gate.lock().await;
    let mut saved = clone_saved(&self.saved.borrow());
    let host = saved
      .hosts
      .iter_mut()
      .find(|host| host.host_id == host_id)
      .ok_or("Pair this machine before remembering its discovery group")?;
    if host.host_public_key != host_key {
      return Err("Machine key differs from its remembered identity".into());
    }
    if host.discovery_group == group {
      return Ok(());
    }
    host.discovery_group = group;
    save(&self.hub, &saved).await?;
    *self.saved.borrow_mut() = saved;
    Ok(())
  }
}
fn clone_saved(saved: &Saved) -> Saved {
  Saved {
    version: saved.version,
    identity: saved.identity.clone(),
    hosts: saved.hosts.clone(),
    selected_host_id: saved.selected_host_id.clone(),
  }
}
pub async fn status(hub: &str) -> Result<Value, String> {
  let store = load(hub).await?;
  Ok(
    json!({"hub_url":store.hub.origin().ascii_serialization(),"device_public_key":store.identity.public_key(),"hosts":store.hosts(),"selected_host_id":store.selected()}),
  )
}
pub async fn resolve(hub: &str, address: &str) -> Result<Value, String> {
  let (user, machine) = tokn_hub_client_core::address::parse_machine_address(address)?;
  let mut endpoint = canonical_hub(hub)?;
  endpoint.set_path(&format!("/hub/v1/resolve/{user}/{machine}"));
  let response: Value = host::call("http.request", json!({"url":endpoint.as_str(),"timeout_ms":10000})).await?;
  let bytes = directory_body(response, 4096)?;
  let machine: ResolvedMachine = serde_json::from_slice(&bytes).map_err(|_| "Invalid Hub directory metadata")?;
  machine.validate_for(address)?;
  serde_json::to_value(machine).map_err(|_| "Invalid Hub metadata".into())
}
pub async fn discover(params: Value) -> Result<Value, String> {
  let store = load(host::string(&params, "hub_url")?).await?;
  let group = DiscoveryGroup {
    namespace: host::string(&params, "namespace")?.into(),
    secret_digest: host::string(&params, "secret_digest")?.into(),
  };
  group.validate()?;
  if !store
    .hosts()
    .iter()
    .any(|host| host.discovery_group.as_ref() == Some(&group))
  {
    return Err("Pair a machine in this discovery group first".into());
  }
  let mut endpoint = store.hub.clone();
  endpoint.set_path(&format!(
    "/hub/v1/discovery/{}/{}",
    group.namespace, group.secret_digest
  ));
  let response: Value = host::call("http.request", json!({"url":endpoint.as_str(),"timeout_ms":10000})).await?;
  let bytes = directory_body(response, MAX_DISCOVERY_RESPONSE)?;
  let result: DiscoveryResult = serde_json::from_slice(&bytes).map_err(|_| "Invalid Hub discovery metadata")?;
  result.validate_for(&group)?;
  serde_json::to_value(result).map_err(|_| "Invalid Hub discovery metadata".into())
}

pub async fn presence(params: Value) -> Result<Value, String> {
  let request = presence_request(&params)?;
  let mut endpoint = canonical_hub(host::string(&params, "hub_url")?)?;
  if request.host_ids.is_empty() {
    return Ok(json!({"machines":[]}));
  }
  endpoint.set_path("/hub/v1/presence");
  let body = serde_json::to_vec(&request).map_err(|_| "Could not encode Hub presence request")?;
  let response: Value = host::call(
    "http.request",
    json!({"url":endpoint.as_str(),"method":"POST","headers":[{"name":"Content-Type","value":"application/json"},{"name":"Accept","value":"application/json"}],"body":body,"timeout_ms":10000}),
  )
  .await?;
  let bytes = directory_body(response, MAX_PRESENCE_RESPONSE)?;
  let result: PresenceResult = serde_json::from_slice(&bytes).map_err(|_| "Invalid Hub presence metadata")?;
  result.validate_for(&request.host_ids)?;
  serde_json::to_value(result).map_err(|_| "Invalid Hub presence metadata".into())
}

fn presence_request(params: &Value) -> Result<PresenceRequest, String> {
  let host_ids = params.get("host_ids").ok_or("Missing host_ids")?;
  let request = PresenceRequest {
    host_ids: serde_json::from_value(host_ids.clone()).map_err(|_| "Invalid presence host_ids")?,
  };
  request.validate()?;
  Ok(request)
}

fn directory_body(mut response: Value, maximum: usize) -> Result<Vec<u8>, String> {
  if response["status"] != 200 {
    return Err("Could not load machine directory through the Hub".into());
  }
  let body = response.get_mut("body").ok_or("Missing Hub directory response body")?;
  if !body.as_array().is_some_and(|bytes| bytes.len() <= maximum) {
    return Err("Hub directory response exceeds its size limit".into());
  }
  serde_json::from_value(body.take()).map_err(|_| "Invalid Hub directory response".into())
}
pub async fn remember(params: Value) -> Result<Value, String> {
  let store = load(host::string(&params, "hub_url")?).await?;
  let address = host::string(&params, "machine_address")?;
  let id = host::string(&params, "host_id")?;
  let resolved: ResolvedMachine =
    serde_json::from_value(resolve(store.hub.as_str(), address).await?).map_err(|_| "Invalid resolved machine")?;
  if resolved.host_id != id {
    return Err("Machine address now identifies another machine".into());
  }
  Ok(json!(store.remember_metadata(resolved).await?))
}
pub async fn pair(params: Value) -> Result<Value, String> {
  let store = load(host::string(&params, "hub_url")?).await?;
  let id = host::string(&params, "host_id")?;
  if store.hosts().iter().any(|host| host.host_id == id) {
    return Err("This machine is already paired".into());
  }
  let code = Zeroizing::new(host::string(&params, "code")?.to_owned());
  let socket = host::Socket::open(&endpoint(&store.hub, id)?).await?;
  let (pairing, first) =
    ClientPairing::start_with_entropy(id, &store.identity, &code, host::unix_time()?, host::entropy().await?)?;
  socket.send(first).await?;
  let (pairing, confirmation) = pairing.confirm(&socket.receive().await?)?;
  socket.send(confirmation).await?;
  let verified = pairing.finish(&socket.receive().await?)?;
  socket.close().await;
  if params
    .get("expected_host_public_key")
    .and_then(Value::as_str)
    .is_some_and(|key| key != verified.host_public_key)
  {
    return Err("Paired machine key differs from the trusted reference".into());
  }
  let mut host = SavedHost {
    host_id: verified.host_id,
    host_public_key: verified.host_public_key,
    machine_address: None,
    name: None,
    discovery_group: verified.discovery_group,
  };
  if let Some(address) = params.get("machine_address").and_then(Value::as_str) {
    let resolved: ResolvedMachine =
      serde_json::from_value(resolve(store.hub.as_str(), address).await?).map_err(|_| "Invalid machine address")?;
    if resolved.host_id != host.host_id {
      return Err("Machine address changed during pairing".into());
    }
    host.machine_address = Some(resolved.machine_address);
    host.name = Some(resolved.name);
  }
  store.put(host.clone()).await?;
  Ok(json!(host))
}
pub async fn forget(params: Value) -> Result<Value, String> {
  let store = load(host::string(&params, "hub_url")?).await?;
  let id = host::string(&params, "host_id")?;
  connection::close_host(store.hub.as_str(), id).await;
  let gate = GATE.with(Clone::clone);
  let _guard = gate.lock().await;
  let mut saved = clone_saved(&store.saved.borrow());
  saved.hosts.retain(|host| host.host_id != id);
  if saved.selected_host_id.as_deref() == Some(id) {
    saved.selected_host_id = None;
  }
  save(&store.hub, &saved).await?;
  *store.saved.borrow_mut() = saved;
  Ok(Value::Null)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn presence_parameters_require_a_bounded_unique_list_of_canonical_machine_ids() {
    let host_id = "550e8400-e29b-41d4-a716-446655440000";
    assert_eq!(
      presence_request(&json!({"host_ids":[host_id]})).unwrap().host_ids,
      [host_id]
    );
    assert!(presence_request(&json!({"host_ids":[]})).unwrap().host_ids.is_empty());
    for params in [
      json!({}),
      json!({"host_ids":"not an array"}),
      json!({"host_ids":["invalid"]}),
      json!({"host_ids":[host_id,host_id]}),
      json!({"host_ids":vec![host_id;129]}),
    ] {
      assert!(presence_request(&params).is_err());
    }
  }

  #[test]
  fn hub_origin_never_accepts_embedded_trust_or_remote_plaintext() {
    assert!(canonical_hub("https://hub.example").is_ok());
    assert!(canonical_hub("http://127.0.0.1:8080").is_ok());
    for value in [
      "http://hub.example",
      "https://user@hub.example",
      "https://hub.example/path",
      "https://hub.example?secret=x",
    ] {
      assert!(canonical_hub(value).is_err());
    }
  }

  #[test]
  fn discovery_metadata_is_optional_for_saved_hosts_but_always_validated() {
    let identity = NoiseIdentity::from_secret([3; 32]).unwrap();
    let mut saved: Saved = serde_json::from_value(json!({
      "version":1,
      "identity":identity.export_secret(),
      "hosts":[{
        "host_id":"11111111-2222-4333-8444-555555555555",
        "host_public_key":identity.public_key(),
        "machine_address":null,
        "name":null,
      }],
      "selected_host_id":null,
    }))
    .unwrap();
    validate(&saved).unwrap();
    assert!(saved.hosts[0].discovery_group.is_none());
    saved.hosts[0].discovery_group = Some(DiscoveryGroup {
      namespace: "personal".into(),
      secret_digest: protocol::encode(&[1; 32]),
    });
    validate(&saved).unwrap();
    saved.hosts[0].discovery_group.as_mut().unwrap().namespace = "Personal".into();
    assert!(validate(&saved).is_err());
  }

  #[test]
  fn directory_bodies_are_bounded_before_decoding_bytes() {
    assert_eq!(
      directory_body(json!({"status":200,"body":[123,125]}), 2).unwrap(),
      b"{}"
    );
    assert!(directory_body(json!({"status":200,"body":[123,125]}), 1).is_err());
    assert!(directory_body(json!({"status":200,"body":[256]}), 1).is_err());
    assert!(directory_body(json!({"status":302,"body":[]}), 2).is_err());
  }
}
