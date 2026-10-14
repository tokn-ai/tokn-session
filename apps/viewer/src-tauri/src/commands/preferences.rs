use crate::device_preferences::{
  self, DevicePreferences, DevicePreferencesSnapshot, InitializeDevicePreferencesRequest, MachinePreferences,
  ViewerPreferences,
};
use tauri::{AppHandle, Emitter, State};

#[derive(Clone, serde::Serialize)]
pub struct NativePreferenceUpdate<T> {
  revision: u64,
  write_id: String,
  preferences: T,
}

fn next_revision(current: u64, write_id: &str) -> Result<u64, String> {
  if write_id.is_empty()
    || write_id.len() > 64
    || !write_id
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
  {
    return Err("Invalid device preference write ID".into());
  }
  current
    .checked_add(1)
    .filter(|revision| *revision <= 9_007_199_254_740_991)
    .ok_or_else(|| "Device preference revision limit reached; restart the app".into())
}

#[tauri::command]
pub async fn initialize_device_preferences(
  state: State<'_, DevicePreferences>,
  request: InitializeDevicePreferencesRequest,
) -> Result<DevicePreferencesSnapshot, String> {
  let guard = state.gate.lock().await;
  let paths = state.paths()?;
  let mut snapshot = tauri::async_runtime::spawn_blocking(move || device_preferences::initialize(&paths, request))
    .await
    .map_err(|error| format!("Device preferences initialization failed: {error}"))??;
  snapshot.revision = *guard;
  Ok(snapshot)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn write_viewer_preferences(
  app: AppHandle,
  state: State<'_, DevicePreferences>,
  preferences: ViewerPreferences,
  write_id: String,
) -> Result<NativePreferenceUpdate<ViewerPreferences>, String> {
  let mut guard = state.gate.lock().await;
  let revision = next_revision(*guard, &write_id)?;
  let path = state.paths()?.viewer_dir().join("preferences.json");
  let saved = tauri::async_runtime::spawn_blocking(move || device_preferences::save_viewer(&path, preferences))
    .await
    .map_err(|error| format!("Viewer preferences save failed: {error}"))??;
  *guard = revision;
  let update = NativePreferenceUpdate {
    revision,
    write_id,
    preferences: saved,
  };
  app
    .emit("viewer-preferences-changed", &update)
    .map_err(|error| error.to_string())?;
  Ok(update)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn write_machine_preferences(
  app: AppHandle,
  state: State<'_, DevicePreferences>,
  preferences: MachinePreferences,
  write_id: String,
) -> Result<NativePreferenceUpdate<MachinePreferences>, String> {
  let mut guard = state.gate.lock().await;
  let revision = next_revision(*guard, &write_id)?;
  let path = state.paths()?.viewer_dir().join("machines.json");
  let saved = tauri::async_runtime::spawn_blocking(move || device_preferences::save_machines(&path, preferences))
    .await
    .map_err(|error| format!("Machine preferences save failed: {error}"))??;
  *guard = revision;
  let update = NativePreferenceUpdate {
    revision,
    write_id,
    preferences: saved,
  };
  app
    .emit("machine-preferences-changed", &update)
    .map_err(|error| error.to_string())?;
  Ok(update)
}
