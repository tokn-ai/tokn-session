//! Native Pi workflows for the shared managed-session supervisor.
mod actor;
mod catalog;
mod protocol;

use std::{
  ffi::OsString,
  path::{Path, PathBuf},
  sync::Arc,
};
use tokn_interaction_core::{AdapterFuture, Provider, managed_sessions::*};

/// Pi-specific worker factory. All durable lifecycle and admission ownership
/// is provided by the common managed-session manager.
#[derive(Clone)]
pub struct PiSessionDriver {
  launch: Arc<dyn PiLaunchSource>,
}

/// Host-resolved launcher prefix and an optional resource whose lifetime must
/// cover the child process (for example, an installation's shared use lock).
pub struct PiLaunchConfiguration {
  pub executable: PathBuf,
  pub arguments: Vec<OsString>,
  pub lifetime: Option<Arc<dyn Send + Sync>>,
}

pub trait PiLaunchSource: Send + Sync {
  fn resolve(&self, working_directory: &Path, expected_plan_id: Option<&str>) -> Result<PiLaunchConfiguration, String>;
  /// Retain process resources until the owning host shuts down when native
  /// cleanup cannot prove the process exited. A repeated close is not proof.
  fn retain_uncertain(&self, lifetime: Arc<dyn Send + Sync>);
}

struct FixedLaunch(PathBuf);
impl PiLaunchSource for FixedLaunch {
  fn resolve(
    &self,
    _working_directory: &Path,
    _expected_plan_id: Option<&str>,
  ) -> Result<PiLaunchConfiguration, String> {
    Ok(PiLaunchConfiguration {
      executable: self.0.clone(),
      arguments: Vec::new(),
      lifetime: None,
    })
  }
  fn retain_uncertain(&self, _lifetime: Arc<dyn Send + Sync>) {}
}

impl PiSessionDriver {
  pub fn new(executable: impl Into<PathBuf>) -> Self {
    Self {
      launch: Arc::new(FixedLaunch(executable.into())),
    }
  }

  pub fn with_launch_source(launch: Arc<dyn PiLaunchSource>) -> Self {
    Self { launch }
  }
}

impl ManagedSessionDriver for PiSessionDriver {
  fn provider(&self) -> Provider {
    Provider::Pi
  }

  fn label(&self) -> &str {
    "Pi"
  }

  fn model_capabilities(&self) -> ManagedModelCapabilities {
    ManagedModelCapabilities {
      updates_default: true,
      supports_model_input: true,
    }
  }

  fn spawn(&self, worker: ManagedWorker) -> AdapterFuture<'static, ()> {
    Box::pin(actor::run(self.launch.clone(), worker))
  }

  fn creation_models(
    &self,
    working_directory: PathBuf,
  ) -> AdapterFuture<'static, Result<Vec<tokn_interaction_core::ModelChoice>, String>> {
    let launch = self.launch.clone();
    Box::pin(catalog::models(launch, working_directory))
  }

  fn validate_history(
    &self,
    path: &Path,
    session_id: &str,
    folder: &Path,
  ) -> Result<Option<ManagedHistoryIdentity>, String> {
    Ok(
      tokn_session_pi::verify_session_identity(path, session_id, folder)?.map(|identity| ManagedHistoryIdentity {
        session_file: identity.session_file,
        session_id: identity.session_id,
        provider: Provider::Pi,
      }),
    )
  }
}

#[cfg(test)]
mod tests;
