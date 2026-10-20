use tauri::{AppHandle, State};
use tokn_viewer_core::images::{
  LoadSessionImageRequest, ReleaseImageUploadRequest, SessionImage, UploadImageChunkRequest, UploadImageChunkResponse,
};

use crate::local_viewer::LocalViewer;

#[tauri::command]
pub async fn load_session_image(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: LoadSessionImageRequest,
) -> Result<SessionImage, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.load_session_image(request))
    .await
    .map_err(|error| format!("session image task failed: {error}"))?
}

#[tauri::command]
pub async fn upload_image_chunk(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: UploadImageChunkRequest,
) -> Result<UploadImageChunkResponse, String> {
  state.service(&app).await?.upload_image_chunk(request).await
}

#[tauri::command]
pub async fn release_image_upload(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: ReleaseImageUploadRequest,
) -> Result<(), String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || service.release_image_upload(request))
    .await
    .map_err(|error| format!("image upload cleanup task failed: {error}"))?
}
