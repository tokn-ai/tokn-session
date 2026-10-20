use super::*;
use crate::runtimes::{RuntimeExecutionKind, RuntimeInfo, RuntimeKind, RuntimeOwnership};

impl ViewerRelay {
  pub fn runtime_info(&self) -> RuntimeInfo {
    let state = self.state.lock().unwrap();
    let external = state.settings.mode == RelayMode::External;
    let embedded = self.index.is_some() || state.settings.mode == RelayMode::Local;
    let reason = if self.closed.load(Ordering::Acquire) {
      Some("The session reader is shutting down".into())
    } else if external {
      Some("This Relay is externally managed. Reconnect through the reader settings.".into())
    } else if self.index.is_none() && state.settings.mode == RelayMode::Local {
      Some("Local history mode has no live reader worker to restart".into())
    } else {
      None
    };
    RuntimeInfo {
      runtime_id: "reader".into(),
      instance_id: state.instance_id.clone(),
      name: "Session reader / Relay".into(),
      kind: RuntimeKind::Reader,
      execution_kind: if external {
        RuntimeExecutionKind::External
      } else if embedded {
        RuntimeExecutionKind::Embedded
      } else {
        RuntimeExecutionKind::Subprocess
      },
      ownership: if external {
        RuntimeOwnership::External
      } else {
        RuntimeOwnership::Viewer
      },
      status: if self.closed.load(Ordering::Acquire) {
        "stopped".into()
      } else if state.error.is_some() && state.phase == "live" {
        "reconnecting".into()
      } else {
        state.phase.clone()
      },
      pid: if external { None } else { state.managed_pid },
      started_at_ms: if external { None } else { state.started_at_ms },
      session_key: None,
      session_id: None,
      project_id: None,
      working_directory: None,
      endpoint: if external {
        Some(state.settings.endpoint.clone())
      } else {
        None
      },
      detail: Some(
        if external {
          "Configured external session source"
        } else if embedded {
          "Session snapshots run inside the viewer; provider adapters share this reader"
        } else {
          "Bundled Relay subprocess owned by the viewer"
        }
        .into(),
      ),
      last_error: state.error.clone(),
      can_restart: reason.is_none(),
      restart_disabled_reason: reason,
    }
  }

  pub async fn restart_runtime(self: &Arc<Self>, expected_instance_id: &str) -> Result<(), String> {
    let _operation = self.configure_lock.lock().await;
    let info = self.runtime_info();
    if info.instance_id != expected_instance_id {
      return Err("The session reader changed. Refresh runtimes before restarting it.".into());
    }
    if !info.can_restart {
      return Err(
        info
          .restart_disabled_reason
          .unwrap_or_else(|| "This reader cannot be restarted".into()),
      );
    }
    let settings = self.status().settings;
    self.configure_inner(settings, true, Some(expected_instance_id))?;
    // The supervisor's ownership lock prevents overlapping old and new children.
    // Do not take it here: the replacement holds it throughout its lifetime.
    Ok(())
  }
}
