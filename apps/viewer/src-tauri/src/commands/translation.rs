use crate::translation::{self, TranslationFailure, TranslationRequest, TranslationResponse, TranslationStatus};

#[tauri::command]
pub async fn get_translation_status() -> TranslationStatus {
  translation::status()
}

#[tauri::command]
pub async fn translate_text(
  window: tauri::WebviewWindow,
  request: TranslationRequest,
) -> Result<TranslationResponse, TranslationFailure> {
  translation::translate(window, request).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn cancel_translation(window: tauri::WebviewWindow, request_id: String) -> Result<(), String> {
  translation::cancel(window, &request_id)
}

#[tauri::command]
pub async fn open_translation_settings() -> Result<(), String> {
  translation::open_settings().await
}
