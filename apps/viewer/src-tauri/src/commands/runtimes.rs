use tauri::AppHandle;
use tokn_viewer_core::runtimes::{RuntimeInventory, RuntimeRestartRequest, RuntimeRestartResponse};

#[tauri::command]
pub async fn list_viewer_runtimes(app: AppHandle) -> Result<RuntimeInventory, String> {
  Ok(crate::runtime_management::inventory(&app).await)
}

#[tauri::command]
pub async fn restart_viewer_runtime(
  app: AppHandle,
  request: RuntimeRestartRequest,
) -> Result<RuntimeRestartResponse, String> {
  crate::runtime_management::restart(app, request).await
}
