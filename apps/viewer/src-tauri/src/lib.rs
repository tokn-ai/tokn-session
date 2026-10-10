mod commands;
mod hub_passkey;
mod local_host;
mod local_viewer;
mod translation;
use tauri::Manager;
pub use tokn_session_relay::stdio as relay_child;
pub use tokn_viewer_core::{model, relay, service};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_opener::init())
    .setup(|app| {
      app.manage(local_viewer::LocalViewer::default());
      app.manage(local_host::LocalHost::new(
        dirs::home_dir()
          .ok_or("Cannot resolve host state directory")?
          .join(".tokn/hub"),
      ));
      app.manage(tokn_hub_remote::RemoteManager::new(
        app.path().app_config_dir()?.join("hub-client"),
      ));
      app.manage(hub_passkey::PasskeyCallbacks::default());
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      commands::local::initialize_local_viewer,
      commands::host::local_host_status,
      commands::host::local_host_start,
      commands::host::local_host_stop,
      commands::host::local_host_pairing,
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
      commands::events::subscribe_session,
      commands::events::load_session_backward,
      commands::events::load_session_details,
      commands::events::inspect_session_event,
      commands::events::renew_session_subscriptions,
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
        tauri::async_runtime::block_on(app.state::<local_host::LocalHost>().shutdown(app));
        tauri::async_runtime::block_on(app.state::<local_viewer::LocalViewer>().shutdown());
        tauri::async_runtime::block_on(app.state::<tokn_hub_remote::RemoteManager>().close_all());
        tauri::async_runtime::block_on(app.state::<hub_passkey::PasskeyCallbacks>().cancel_all());
      }
    });
}
