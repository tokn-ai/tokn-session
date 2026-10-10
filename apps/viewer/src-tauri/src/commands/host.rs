use crate::local_host::{HostPairing, HostStatus, LocalHost, StartHostRequest};
use tauri::{AppHandle, State};

#[tauri::command]
pub async fn local_host_status(state: State<'_, LocalHost>) -> Result<HostStatus, String> {
  state.status().await
}
#[tauri::command]
pub async fn local_host_start(
  app: AppHandle,
  state: State<'_, LocalHost>,
  request: StartHostRequest,
) -> Result<HostStatus, String> {
  state.start(app, request).await
}
#[tauri::command]
pub async fn local_host_stop(app: AppHandle, state: State<'_, LocalHost>) -> Result<HostStatus, String> {
  state.stop(&app).await
}
#[tauri::command]
pub async fn local_host_pairing(state: State<'_, LocalHost>) -> Result<HostPairing, String> {
  state.pairing().await
}

#[tauri::command]
pub async fn local_host_stop_external(app: AppHandle, state: State<'_, LocalHost>) -> Result<HostStatus, String> {
  state.stop_external(&app).await
}
