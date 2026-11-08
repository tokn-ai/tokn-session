//! Remote-only applet backend. Keys, pairing and encrypted network state live
//! here; the shared viewer owns rendered histories and display caches.
mod bridge;
mod connection;
mod host;
mod preferences;
mod storage;

use apell_guest::HostError;
use serde_json::{Value, json};

apell_guest::export!(handle);

pub async fn handle(command: String, params: Value) -> Result<Value, HostError> {
  dispatch(&command, params).await.map_err(HostError::new)
}

async fn dispatch(command: &str, params: Value) -> Result<Value, String> {
  match command {
    // Initialization is local and bounded; connection startup belongs to an
    // explicit command, never the host's thirty-second initialize deadline.
    "initialize" => Ok(Value::Null),
    "snapshot" => Ok(json!({"version":1,"connections":connection::inventory()})),
    "ack_bridge_chunk" => bridge::acknowledge(params),
    "initialize_device_preferences" => preferences::initialize(params).await,
    "write_viewer_preferences" => preferences::write("viewer", params).await,
    "write_machine_preferences" => preferences::write("machines", params).await,
    "hub_client_status" => storage::status(host::string(&params, "hub_url")?).await,
    "hub_client_resolve" => {
      storage::resolve(
        host::string(&params, "hub_url")?,
        host::string(&params, "machine_address")?,
      )
      .await
    }
    "hub_client_remember_metadata" => storage::remember(params).await,
    "hub_client_discover" => storage::discover(params).await,
    "hub_client_pair" => storage::pair(params).await,
    "hub_client_forget" => storage::forget(params).await,
    "hub_client_open" => connection::open(params).await,
    "hub_client_request" => connection::request(params).await,
    "hub_client_listen" => connection::listen(host::string(&params, "connection_id")?).await,
    "hub_client_close" => {
      connection::close(host::string(&params, "connection_id")?).await;
      Ok(Value::Null)
    }
    "hub_client_auth_start" | "hub_client_auth_finish" | "hub_client_auth_cancel" => {
      Err("Pair this applet with an authenticator code; browser passkey sign-in is unavailable here".into())
    }
    _ => Err("Unknown Tokn Session applet command".into()),
  }
}

pub(crate) fn emit(name: &str, payload: Value) {
  let _ = apell_guest::emit(json!({"name":name,"payload":payload}));
}
