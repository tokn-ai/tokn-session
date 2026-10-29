//! Remote-only app shell. Mobile never initializes local histories or hosting.
use crate::{commands, device_preferences::DevicePreferences, native_remote::NativeRemote};
use tauri::Manager;

#[tauri::mobile_entry_point]
pub fn run() {
  tauri::Builder::default()
    .setup(|app| {
      // The OS application sandbox, rather than a desktop HOME discovery, owns
      // device identity and preferences on both Android and iOS.
      let directory = app.path().app_data_dir()?;
      let paths = tokn_session_storage::SessionPaths::from_home(&directory);
      app.manage(NativeRemote::new(paths.clone(), directory.join("hub-client")));
      app.manage(DevicePreferences::new(paths));
      tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
        .title("Tokn Sessions")
        .initialization_script("Object.defineProperty(window, '__TOKN_MOBILE__', { value: true });")
        .build()?;
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      commands::preferences::initialize_device_preferences,
      commands::preferences::write_viewer_preferences,
      commands::preferences::write_machine_preferences,
      commands::hub::hub_client_status,
      commands::hub::hub_client_resolve,
      commands::hub::hub_client_remember_metadata,
      commands::hub::hub_client_pair,
      commands::hub::hub_client_open,
      commands::hub::hub_client_request,
      commands::hub::hub_client_listen,
      commands::hub::hub_client_close,
      commands::hub::hub_client_forget,
    ])
    .build(tauri::generate_context!())
    .expect("error while building Tokn mobile viewer")
    .run(|app, event| {
      if matches!(event, tauri::RunEvent::Exit) {
        tauri::async_runtime::block_on(app.state::<NativeRemote>().shutdown());
      }
    });
}
