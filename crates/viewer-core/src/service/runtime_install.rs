//! Explicit host software management. Transport adapters enforce host runtime
//! administration permission; session-scoped services never reach the installer.
use super::*;
use crate::runtime_install::*;

impl ViewerService {
  pub async fn get_agent_installations(&self) -> Result<RuntimeInstallInventory, String> {
    self.check_managed_workspace()?;
    self.installations.inventory().await
  }

  pub async fn start_agent_installation(&self, request: RuntimeInstallRequest) -> Result<RuntimeInstallJob, String> {
    self.check_managed_workspace()?;
    self.installations.start(request).await
  }

  pub async fn cancel_agent_installation(
    &self,
    request: RuntimeInstallCancelRequest,
  ) -> Result<RuntimeInstallJob, String> {
    self.check_managed_workspace()?;
    self.installations.cancel(request).await
  }

  pub async fn remove_agent_installation(
    &self,
    request: RuntimeInstallRemoveRequest,
  ) -> Result<RuntimeInstallInventory, String> {
    self.check_managed_workspace()?;
    let snapshot = self.installations.remove(request).await?;
    self.sessions.runtimes.changed();
    Ok(snapshot)
  }
}
