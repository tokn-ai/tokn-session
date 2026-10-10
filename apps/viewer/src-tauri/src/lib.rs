mod commands;
mod hub_passkey;
mod translation;
use tauri::{Emitter, Manager};
pub use tokn_session_relay::stdio as relay_child;
use tokn_viewer_core::{
  ViewerService,
  runtime::{ViewerRuntime, session_index_path},
};
pub use tokn_viewer_core::{model, relay, service};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_opener::init())
    .setup(|app| {
      let path = session_index_path()?;
      if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
      }
      let service = ViewerService::native(path)?;
      let runtime = tauri::async_runtime::block_on(async { ViewerRuntime::start(service.clone()) });
      let mut events = runtime.events.subscribe();
      let handle = app.handle().clone();
      tauri::async_runtime::spawn(async move {
        loop {
          match events.recv().await {
            Ok(event) => {
              let _ = handle.emit(&event.event, event.payload);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
              let _ = handle.emit(
                "relay-changed",
                relay::RelayChange {
                  session_key: None,
                  reset: true,
                },
              );
              let _ = handle.emit(
                "session-index-changed",
                serde_json::json!({"attention_session_keys":[],"updated_session_keys":[]}),
              );
            }
            Err(_) => break,
          }
        }
      });
      let settings = app
        .path()
        .app_config_dir()
        .map_err(|e| e.to_string())
        .and_then(|path| relay::read_settings(&path.join("relay.json")));
      tauri::async_runtime::block_on(async {
        match settings {
          Ok(settings) => {
            if let Err(error) = service.relay.configure(settings) {
              service.relay.configuration_failed(error);
            }
          }
          Err(error) => service.relay.configuration_failed(error),
        }
      });
      app.manage(service);
      app.manage(runtime);
      app.manage(tokn_hub_remote::RemoteManager::new(
        app.path().app_config_dir()?.join("hub-client"),
      ));
      app.manage(hub_passkey::PasskeyCallbacks::default());
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      commands::hub::hub_client_status,
      commands::hub::hub_client_resolve,
      commands::hub::hub_client_remember_metadata,
      commands::hub::hub_client_pair,
      commands::hub::hub_client_open,
      commands::hub::hub_client_request,
      commands::hub::hub_client_listen,
      commands::hub::hub_client_close,
      commands::hub::hub_client_forget,
      commands::hub::hub_client_auth_start,
      commands::hub::hub_client_auth_finish,
      commands::hub::hub_client_auth_cancel,
      commands::hub::hub_client_passkey_credential,
      commands::sessions::list_sessions,
      commands::sessions::list_session_children,
      commands::events::load_event_page,
      commands::events::load_session_updates,
      commands::events::update_session_view,
      commands::events::load_event_detail,
      commands::events::load_trajectory_event_page,
      commands::events::acknowledge_session_attention,
      commands::indexing::get_session_index_progress,
      commands::indexing::retry_session_index,
      commands::input::get_session_input_status,
      commands::input::submit_session_input,
      commands::relay::get_relay_status,
      commands::relay::configure_relay,
      commands::translation::get_translation_status,
      commands::translation::translate_text,
      commands::translation::cancel_translation,
    ])
    .build(tauri::generate_context!())
    .expect("error while building tokn session viewer")
    .run(|app, event| {
      if matches!(event, tauri::RunEvent::Exit) {
        tauri::async_runtime::block_on(app.state::<ViewerService>().relay.shutdown());
        tauri::async_runtime::block_on(app.state::<tokn_hub_remote::RemoteManager>().close_all());
        tauri::async_runtime::block_on(app.state::<hub_passkey::PasskeyCallbacks>().cancel_all());
      }
    });
}
