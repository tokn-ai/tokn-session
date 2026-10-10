use tauri::{AppHandle, State};

use crate::local_viewer::LocalViewer;
use crate::model::{
  ListSessionChildrenRequest, ListSessionChildrenResponse, ListSessionsRequest, ListSessionsResponse,
};

#[tauri::command]
pub async fn list_sessions(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: ListSessionsRequest,
) -> Result<ListSessionsResponse, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.list_sessions(request))
    .await
    .map_err(|error| format!("session listing task failed: {error}"))?
}

#[tauri::command]
pub async fn list_session_children(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: ListSessionChildrenRequest,
) -> Result<ListSessionChildrenResponse, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.list_session_children(request))
    .await
    .map_err(|error| format!("session-child listing task failed: {error}"))?
}
