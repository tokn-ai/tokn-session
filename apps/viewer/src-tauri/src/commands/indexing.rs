use tauri::{AppHandle, State};

use crate::local_viewer::LocalViewer;
use crate::model::SessionIndexProgress;

/// After local initialization, reads the worker snapshot without provider I/O.
#[tauri::command]
pub async fn get_session_index_progress(
  app: AppHandle,
  state: State<'_, LocalViewer>,
) -> Result<SessionIndexProgress, String> {
  Ok(state.service(&app).await?.session_index_progress())
}

/// Requests that the local index scheduler run again. This command only
/// queues a wake; it deliberately does not start a second provider worker on
/// the Tauri IPC task.
#[tauri::command]
pub async fn retry_session_index(
  app: AppHandle,
  state: State<'_, LocalViewer>,
) -> Result<SessionIndexProgress, String> {
  state.service(&app).await?.request_session_index_retry()
}
