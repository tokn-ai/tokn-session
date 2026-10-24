//! Machine-facing RPC configuration and runtime composition; transport auth is
//! enforced by the desktop/HTTP/Hub command adapters before these methods run.
use super::*;
use crate::{rpc_profiles::*, rpc_runtime::*};

impl ViewerService {
  pub async fn get_rpc_runtime_profiles(&self) -> Result<RpcRuntimeProfilesSnapshot, String> {
    self.check_managed_workspace()?;
    let profiles = self.sessions.runtimes.profiles()?;
    tokio::task::spawn_blocking(move || profiles.snapshot())
      .await
      .map_err(|_| "Could not read runtime profiles")?
  }
  pub async fn validate_rpc_runtime_profile(
    &self,
    request: RpcRuntimeProfileInput,
  ) -> Result<RpcProfileValidation, String> {
    self.check_managed_workspace()?;
    let profiles = self.sessions.runtimes.profiles()?;
    tokio::task::spawn_blocking(move || profiles.validate(request))
      .await
      .map_err(|_| "Could not validate runtime profile")?
  }
  pub async fn save_rpc_runtime_profile(
    &self,
    request: RpcSaveProfileRequest,
  ) -> Result<RpcRuntimeProfilesSnapshot, String> {
    self.check_managed_workspace()?;
    let profiles = self.sessions.runtimes.profiles()?;
    let snapshot = tokio::task::spawn_blocking(move || profiles.save(request))
      .await
      .map_err(|_| "Could not save runtime profile")??;
    self.sessions.runtimes.changed();
    Ok(snapshot)
  }
  pub async fn delete_rpc_runtime_profile(
    &self,
    request: RpcRemoveProfileRequest,
  ) -> Result<RpcRuntimeProfilesSnapshot, String> {
    self.check_managed_workspace()?;
    self.sessions.runtimes.remove_profile(request).await
  }
  pub async fn list_rpc_runtimes(&self) -> Result<RpcRuntimeInventory, String> {
    self.check_managed_workspace()?;
    Ok(self.sessions.runtimes.inventory())
  }
  pub async fn start_rpc_runtime(&self, request: RpcStartRuntimeRequest) -> Result<RpcRuntimeInfo, String> {
    self.check_managed_workspace()?;
    self.sessions.runtimes.start(request).await
  }
  pub async fn close_rpc_runtime(&self, request: RpcCloseRuntimeRequest) -> Result<RpcCloseRuntimeResponse, String> {
    self.check_managed_workspace()?;
    self.sessions.runtimes.close(request).await
  }
  /// Internal composition API for a host-verified history selection. Session
  /// controls will consume these bindings without replacing historical readers.
  pub async fn bind_rpc_session(&self, request: RpcBindSessionRequest) -> Result<RpcSessionBinding, String> {
    self.check_managed_workspace()?;
    uuid::Uuid::parse_str(&request.request_id).map_err(|_| "Invalid binding request ID")?;
    let service = self.clone();
    let key = encode_session_key(&decode_session_key(&request.session_key)?)?;
    let canonical_key = key.clone();
    let (provider, native_id) = tokio::task::spawn_blocking(move || service.verify_rpc_history(&key))
      .await
      .map_err(|_| "Could not verify session history")??;
    self
      .sessions
      .runtimes
      .bind(
        request.request_id,
        canonical_key,
        request.profile_id,
        request.expected_instance_id,
        native_id,
        provider,
      )
      .await
  }
  pub async fn get_rpc_session_binding(
    &self,
    request: RpcSessionBindingRequest,
  ) -> Result<Option<RpcSessionBinding>, String> {
    self.check_managed_workspace()?;
    self.validate_session_key(&request.session_key)?;
    let key = encode_session_key(&decode_session_key(&request.session_key)?)?;
    Ok(self.sessions.runtimes.binding(&key))
  }
  fn verify_rpc_history(&self, key: &str) -> Result<(tokn_interaction_core::Provider, String), String> {
    let locator = decode_session_key(key)?;
    let provider = match locator.provider {
      ViewerProvider::Codex => tokn_interaction_core::Provider::Codex,
      ViewerProvider::Pi => tokn_interaction_core::Provider::Pi,
      ViewerProvider::Dsh => tokn_interaction_core::Provider::Dsh,
      ViewerProvider::OpenCode => {
        return Err(
          "OpenCode V1 history cannot bind to a V2 runtime; V2 conversation reading is a separate integration".into(),
        );
      }
      _ => return Err("This provider has no RPC runtime adapter".into()),
    };
    let catalog = self
      .indexed_session_inventory(locator.provider)?
      .ok_or("Session catalog is not ready")?;
    let header = catalog
      .headers
      .into_iter()
      .find(|header| locator_for_header(locator.provider, header) == locator)
      .ok_or("Session is not in this machine's catalog")?;
    let current = self
      .repository
      .session_header_at_path(locator.provider, &header.path)
      .map_err(|_| "Session history is unavailable")?;
    if current.id != locator.session_id {
      return Err("Session history identity changed; refresh the catalog".into());
    }
    if locator.provider == ViewerProvider::Codex
      && (current.parent_session_id.is_some()
        || current
          .agent_path
          .as_deref()
          .is_some_and(|path| path != "/root" && !path.is_empty()))
    {
      return Err("Control Codex subagents through their parent thread".into());
    }
    Ok((provider, current.id))
  }
}
