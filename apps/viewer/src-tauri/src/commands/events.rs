use tauri::State;

use crate::model::{
  AcknowledgeSessionAttentionRequest, AcknowledgeSessionAttentionResponse, EventDetail, EventPage, EventPageRequest,
  LoadEventDetailRequest, LoadTrajectoryEventPageRequest, SessionViewRequest, TrajectoryEventPage,
};
use crate::service::ViewerService;

#[tauri::command]
pub async fn load_event_page(state: State<'_, ViewerService>, request: EventPageRequest) -> Result<EventPage, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.load_event_page(request))
    .await
    .map_err(|error| format!("event loading task failed: {error}"))?
}

#[tauri::command]
pub async fn load_event_detail(
  state: State<'_, ViewerService>,
  request: LoadEventDetailRequest,
) -> Result<EventDetail, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.load_event_detail(request))
    .await
    .map_err(|error| format!("event detail task failed: {error}"))?
}

#[tauri::command]
pub async fn load_trajectory_event_page(
  state: State<'_, ViewerService>,
  request: LoadTrajectoryEventPageRequest,
) -> Result<TrajectoryEventPage, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.load_trajectory_event_page(request))
    .await
    .map_err(|error| format!("trajectory event loading task failed: {error}"))?
}

#[tauri::command]
pub async fn acknowledge_session_attention(
  state: State<'_, ViewerService>,
  request: AcknowledgeSessionAttentionRequest,
) -> Result<AcknowledgeSessionAttentionResponse, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.acknowledge_session_attention(request))
    .await
    .map_err(|error| format!("session attention acknowledgement task failed: {error}"))?
}

#[tauri::command]
pub async fn update_session_view(state: State<'_, ViewerService>, request: SessionViewRequest) -> Result<(), String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.update_session_view(request))
    .await
    .map_err(|error| format!("session view update task failed: {error}"))?
}

#[tauri::command]
pub async fn load_session_updates(
  state: State<'_, ViewerService>,
  request: tokn_viewer_core::updates::SessionUpdatesRequest,
) -> Result<tokn_viewer_core::updates::SessionUpdate, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.load_session_updates(request))
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn subscribe_session(
  state: State<'_, ViewerService>,
  request: tokn_viewer_core::updates::SessionUpdatesRequest,
) -> Result<serde_json::Value, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.subscribe_session(request))
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub async fn load_session_backward(
  state: State<'_, ViewerService>,
  request: tokn_viewer_core::updates::SessionUpdatesRequest,
) -> Result<tokn_viewer_core::updates::SessionUpdate, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.load_session_backward(request))
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub async fn load_session_details(
  state: State<'_, ViewerService>,
  request: tokn_viewer_core::delivery::SessionDetailsRequest,
) -> Result<tokn_viewer_core::delivery::SessionDetails, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.load_session_details(request))
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub async fn inspect_session_event(
  state: State<'_, ViewerService>,
  request: LoadEventDetailRequest,
) -> Result<EventDetail, String> {
  let service = state.inner().clone();
  tauri::async_runtime::spawn_blocking(move || service.inspect_session_event(request))
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub fn renew_session_subscriptions(state: State<'_, ViewerService>, ids: Vec<String>) -> Result<(), String> {
  state.renew_session_subscriptions(&ids)
}
