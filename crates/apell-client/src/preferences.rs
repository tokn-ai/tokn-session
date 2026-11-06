//! Device preferences stay independent of the currently selected session host.
use crate::{emit, host, storage};
use futures_util::lock::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::rc::Rc;

thread_local! {static GATE:Rc<Mutex<()>>=Rc::new(Mutex::new(()));}
const KEY: &str = "device-preferences";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
  version: u8,
  revision: u64,
  viewer_preferences: Value,
  machine_preferences: Value,
}

fn viewer_default() -> Value {
  json!({"version":1,"sidebar_order":"time","background_updates_scope":"all","background_update_level":"messages","conversation_update_level":"steps","show_lifecycle":false})
}
fn machines_default() -> Value {
  json!({"version":1,"selected_machine":null,"hub_urls":[]})
}
fn viewer(value: &Value) -> Result<(), String> {
  let object = value.as_object().ok_or("Invalid viewer preferences")?;
  if object.len() != 6
    || value["version"] != 1
    || !matches!(value["sidebar_order"].as_str(), Some("time" | "project"))
    || !matches!(value["background_updates_scope"].as_str(), Some("all" | "sidebar"))
    || !matches!(value["background_update_level"].as_str(), Some("final" | "messages"))
    || !matches!(
      value["conversation_update_level"].as_str(),
      Some("final" | "messages" | "steps" | "all")
    )
    || !value["show_lifecycle"].is_boolean()
  {
    return Err("Invalid viewer preferences".into());
  }
  Ok(())
}
fn machines(value: &Value) -> Result<(), String> {
  if value.as_object().is_none_or(|value| value.len() != 3) || value["version"] != 1 {
    return Err("Invalid machine preferences".into());
  }
  let hubs = value["hub_urls"]
    .as_array()
    .filter(|urls| urls.len() <= 16)
    .ok_or("Invalid saved Hub list")?;
  let mut unique = std::collections::BTreeSet::new();
  for hub in hubs {
    let canonical = storage::canonical_hub(hub.as_str().ok_or("Invalid Hub URL")?)?
      .origin()
      .ascii_serialization();
    if !unique.insert(canonical) {
      return Err("Duplicate saved Hub URL".into());
    }
  }
  if !value["selected_machine"].is_null() {
    let selected = &value["selected_machine"];
    if selected["kind"] != "hub"
      || selected.as_object().is_none_or(|value| value.len() != 3)
      || !tokn_hub_client_core::protocol::valid_host_uuid(host::string(selected, "host_id")?)
    {
      return Err("Invalid selected remote machine".into());
    }
    storage::canonical_hub(host::string(selected, "hub_url")?)?;
  }
  Ok(())
}
async fn load(request: &Value) -> Result<Snapshot, String> {
  let response: Value = host::call("config.get", json!({"key":KEY})).await?;
  let snapshot = if response.get("found") == Some(&Value::Bool(true)) {
    serde_json::from_value::<Snapshot>(response["value"].clone()).map_err(|_| "Saved applet preferences are invalid")?
  } else if response.get("found") == Some(&Value::Bool(false)) {
    let proposed_viewer = request
      .get("viewer_preferences")
      .filter(|value| !value.is_null())
      .cloned()
      .unwrap_or_else(viewer_default);
    let proposed_machines = request
      .get("machine_preferences")
      .filter(|value| !value.is_null())
      .cloned()
      .unwrap_or_else(machines_default);
    Snapshot {
      version: 1,
      revision: 0,
      viewer_preferences: proposed_viewer,
      machine_preferences: proposed_machines,
    }
  } else {
    return Err("Invalid applet configuration response".into());
  };
  if snapshot.version != 1 || snapshot.revision > 9_007_199_254_740_991 {
    return Err("Invalid applet preference version or revision".into());
  }
  viewer(&snapshot.viewer_preferences)?;
  machines(&snapshot.machine_preferences)?;
  Ok(snapshot)
}
pub async fn initialize(params: Value) -> Result<Value, String> {
  let gate = GATE.with(Clone::clone);
  let _guard = gate.lock().await;
  let snapshot = load(params.get("request").unwrap_or(&Value::Null)).await?;
  host::call::<Value>("config.set", json!({"key":KEY,"value":snapshot})).await?;
  serde_json::to_value(snapshot).map_err(|_| "Invalid applet preferences".into())
}
pub async fn write(kind: &str, params: Value) -> Result<Value, String> {
  let gate = GATE.with(Clone::clone);
  let _guard = gate.lock().await;
  let write_id = host::string(&params, "write_id")?;
  if write_id.is_empty()
    || write_id.len() > 64
    || !write_id
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
  {
    return Err("Invalid preference write identity".into());
  }
  let preferences = params.get("preferences").cloned().ok_or("Missing preferences")?;
  if kind == "viewer" {
    viewer(&preferences)?;
  } else {
    machines(&preferences)?;
  }
  let mut snapshot = load(&Value::Null).await?;
  snapshot.revision = snapshot
    .revision
    .checked_add(1)
    .filter(|revision| *revision <= 9_007_199_254_740_991)
    .ok_or("Preference revision exhausted")?;
  if kind == "viewer" {
    snapshot.viewer_preferences = preferences.clone();
  } else {
    snapshot.machine_preferences = preferences.clone();
  }
  host::call::<Value>("config.set", json!({"key":KEY,"value":snapshot})).await?;
  let update = json!({"revision":snapshot.revision,"write_id":write_id,"preferences":preferences});
  emit(
    if kind == "viewer" {
      "viewer-preferences-changed"
    } else {
      "machine-preferences-changed"
    },
    update.clone(),
  );
  Ok(update)
}
#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn preferences_stay_remote_scoped_and_bounded() {
    assert!(viewer(&viewer_default()).is_ok());
    assert!(machines(&machines_default()).is_ok());
    let mut invalid = viewer_default();
    invalid["conversation_update_level"] = json!("unknown");
    assert!(viewer(&invalid).is_err());
    assert!(machines(&json!({"version":1,"hub_urls":[],"selected_machine":{"kind":"local"}})).is_err());
    assert!(machines(&json!({"version":1,"hub_urls":["http://remote.example"],"selected_machine":null})).is_err());
  }
}
