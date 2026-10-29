// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
  #[cfg(desktop)]
  tokn_session_viewer::relay_child::run_if_requested();
  tokn_session_viewer::run()
}
