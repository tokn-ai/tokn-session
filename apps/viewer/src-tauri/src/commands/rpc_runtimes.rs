//! Host-owned runtime profiles and lifecycle. Remote calls use the same service
//! through the authenticated HTTP/Hub adapter, never this local state directly.
use crate::local_viewer::LocalViewer;
use tauri::{AppHandle, State};
use tokn_viewer_core::{rpc_profiles::*, rpc_runtime::*, runtime_install::*};

#[tauri::command]
pub async fn get_agent_installations(
  app: AppHandle,
  state: State<'_, LocalViewer>,
) -> Result<RuntimeInstallInventory, String> {
  state.service(&app).await?.get_agent_installations().await
}

#[tauri::command]
pub async fn start_agent_installation(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RuntimeInstallRequest,
) -> Result<RuntimeInstallJob, String> {
  state.service(&app).await?.start_agent_installation(request).await
}

#[tauri::command]
pub async fn cancel_agent_installation(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RuntimeInstallCancelRequest,
) -> Result<RuntimeInstallJob, String> {
  state.service(&app).await?.cancel_agent_installation(request).await
}

#[tauri::command]
pub async fn remove_agent_installation(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RuntimeInstallRemoveRequest,
) -> Result<RuntimeInstallInventory, String> {
  state.service(&app).await?.remove_agent_installation(request).await
}

#[tauri::command]
pub async fn get_rpc_runtime_profiles(
  app: AppHandle,
  state: State<'_, LocalViewer>,
) -> Result<RpcRuntimeProfilesSnapshot, String> {
  state.service(&app).await?.get_rpc_runtime_profiles().await
}

#[tauri::command]
pub async fn validate_rpc_runtime_profile(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RpcRuntimeProfileInput,
) -> Result<RpcProfileValidation, String> {
  state.service(&app).await?.validate_rpc_runtime_profile(request).await
}

#[tauri::command]
pub async fn save_rpc_runtime_profile(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RpcSaveProfileRequest,
) -> Result<RpcRuntimeProfilesSnapshot, String> {
  state.service(&app).await?.save_rpc_runtime_profile(request).await
}

#[tauri::command]
pub async fn delete_rpc_runtime_profile(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RpcRemoveProfileRequest,
) -> Result<RpcRuntimeProfilesSnapshot, String> {
  state.service(&app).await?.delete_rpc_runtime_profile(request).await
}

#[tauri::command]
pub async fn list_rpc_runtimes(app: AppHandle, state: State<'_, LocalViewer>) -> Result<RpcRuntimeInventory, String> {
  state.service(&app).await?.list_rpc_runtimes().await
}

#[tauri::command]
pub async fn start_rpc_runtime(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RpcStartRuntimeRequest,
) -> Result<RpcRuntimeInfo, String> {
  state.service(&app).await?.start_rpc_runtime(request).await
}

#[tauri::command]
pub async fn close_rpc_runtime(
  app: AppHandle,
  state: State<'_, LocalViewer>,
  request: RpcCloseRuntimeRequest,
) -> Result<RpcCloseRuntimeResponse, String> {
  state.service(&app).await?.close_rpc_runtime(request).await
}
