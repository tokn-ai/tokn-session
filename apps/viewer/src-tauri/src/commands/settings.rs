use crate::local_viewer::LocalViewer;
use tauri::{AppHandle, State};
use tokn_viewer_core::settings::MachineSettingsSnapshot;

#[tauri::command]
pub async fn get_machine_settings(
  app: AppHandle,
  state: State<'_, LocalViewer>,
) -> Result<MachineSettingsSnapshot, String> {
  let service = state.service(&app).await?;
  tauri::async_runtime::spawn_blocking(move || {
    let mut settings = service.get_machine_settings()?;
    // Native desktop commands bind to this computer. A remote selected machine
    // reaches the read-only HTTP projection through its own Hub connection.
    settings.reader.editable = true;
    settings.reader.edit_reason = None;
    settings.hosting.available = true;
    settings.hosting.editable = true;
    settings.hosting.reason = None;
    Ok(settings)
  })
  .await
  .map_err(|error| format!("Machine settings task failed: {error}"))?
}
