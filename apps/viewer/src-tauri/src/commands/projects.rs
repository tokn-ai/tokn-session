use crate::local_viewer::LocalViewer;
use tauri::{AppHandle, Emitter, State};
use tokn_viewer_core::workspace::{
  CreateProjectSessionRequest, ManagedSessionDialogResponse, ManagedSessionRequest, ManagedSessionSnapshot,
  ProjectWorkspaceSnapshot, RegisterProjectRequest, RegisteredProject, SessionCreationError,
};

#[tauri::command]
pub async fn get_session_creation_options(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: tokn_viewer_core::workspace::SessionCreationOptionsRequest,
) -> Result<tokn_viewer_core::workspace::SessionCreationOptions, String> {
  state.service(&app).await?.get_session_creation_options(request).await
}

#[tauri::command]
pub async fn list_registered_projects(
  app: AppHandle,
  state: State<'_, LocalViewer>,
) -> Result<ProjectWorkspaceSnapshot, String> {
  let service = state.service(&app).await?;
  tokio::task::spawn_blocking(move || service.list_registered_projects())
    .await
    .map_err(|_| "Could not list registered projects")?
}

#[tauri::command]
pub async fn register_project(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RegisterProjectRequest,
) -> Result<RegisteredProject, String> {
  let service = state.service(&app).await?;
  let project = tokio::task::spawn_blocking(move || service.register_project(request))
    .await
    .map_err(|_| "Could not register the project folder")??;
  let _ = app.emit("registered-projects-changed", &project);
  Ok(project)
}

#[tauri::command]
pub async fn create_project_session(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: CreateProjectSessionRequest,
) -> Result<ManagedSessionSnapshot, SessionCreationError> {
  state
    .service(&app)
    .await
    .map_err(|message| SessionCreationError {
      message,
      may_have_started: false,
    })?
    .create_project_session(request)
    .await
}

#[tauri::command]
pub async fn get_managed_session(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: ManagedSessionRequest,
) -> Result<Option<ManagedSessionSnapshot>, String> {
  state.service(&app).await?.get_managed_session(request).await
}

#[tauri::command]
pub async fn stop_managed_session(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: ManagedSessionRequest,
) -> Result<ManagedSessionSnapshot, String> {
  state.service(&app).await?.stop_managed_session(request).await
}

#[tauri::command]
pub async fn resume_managed_session(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: ManagedSessionRequest,
) -> Result<ManagedSessionSnapshot, String> {
  state.service(&app).await?.resume_managed_session(request).await
}

#[tauri::command]
pub async fn respond_managed_session_dialog(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: ManagedSessionDialogResponse,
) -> Result<ManagedSessionSnapshot, String> {
  state.service(&app).await?.respond_managed_session_dialog(request).await
}
