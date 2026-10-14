use crate::local_viewer::LocalViewer;
use tauri::{AppHandle, State};
use tokn_viewer_core::background::BackgroundUpdatesRequest;

#[tauri::command]
pub async fn subscribe_background_updates(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: BackgroundUpdatesRequest,
) -> Result<serde_json::Value, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.subscribe_background(request))
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn renew_background_subscriptions(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  ids: Vec<String>,
) -> Result<serde_json::Value, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.renew_background_subscriptions(&ids))
    .await
    .map_err(|error| error.to_string())?
}
