//! Thin app bridge. The native client connects directly to Hub ciphertext routes.
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};
use tokn_hub_remote::{AuthOptions, ClientStatus, ConnectionInfo, RemoteManager, ResolvedMachine};

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_status(state: State<'_, RemoteManager>, hub_url: String) -> Result<ClientStatus, String> {
  state.status(&hub_url).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_resolve(
  state: State<'_, RemoteManager>,
  hub_url: String,
  machine_address: String,
) -> Result<ResolvedMachine, String> {
  state.resolve(&hub_url, &machine_address).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_remember_metadata(
  state: State<'_, RemoteManager>,
  hub_url: String,
  host_id: String,
  machine_address: String,
  name: String,
) -> Result<tokn_hub_remote::SavedHost, String> {
  state
    .remember_metadata(&hub_url, &host_id, &machine_address, &name)
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_pair(
  state: State<'_, RemoteManager>,
  hub_url: String,
  host_id: String,
  code: String,
  expected_host_public_key: Option<String>,
  machine_address: Option<String>,
) -> Result<tokn_hub_remote::SavedHost, String> {
  state
    .pair_with_address(
      &hub_url,
      &host_id,
      code,
      expected_host_public_key.as_deref(),
      machine_address.as_deref(),
    )
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_open(
  app: AppHandle,
  state: State<'_, RemoteManager>,
  hub_url: String,
  host_id: String,
  host_public_key: Option<String>,
) -> Result<ConnectionInfo, String> {
  let sink = std::sync::Arc::new(move |event: &str, payload: Value| {
    let _ = app.emit(event, payload);
  });
  state.open(&hub_url, &host_id, host_public_key.as_deref(), sink).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_request(
  state: State<'_, RemoteManager>,
  connection_id: String,
  command: String,
  payload: Option<Value>,
) -> Result<Value, String> {
  state
    .request(
      &connection_id,
      &command,
      payload.unwrap_or_else(|| serde_json::json!({})),
    )
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_listen(state: State<'_, RemoteManager>, connection_id: String) -> Result<(), String> {
  state.listen(&connection_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_close(state: State<'_, RemoteManager>, connection_id: String) -> Result<(), String> {
  state.close(&connection_id).await;
  Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_forget(
  state: State<'_, RemoteManager>,
  hub_url: String,
  host_id: String,
) -> Result<(), String> {
  state.forget(&hub_url, &host_id).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_auth_start(
  state: State<'_, RemoteManager>,
  hub_url: String,
  host_id: String,
  host_public_key: String,
  register: bool,
) -> Result<AuthOptions, String> {
  state.auth_start(&hub_url, &host_id, &host_public_key, register).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_auth_finish(
  state: State<'_, RemoteManager>,
  auth_id: String,
  credential: Value,
) -> Result<tokn_hub_remote::SavedHost, String> {
  state.auth_finish(&auth_id, credential).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_auth_cancel(
  state: State<'_, RemoteManager>,
  callbacks: State<'_, crate::hub_passkey::PasskeyCallbacks>,
  auth_id: String,
) -> Result<(), String> {
  state.auth_cancel(&auth_id).await;
  callbacks.cancel(&auth_id).await;
  Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn hub_client_passkey_credential(
  app: AppHandle,
  callbacks: State<'_, crate::hub_passkey::PasskeyCallbacks>,
  hub_url: String,
  options: Value,
  register: bool,
  auth_id: Option<String>,
) -> Result<Value, String> {
  crate::hub_passkey::credential(app, callbacks.inner().clone(), &hub_url, options, register, auth_id).await
}
