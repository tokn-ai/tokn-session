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

#[tauri::command]
pub async fn get_session_question_status(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: tokn_viewer_core::model::SessionQuestionStatusRequest,
) -> Result<tokn_viewer_core::model::SessionQuestionStatus, String> {
  state.service(&app).await?.get_session_question_status(request).await
}

#[tauri::command]
pub async fn submit_session_question_answers(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: tokn_viewer_core::model::SubmitSessionQuestionAnswersRequest,
) -> Result<SubmitSessionInputResponse, String> {
  state
    .service(&app)
    .await?
    .submit_session_question_answers(request)
    .await
}
