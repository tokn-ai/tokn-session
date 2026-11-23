use tauri::{AppHandle, State};
use tokn_viewer_core::session_list::{OpenSessionListRequest, SessionListSubscriptionRequest, SessionListUpdate};

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

#[tauri::command]
pub async fn open_session_list(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: OpenSessionListRequest,
) -> Result<SessionListUpdate, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.open_session_list(request))
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn renew_session_lists(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  ids: Vec<String>,
) -> Result<tokn_viewer_core::updates::SessionSubscriptionRenewal, String> {
  state.service(&app).await?.renew_session_lists(&ids)
}

#[tauri::command]
pub async fn release_session_list(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: SessionListSubscriptionRequest,
) -> Result<bool, String> {
  state.service(&app).await?.release_session_list(request)
}
