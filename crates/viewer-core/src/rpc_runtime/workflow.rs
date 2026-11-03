//! Explicit-input workflow over saved host runtime configuration. Inspection
//! never calls this activation path.
use super::*;
use std::path::Path;

impl RpcRuntimeManager {
  /// Host configuration and executable metadata only: no process, network, or
  /// native session is activated by checking whether Send can connect.
  pub(crate) async fn activation_ready(&self, provider: Provider, cwd: &Path) -> Result<(), String> {
    self.ensure_open()?;
    if provider == Provider::Pi {
      return Err("Pi continuation requires its verified owned-session runtime".into());
    }
    if !cwd.is_absolute() {
      return Err("This session has no verified working directory for RPC activation".into());
    }
    let manager = self.clone();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || manager.resolve_plan(provider, &cwd, None).map(|_| ()))
      .await
      .map_err(|_| "Could not check runtime readiness on this machine")?
  }

  pub(crate) async fn ensure_session(
    &self,
    request_id: &str,
    session_key: &str,
    provider: Provider,
    native_id: &str,
    cwd: &Path,
  ) -> Result<RpcSessionBinding, String> {
    self
      .0
      .runtimes
      .ensure_session(request_id, session_key, provider, native_id, cwd)
      .await
      .map(Into::into)
  }

  pub(crate) async fn ensure_verified_session(
    &self,
    request_id: &str,
    session_key: &str,
    plan: tokn_interaction_core::runtime_plan::RuntimePlan,
    descriptor: tokn_interaction_core::runtime_plan::VerifiedSessionDescriptor,
  ) -> Result<RpcSessionBinding, String> {
    let manager = self.clone();
    let request_id = request_id.to_owned();
    let session_key = session_key.to_owned();
    // Core activation is cancellation-safe. Keep its installation admission
    // lease in an equally owned task until the child owns its lifetime lease.
    tokio::spawn(async move {
      let checker = manager.clone();
      let selected = plan.clone();
      let _installation = tokio::task::spawn_blocking(move || checker.hold_plan_installation(&selected))
        .await
        .map_err(|_| "Could not verify the selected agent installation")??;
      manager
        .0
        .runtimes
        .ensure_session_with_plan(&request_id, &session_key, plan, descriptor)
        .await
        .map(Into::into)
    })
    .await
    .map_err(|_| "Agent activation was interrupted".to_owned())?
  }

  pub(crate) async fn execute_binding(
    &self,
    binding: &RpcSessionBinding,
    request_id: String,
    command: Command,
  ) -> Result<Reply, InteractionError> {
    self
      .0
      .runtimes
      .execute_binding(&binding.into(), request_id, command)
      .await
  }

  pub(crate) fn binding_snapshot(&self, key: &str) -> Option<RuntimeSnapshot> {
    self.0.runtimes.binding_snapshot(key)
  }

  pub(crate) async fn release_binding(&self, key: &str) -> Result<(), String> {
    self.0.runtimes.release_binding(key).await
  }
}
