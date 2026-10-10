use crate::local_viewer::LocalViewer;
use tauri::{AppHandle, State};

#[tauri::command]
pub async fn initialize_local_viewer(app: AppHandle, state: State<'_, LocalViewer>) -> Result<(), String> {
  state.service(&app).await.map(|_| ())
}
