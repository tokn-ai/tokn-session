use crate::local_viewer::LocalViewer;
use tauri::{AppHandle, State};
use tokn_viewer_core::model::{
  SessionInputStatus, SessionInputStatusRequest, SubmitSessionInputRequest, SubmitSessionInputResponse,
};

#[tauri::command]
pub async fn get_session_input_status(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: SessionInputStatusRequest,
) -> Result<SessionInputStatus, String> {
  state.service(&app).await?.get_session_input_status(request).await
}

#[tauri::command]
pub async fn submit_session_input(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: SubmitSessionInputRequest,
) -> Result<SubmitSessionInputResponse, String> {
  state.service(&app).await?.submit_session_input(request).await
}
