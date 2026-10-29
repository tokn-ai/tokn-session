//! Host configuration, adapter factories and viewer DTO mapping for shared runtimes.
mod factory;
#[cfg(test)]
pub(crate) mod tests;
mod workflow;

use crate::rpc_profiles::*;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::{
  collections::BTreeMap,
  path::PathBuf,
  sync::{Arc, Mutex},
};
use tokio::sync::broadcast;
use tokn_interaction_core::runtime as core_runtime;
use tokn_interaction_core::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcStartRuntimeRequest {
  pub request_id: String,
  pub profile_id: String,
  pub expected_revision: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcCloseRuntimeRequest {
  pub request_id: String,
  pub runtime_id: String,
  pub expected_instance_id: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RpcCloseRuntimeResponse {
  pub runtime_id: String,
  pub instance_id: String,
  pub closed: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct RpcRuntimeInfo {
  pub runtime_id: String,
  pub instance_id: String,
  pub profile_id: String,
  pub profile_revision: u64,
  pub name: String,
  pub provider: Provider,
  pub ownership: Ownership,
  pub status: String,
  pub capabilities: Capabilities,
  pub session_count: usize,
  pub last_error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RpcRuntimeInventory {
  pub version: u32,
  pub runtimes: Vec<RpcRuntimeInfo>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcBindSessionRequest {
  pub request_id: String,
  pub session_key: String,
  pub profile_id: String,
  pub expected_instance_id: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RpcSessionBindingRequest {
  pub session_key: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RpcSessionBinding {
  pub session_key: String,
  pub profile_id: String,
  pub target: SessionTarget,
  pub capabilities: Capabilities,
}

#[derive(Clone)]
pub(crate) struct RpcRuntimeManager(Arc<Inner>);
struct Inner {
  profiles: Arc<RpcProfileStore>,
  transient_profiles: Arc<Mutex<BTreeMap<String, RpcResolvedProfile>>>,
  runtimes: core_runtime::RuntimeCoordinator<Host>,
  #[cfg(test)]
  interactions: InteractionService,
}

#[derive(Clone)]
struct Host {
  profiles: Arc<RpcProfileStore>,
  transient_profiles: Arc<Mutex<BTreeMap<String, RpcResolvedProfile>>>,
}

impl RpcRuntimeManager {
  pub(crate) fn creation_selection(
    &self,
    provider: Provider,
    cwd: &std::path::Path,
    selected: Option<&core_runtime::RuntimeSelection>,
  ) -> Result<Option<core_runtime::RuntimeSelection>, String> {
    let profiles = self.profiles()?;
    let snapshot = profiles.snapshot()?;
    let selection = if let Some(selected) = selected {
      Some(selected.clone())
    } else {
      workflow::select_profile(&snapshot, provider, cwd)?.map(|profile| core_runtime::RuntimeSelection {
        profile_id: profile.profile_id.clone(),
        expected_revision: snapshot.revision,
      })
    };
    if let Some(selection) = &selection {
      let transient = self
        .0
        .transient_profiles
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&selection.profile_id)
        .cloned();
      let resolved = if let Some(profile) = transient {
        let canonical_cwd = std::fs::canonicalize(cwd).map_err(|_| "The project folder is unavailable")?;
        if provider != Provider::Codex
          || selection.expected_revision != 0
          || profile.profile.working_directory.as_deref().map(std::path::Path::new) != Some(canonical_cwd.as_path())
        {
          return Err("The selected built-in runtime belongs to another project".into());
        }
        profile
      } else {
        profiles.resolve_at_revision(&selection.profile_id, selection.expected_revision)?
      };
      if resolved.profile.provider != provider {
        return Err("The chosen runtime belongs to another agent".into());
      }
      resolved.validate_launch_ready()?;
      crate::service::validate_creation_profile(&resolved, cwd)?;
    } else {
      workflow::builtin_codex(cwd)?;
    }
    Ok(selection)
  }

  /// Resolve the exact runtime behind a draft catalog, including built-in Codex.
  /// This reads configuration only; ordinary default creation keeps its legacy
  /// unpinned intent while an explicit model choice pins this returned identity.
  pub(crate) fn creation_model_selection(
    &self,
    provider: Provider,
    cwd: &std::path::Path,
    selected: Option<&core_runtime::RuntimeSelection>,
  ) -> Result<Option<core_runtime::RuntimeSelection>, String> {
    if let Some(selection) = self.creation_selection(provider, cwd, selected)? {
      return Ok(Some(selection));
    }
    let profile = workflow::builtin_codex(cwd)?;
    self.pin_builtin_creation_profile(profile).map(Some)
  }

  fn pin_builtin_creation_profile(
    &self,
    profile: RpcResolvedProfile,
  ) -> Result<core_runtime::RuntimeSelection, String> {
    let id = profile.profile.profile_id.clone();
    let mut transient = self
      .0
      .transient_profiles
      .lock()
      .unwrap_or_else(|error| error.into_inner());
    if !transient.contains_key(&id) && transient.len() >= 256 {
      return Err(
        "Project runtime configuration limit reached; reopen the app to clear inactive configurations".into(),
      );
    }
    transient.entry(id.clone()).or_insert(profile);
    Ok(core_runtime::RuntimeSelection {
      profile_id: id,
      expected_revision: 0,
    })
  }

  pub(crate) async fn create_native_session(
    &self,
    provider: Provider,
    cwd: PathBuf,
    selection: Option<core_runtime::RuntimeSelection>,
    native_id: Option<String>,
    model: Option<ModelIdentity>,
  ) -> Result<core_runtime::CreatedNativeSession, InteractionError> {
    self
      .0
      .runtimes
      .create_native(provider, cwd, selection, native_id, model)
      .await
  }
  pub(crate) async fn creation_models(
    &self,
    provider: Provider,
    cwd: PathBuf,
    selection: Option<core_runtime::RuntimeSelection>,
  ) -> Result<Vec<ModelChoice>, InteractionError> {
    if provider == Provider::Pi {
      return Err(InteractionError::unsupported(
        "Use the managed Pi driver to discover creation models",
      ));
    }
    self.0.runtimes.creation_models(provider, cwd, selection).await
  }
  pub(crate) async fn execute_created(
    &self,
    created: &core_runtime::CreatedNativeSession,
    request_id: String,
    command: Command,
  ) -> Result<Reply, InteractionError> {
    self.0.runtimes.execute_created(created, request_id, command).await
  }
  pub(crate) async fn adopt_created(
    &self,
    created: &core_runtime::CreatedNativeSession,
    key: String,
  ) -> Result<RpcSessionBinding, InteractionError> {
    self.0.runtimes.adopt_created(created, key).await.map(Into::into)
  }
  pub(crate) async fn adopt_verified_creation(
    &self,
    provider: Provider,
    native_id: &str,
    cwd: PathBuf,
    key: String,
  ) -> Result<Option<core_runtime::CreatedNativeSession>, InteractionError> {
    self
      .0
      .runtimes
      .adopt_verified_creation(provider, native_id, cwd, key)
      .await
  }

  pub(crate) fn new(path: Option<PathBuf>, interactions: InteractionService) -> Self {
    let parent = path.as_ref().and_then(|path| path.parent());
    let lease_directory = parent.map(|path| path.join("rpc-leases"));
    let ownership = crate::session_ownership::SessionOwnership::new(parent.map(|path| path.join("session-leases")));
    let profiles = Arc::new(RpcProfileStore::configured(path));
    let host = Host {
      profiles: profiles.clone(),
      transient_profiles: Arc::new(Mutex::new(BTreeMap::new())),
    };
    Self(Arc::new(Inner {
      profiles,
      transient_profiles: host.transient_profiles.clone(),
      #[cfg(test)]
      interactions: interactions.clone(),
      runtimes: core_runtime::RuntimeCoordinator::new(host, interactions, lease_directory, ownership),
    }))
  }

  pub(crate) fn with_session_ownership(
    mut self,
    ownership: crate::session_ownership::SessionOwnership,
  ) -> Result<Self, String> {
    Arc::get_mut(&mut self.0)
      .ok_or("Session ownership must be configured before sharing the runtime manager")?
      .runtimes
      .with_session_ownership(ownership)?;
    Ok(self)
  }

  pub(crate) fn profiles(&self) -> Result<Arc<RpcProfileStore>, String> {
    Ok(self.0.profiles.clone())
  }

  pub(crate) fn subscribe(&self) -> broadcast::Receiver<()> {
    self.0.runtimes.subscribe()
  }

  pub(crate) fn changed(&self) {
    self.0.runtimes.changed();
  }

  fn ensure_open(&self) -> Result<(), String> {
    self.0.runtimes.ensure_open()
  }

  pub(crate) fn inventory(&self) -> RpcRuntimeInventory {
    let inventory = self.0.runtimes.inventory();
    RpcRuntimeInventory {
      version: inventory.version,
      runtimes: inventory.runtimes.into_iter().map(Into::into).collect(),
    }
  }

  pub(crate) async fn remove_profile(
    &self,
    request: RpcRemoveProfileRequest,
  ) -> Result<RpcRuntimeProfilesSnapshot, String> {
    let profiles = self.profiles()?;
    self
      .0
      .runtimes
      .with_inactive_profile(request.profile_id.clone(), move || profiles.remove(request))
      .await
  }

  pub(crate) async fn start(&self, request: RpcStartRuntimeRequest) -> Result<RpcRuntimeInfo, String> {
    self
      .0
      .runtimes
      .start(core_runtime::StartRuntimeRequest {
        request_id: request.request_id,
        profile_id: request.profile_id,
        expected_revision: request.expected_revision,
      })
      .await
      .map(Into::into)
  }

  pub(crate) async fn close(&self, request: RpcCloseRuntimeRequest) -> Result<RpcCloseRuntimeResponse, String> {
    self
      .0
      .runtimes
      .close(core_runtime::CloseRuntimeRequest {
        request_id: request.request_id,
        runtime_id: request.runtime_id,
        expected_instance_id: request.expected_instance_id,
      })
      .await
      .map(|response| RpcCloseRuntimeResponse {
        runtime_id: response.runtime_id,
        instance_id: response.instance_id,
        closed: response.closed,
      })
  }

  pub(crate) fn binding(&self, key: &str) -> Option<RpcSessionBinding> {
    self.0.runtimes.binding(key).map(Into::into)
  }

  /// Called only with a historical identity verified by the host.
  pub(crate) async fn bind(
    &self,
    request_id: String,
    key: String,
    profile_id: String,
    instance: String,
    native_id: String,
    provider: Provider,
  ) -> Result<RpcSessionBinding, String> {
    self
      .0
      .runtimes
      .bind(request_id, key, profile_id, instance, native_id, provider)
      .await
      .map(Into::into)
  }

  pub(crate) async fn shutdown(&self) {
    self.0.runtimes.shutdown().await;
  }
}

impl From<core_runtime::RuntimeInfo> for RpcRuntimeInfo {
  fn from(info: core_runtime::RuntimeInfo) -> Self {
    Self {
      runtime_id: info.runtime_id,
      instance_id: info.instance_id,
      profile_id: info.profile_id,
      profile_revision: info.profile_revision,
      name: info.name,
      provider: info.provider,
      ownership: info.ownership,
      status: info.status,
      capabilities: info.capabilities,
      session_count: info.session_count,
      last_error: info.last_error,
    }
  }
}

impl From<core_runtime::SessionBinding> for RpcSessionBinding {
  fn from(binding: core_runtime::SessionBinding) -> Self {
    Self {
      session_key: binding.session_key,
      profile_id: binding.profile_id,
      target: binding.target,
      capabilities: binding.capabilities,
    }
  }
}

impl From<&RpcSessionBinding> for core_runtime::SessionBinding {
  fn from(binding: &RpcSessionBinding) -> Self {
    Self {
      session_key: binding.session_key.clone(),
      profile_id: binding.profile_id.clone(),
      target: binding.target.clone(),
      capabilities: binding.capabilities.clone(),
    }
  }
}
