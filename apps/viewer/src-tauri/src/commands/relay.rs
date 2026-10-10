use tauri::{Manager, State};

use crate::{
  local_viewer::LocalViewer,
  relay::{RelaySettings, RelayStatus},
};

#[tauri::command]
pub async fn get_relay_status(app: tauri::AppHandle, state: State<'_, LocalViewer>) -> Result<RelayStatus, String> {
  Ok(state.service(&app).await?.relay.status())
}

#[tauri::command]
pub async fn configure_relay(
  app: tauri::AppHandle,
  state: State<'_, LocalViewer>,
  settings: RelaySettings,
) -> Result<RelayStatus, String> {
  let service = state.service(&app).await?;
  let _guard = service.relay.configure_lock.lock().await;
  let path = app
    .path()
    .app_config_dir()
    .map_err(|e| e.to_string())?
    .join("relay.json");
  let saved = settings.clone();
  tauri::async_runtime::spawn_blocking(move || crate::relay::write_settings(&path, &saved))
    .await
    .map_err(|e| e.to_string())??;
  service.relay.configure(settings)?;
  // A source-mode change may bring a previously covered provider back to local
  // history; do not leave its catalog waiting for the five-minute safety sweep.
  let _ = service.request_session_index_retry();
  Ok(service.relay.status())
}
