//! Host composition and presentation for shared managed sessions.
//! Native Pi workflows belong to pi-adapter; lifecycle and receipts belong to
//! interaction-core. This module supplies viewer keys and projects only.
use crate::{
  images::ResolvedImage,
  input::{Admission, DeliveryError},
  model::{SessionLocator, ViewerProvider, decode_session_key, encode_session_key},
  models::MessageModelSelection,
  runtimes::{RuntimeExecutionKind, RuntimeInfo, RuntimeKind, RuntimeOwnership, RuntimeRestartRequest},
  session_ownership::SessionOwnership,
  workspace::{
    CreateProjectSessionRequest, ManagedSessionDialogResponse, ManagedSessionRequest, ManagedSessionSnapshot,
    RegisteredProject,
  },
};
use std::{
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};
use tokn_interaction_core::runtime_plan::{
  RuntimeConfigurationIdentity, RuntimePlan, RuntimeReuseScope, RuntimeStrategy,
};
use tokn_interaction_core::{Provider, managed_sessions as shared};

#[derive(Clone)]
pub(crate) struct ManagedSessions {
  manager: shared::ManagedSessions,
  executable: Option<PathBuf>,
}

struct ViewerHost;

impl shared::ManagedSessionHost for ViewerHost {
  fn session_key(&self, identity: &shared::ManagedHistoryIdentity) -> Result<String, String> {
    let provider = match identity.provider {
      Provider::Codex => ViewerProvider::Codex,
      Provider::Pi => ViewerProvider::Pi,
      Provider::OpenCode => ViewerProvider::OpenCode,
      Provider::Dsh => ViewerProvider::Dsh,
    };
    encode_session_key(&SessionLocator {
      version: 1,
      provider,
      session_id: identity.session_id.clone(),
      source_path: identity.session_file.clone(),
    })
  }

  fn keys_equal(&self, left: &str, right: &str) -> bool {
    match (decode_session_key(left), decode_session_key(right)) {
      (Ok(left), Ok(right)) => left == right,
      _ => false,
    }
  }
}

impl std::ops::Deref for ManagedSessions {
  type Target = shared::ManagedSessions;
  fn deref(&self) -> &Self::Target {
    &self.manager
  }
}

impl ManagedSessions {
  pub(crate) fn new_with_session_ownership(
    state_path: Option<PathBuf>,
    ownership: SessionOwnership,
  ) -> Result<Self, String> {
    let executable = std::env::var_os("TOKN_SESSION_PI_EXECUTABLE")
      .map(PathBuf::from)
      .unwrap_or_else(|| "pi".into());
    Self::with_configuration(state_path, executable, ownership)
  }

  fn with_configuration(
    state_path: Option<PathBuf>,
    executable: PathBuf,
    ownership: SessionOwnership,
  ) -> Result<Self, String> {
    // Resolve once for the lifetime of this driver, so its advertised plan and
    // the executable used by the preserved worker are the same configuration.
    let resolved = crate::rpc_runtime::discovery::resolve_executable(
      Some(executable.as_os_str()),
      std::env::var_os("PATH").as_deref(),
      "pi",
    )
    .ok();
    let manager = shared::ManagedSessions::new(
      shared::ManagedSessionConfiguration {
        state_path,
        idle_timeout: Duration::from_secs(5 * 60),
        legacy_provider: Some(Provider::Pi),
        lease_prefix: "managed-pi".into(),
      },
      Arc::new(tokn_pi_adapter::managed::PiSessionDriver::new(
        resolved.clone().unwrap_or(executable),
      )),
      Arc::new(ViewerHost),
      ownership,
    )?;
    Ok(Self {
      manager,
      executable: resolved,
    })
  }

  pub(crate) fn runtime_plan(&self, cwd: &Path) -> Result<RuntimePlan, String> {
    self.manager.available()?;
    let executable = self
      .executable
      .as_ref()
      .ok_or("Pi is unavailable. Install it or set TOKN_SESSION_PI_EXECUTABLE, then restart local sessions.")?;
    crate::rpc_runtime::discovery::resolve_executable(Some(executable.as_os_str()), None, "pi")?;
    let canonical = std::fs::canonicalize(cwd).map_err(|_| "The project folder is unavailable")?;
    if !canonical.is_dir() {
      return Err("The project folder is unavailable".into());
    }
    let plan = RuntimePlan {
      provider: Provider::Pi,
      configuration: RuntimeConfigurationIdentity {
        id: crate::rpc_runtime::discovery::executable_configuration_id(Provider::Pi, &canonical, executable)?,
        revision: 0,
      },
      ownership: tokn_interaction_core::Ownership::Managed,
      reuse_scope: RuntimeReuseScope::Session,
      strategy: RuntimeStrategy::SessionWorker,
      working_directory: cwd.to_owned(),
    };
    plan.validate()?;
    Ok(plan)
  }

  #[cfg(test)]
  pub(crate) fn with_test_executable(state_path: Option<PathBuf>, executable: PathBuf) -> Result<Self, String> {
    let ownership = SessionOwnership::new(
      state_path
        .as_ref()
        .and_then(|path| path.parent())
        .map(|parent| parent.join("session-leases")),
    )?;
    Self::with_configuration(state_path, executable, ownership)
  }

  pub(crate) fn disabled(message: String) -> Self {
    Self {
      manager: shared::ManagedSessions::disabled(message, Provider::Pi),
      executable: None,
    }
  }

  #[cfg(test)]
  pub(crate) async fn create(
    &self,
    request: CreateProjectSessionRequest,
    project: RegisteredProject,
  ) -> Result<ManagedSessionSnapshot, String> {
    self.create_with_images(request, project, Vec::new()).await
  }

  pub(crate) async fn create_with_images(
    &self,
    request: CreateProjectSessionRequest,
    project: RegisteredProject,
    images: Vec<ResolvedImage>,
  ) -> Result<ManagedSessionSnapshot, String> {
    self
      .manager
      .create_with_images(
        request.pi_request(),
        shared::RegisteredProject {
          project_id: project.project_id,
          folder_path: project.folder_path,
          available: project.available,
        },
        images.into_iter().map(image_input).collect(),
      )
      .await
      .map(Into::into)
  }

  pub(crate) fn get(&self, request: &ManagedSessionRequest) -> Result<Option<ManagedSessionSnapshot>, String> {
    self.manager.get(request).map(|snapshot| snapshot.map(Into::into))
  }

  pub(crate) fn creation_receipt(
    &self,
    request: &CreateProjectSessionRequest,
  ) -> Result<Option<ManagedSessionSnapshot>, String> {
    self
      .manager
      .creation_receipt(&request.pi_request())
      .map(|snapshot| snapshot.map(Into::into))
  }

  pub(crate) async fn stop(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    self.manager.stop(request).await.map(Into::into)
  }

  pub(crate) async fn resume(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    self.manager.resume(request).await.map(Into::into)
  }

  pub(crate) async fn respond_dialog(
    &self,
    response: ManagedSessionDialogResponse,
  ) -> Result<ManagedSessionSnapshot, String> {
    self.manager.respond_dialog(response).await.map(Into::into)
  }

  pub(crate) async fn submit_owned_input_with_model(
    &self,
    session_key: &str,
    request_id: &str,
    text: &str,
    images: Vec<ResolvedImage>,
    selection: Option<MessageModelSelection>,
  ) -> Result<Admission, DeliveryError> {
    self
      .manager
      .submit_owned_input_with_model(
        session_key,
        request_id,
        text,
        images.into_iter().map(image_input).collect(),
        selection,
      )
      .await
  }

  pub(crate) fn subscribe(&self) -> ManagedSessionEvents {
    ManagedSessionEvents(self.manager.subscribe())
  }

  pub(crate) async fn list_runtimes(&self) -> Vec<RuntimeInfo> {
    self
      .manager
      .list_runtimes()
      .await
      .into_iter()
      .map(runtime_info)
      .collect()
  }

  pub(crate) async fn restart_runtime(&self, request: &RuntimeRestartRequest) -> Result<(), String> {
    self
      .manager
      .restart_runtime(&shared::RuntimeRestartRequest {
        request_id: request.request_id.clone(),
        runtime_id: request.runtime_id.clone(),
        expected_instance_id: request.expected_instance_id.clone(),
      })
      .await
  }
}

pub(crate) struct ManagedSessionEvents(tokio::sync::broadcast::Receiver<shared::ManagedSessionSnapshot>);
impl ManagedSessionEvents {
  pub(crate) async fn recv(&mut self) -> Result<ManagedSessionSnapshot, tokio::sync::broadcast::error::RecvError> {
    self.0.recv().await.map(Into::into)
  }
}

pub(crate) fn image_input(image: ResolvedImage) -> shared::ResolvedImage {
  shared::ResolvedImage {
    image_id: image.image_id,
    mime_type: image.mime_type,
    sha256: image.sha256,
    bytes: image.bytes,
  }
}

fn runtime_info(info: shared::ManagedRuntimeInfo) -> RuntimeInfo {
  RuntimeInfo {
    runtime_id: info.runtime_id,
    instance_id: info.instance_id,
    name: info
      .session_id
      .as_deref()
      .map(|id| format!("Pi session {}", id.chars().take(12).collect::<String>()))
      .unwrap_or_else(|| "Pi session starting".into()),
    kind: RuntimeKind::Pi,
    execution_kind: RuntimeExecutionKind::Subprocess,
    ownership: if info.externally_owned {
      RuntimeOwnership::External
    } else {
      RuntimeOwnership::Viewer
    },
    status: info.status,
    pid: info.pid,
    started_at_ms: info.started_at_ms,
    session_key: info.session_key,
    session_id: info.session_id,
    project_id: Some(info.project_id),
    working_directory: Some(info.working_directory),
    endpoint: None,
    detail: Some(
      if info.externally_owned {
        "Owned by another viewer process"
      } else if info.has_worker {
        "Viewer-owned Pi RPC worker; stops when the viewer quits"
      } else {
        "Saved conversation; no Pi worker is running in this viewer"
      }
      .into(),
    ),
    last_error: info.last_error,
    can_restart: info.can_restart,
    restart_disabled_reason: info.restart_disabled_reason,
  }
}

#[cfg(test)]
mod tests;
