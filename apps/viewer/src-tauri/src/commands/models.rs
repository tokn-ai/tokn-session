use crate::local_viewer::LocalViewer;
use tauri::{AppHandle, State};
use tokn_viewer_core::models::*;

#[tauri::command]
pub async fn get_session_model_status(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: SessionModelStatusRequest,
) -> Result<SessionModelStatus, String> {
  state.service(&app).await?.get_session_model_status(request).await
}

#[tauri::command]
pub async fn list_session_models(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: ListSessionModelsRequest,
) -> Result<ListSessionModelsResponse, String> {
  state.service(&app).await?.list_session_models(request).await
}
