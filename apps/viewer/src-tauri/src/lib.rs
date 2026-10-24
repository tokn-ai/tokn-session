mod commands;
mod device_preferences;
mod external_host;
mod hub_passkey;
mod local_host;
mod local_viewer;
mod native_remote;
mod relay_settings;
mod runtime_management;
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
      let paths = tokn_session_storage::SessionPaths::discover()?;
      app.manage(local_host::LocalHost::unified(paths.clone()));
      app.manage(native_remote::NativeRemote::new(
        paths,
        app.path().app_config_dir()?.join("hub-client"),
      ));
      app.manage(device_preferences::DevicePreferences::default());
      app.manage(hub_passkey::PasskeyCallbacks::default());
      app.manage(runtime_management::RuntimeRestartBroker::default());
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      commands::local::initialize_local_viewer,
      commands::preferences::initialize_device_preferences,
      commands::preferences::write_viewer_preferences,
      commands::preferences::write_machine_preferences,
      commands::host::local_host_status,
      commands::host::local_host_start,
      commands::host::local_host_stop,
      commands::host::local_host_stop_external,
      commands::host::local_host_remove_external,
      commands::host::local_host_pairing,
      commands::host::get_rpc_runtime_access,
      commands::host::set_rpc_runtime_access,
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
      commands::projects::list_registered_projects,
      commands::projects::register_project,
      commands::projects::create_project_session,
      commands::projects::get_managed_session,
      commands::projects::stop_managed_session,
      commands::projects::resume_managed_session,
      commands::projects::respond_managed_session_dialog,
      commands::sessions::list_session_children,
      commands::background::subscribe_background_updates,
      commands::background::renew_background_subscriptions,
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
      commands::images::load_session_image,
      commands::images::upload_image_chunk,
      commands::images::release_image_upload,
      commands::models::get_session_model_status,
      commands::models::list_session_models,
      commands::input::get_session_input_status,
      commands::input::submit_session_input,
      commands::input::get_session_question_status,
      commands::input::submit_session_question_answers,
      commands::session_control::get_session_control,
      commands::session_control::set_session_control,
      commands::session_control::cancel_session_turn,
      commands::relay::get_relay_status,
      commands::settings::get_machine_settings,
      commands::rpc_runtimes::get_rpc_runtime_profiles,
      commands::rpc_runtimes::validate_rpc_runtime_profile,
      commands::rpc_runtimes::save_rpc_runtime_profile,
      commands::rpc_runtimes::delete_rpc_runtime_profile,
      commands::rpc_runtimes::list_rpc_runtimes,
      commands::rpc_runtimes::start_rpc_runtime,
      commands::rpc_runtimes::close_rpc_runtime,
      commands::runtimes::list_viewer_runtimes,
      commands::runtimes::restart_viewer_runtime,
      commands::relay::configure_relay,
      commands::translation::get_translation_status,
      commands::translation::translate_text,
      commands::translation::cancel_translation,
      commands::translation::open_translation_settings,
    ])
    .build(tauri::generate_context!())
    .expect("error while building tokn session viewer")
    .run(|app, event| {
      if matches!(event, tauri::RunEvent::Exit) {
        app.state::<runtime_management::RuntimeRestartBroker>().close();
        tauri::async_runtime::block_on(app.state::<local_host::LocalHost>().shutdown(app));
        tauri::async_runtime::block_on(app.state::<local_viewer::LocalViewer>().shutdown());
        tauri::async_runtime::block_on(app.state::<native_remote::NativeRemote>().shutdown());
        tauri::async_runtime::block_on(app.state::<hub_passkey::PasskeyCallbacks>().cancel_all());
      }
    });
}
