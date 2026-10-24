use super::*;
use crate::runtimes::{RuntimeInfo, RuntimeRestartRequest};

impl ViewerService {
  pub async fn list_runtimes(&self) -> Vec<RuntimeInfo> {
    if self.session_scope.is_some() {
      return Vec::new();
    }
    let mut runtimes = vec![self.relay.runtime_info()];
    let mut pi = self.sessions.managed.list_runtimes().await;
    if let Err(reason) = self.check_managed_workspace() {
      for runtime in &mut pi {
        runtime.can_restart = false;
        runtime.restart_disabled_reason = Some(reason.clone());
      }
    }
    runtimes.extend(pi);
    runtimes
  }

  pub async fn restart_runtime(&self, request: RuntimeRestartRequest) -> Result<(), String> {
    if self.session_scope.is_some() {
      return Err("Runtime management is unavailable in a session share".into());
    }
    uuid::Uuid::parse_str(&request.request_id).map_err(|_| "Invalid runtime restart request ID")?;
    if request.runtime_id == "reader" {
      self.relay.restart_runtime(&request.expected_instance_id).await
    } else if request.runtime_id.starts_with("pi:") {
      self.check_managed_workspace()?;
      self.sessions.managed.restart_runtime(&request).await
    } else {
      Err("Unknown viewer runtime".into())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn external_reader_cannot_restart_a_local_pi_worker() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    service
      .relay
      .configure(crate::relay::RelaySettings {
        mode: crate::relay::RelayMode::External,
        endpoint: "tcp://127.0.0.1:1".into(),
        ..Default::default()
      })
      .unwrap();
    let result = service
      .restart_runtime(RuntimeRestartRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        runtime_id: format!("pi:{}", uuid::Uuid::new_v4()),
        expected_instance_id: "untrusted-instance".into(),
      })
      .await;
    assert!(result.unwrap_err().contains("local connection"));
    service.relay.shutdown().await;
  }
}
