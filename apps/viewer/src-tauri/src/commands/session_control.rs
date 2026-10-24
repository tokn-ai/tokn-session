use crate::local_viewer::LocalViewer;
use tauri::{AppHandle, State};
use tokn_viewer_core::{
  model::SubmitSessionInputResponse,
  session_supervisor::{
    CancelSessionTurnRequest, SessionControlRequest, SessionControlStatus, SetSessionControlRequest,
  },
};

#[tauri::command]
pub async fn get_session_control(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: SessionControlRequest,
) -> Result<SessionControlStatus, String> {
  state.service(&app).await?.get_session_control(request).await
}

#[tauri::command]
pub async fn set_session_control(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: SetSessionControlRequest,
) -> Result<SessionControlStatus, String> {
  state.service(&app).await?.set_session_control(request).await
}

#[tauri::command]
pub async fn cancel_session_turn(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: CancelSessionTurnRequest,
) -> Result<SubmitSessionInputResponse, String> {
  state.service(&app).await?.cancel_session_turn(request).await
}
