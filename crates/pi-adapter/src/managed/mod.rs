//! Native Pi workflows for the shared managed-session supervisor.
mod actor;
mod catalog;
mod protocol;

use std::path::{Path, PathBuf};
use tokn_interaction_core::{AdapterFuture, Provider, managed_sessions::*};

/// Pi-specific worker factory. All durable lifecycle and admission ownership
/// is provided by the common managed-session manager.
#[derive(Clone, Debug)]
pub struct PiSessionDriver {
  executable: PathBuf,
}

impl PiSessionDriver {
  pub fn new(executable: impl Into<PathBuf>) -> Self {
    Self {
      executable: executable.into(),
    }
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
    let executable = self.executable.clone();
    Box::pin(actor::run(executable, worker))
  }

  fn creation_models(
    &self,
    working_directory: PathBuf,
  ) -> AdapterFuture<'static, Result<Vec<tokn_interaction_core::ModelChoice>, String>> {
    Box::pin(catalog::models(self.executable.clone(), working_directory))
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
