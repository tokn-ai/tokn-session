use tauri::{AppHandle, State};

use crate::local_viewer::LocalViewer;
use crate::model::{
  AcknowledgeSessionAttentionRequest, AcknowledgeSessionAttentionResponse, EventDetail, EventPage, EventPageRequest,
  LoadEventDetailRequest, LoadTrajectoryEventPageRequest, SessionViewRequest, TrajectoryEventPage,
};

#[tauri::command]
pub async fn load_event_page(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: EventPageRequest,
) -> Result<EventPage, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.load_event_page(request))
    .await
    .map_err(|error| format!("event loading task failed: {error}"))?
}

#[tauri::command]
pub async fn load_event_detail(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: LoadEventDetailRequest,
) -> Result<EventDetail, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.load_event_detail(request))
    .await
    .map_err(|error| format!("event detail task failed: {error}"))?
}

#[tauri::command]
pub async fn load_trajectory_event_page(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: LoadTrajectoryEventPageRequest,
) -> Result<TrajectoryEventPage, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.load_trajectory_event_page(request))
    .await
    .map_err(|error| format!("trajectory event loading task failed: {error}"))?
}

#[tauri::command]
pub async fn acknowledge_session_attention(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: AcknowledgeSessionAttentionRequest,
) -> Result<AcknowledgeSessionAttentionResponse, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.acknowledge_session_attention(request))
    .await
    .map_err(|error| format!("session attention acknowledgement task failed: {error}"))?
}

#[tauri::command]
pub async fn update_session_view(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: SessionViewRequest,
) -> Result<(), String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.update_session_view(request))
    .await
    .map_err(|error| format!("session view update task failed: {error}"))?
}

#[tauri::command]
pub async fn load_session_updates(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: tokn_viewer_core::updates::SessionUpdatesRequest,
) -> Result<tokn_viewer_core::updates::SessionUpdate, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.load_session_updates(request))
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn subscribe_session(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: tokn_viewer_core::updates::SessionUpdatesRequest,
) -> Result<serde_json::Value, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.subscribe_session(request))
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub async fn load_session_backward(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: tokn_viewer_core::updates::SessionUpdatesRequest,
) -> Result<tokn_viewer_core::updates::SessionUpdate, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.load_session_backward(request))
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub async fn load_session_details(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: tokn_viewer_core::delivery::SessionDetailsRequest,
) -> Result<tokn_viewer_core::delivery::SessionDetails, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.load_session_details(request))
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub async fn inspect_session_event(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: LoadEventDetailRequest,
) -> Result<EventDetail, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.inspect_session_event(request))
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub async fn renew_session_subscriptions(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  ids: Vec<String>,
) -> Result<(), String> {
  state.service(&app).await?.renew_session_subscriptions(&ids)
}
