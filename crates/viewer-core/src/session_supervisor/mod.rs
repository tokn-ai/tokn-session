//! Host-owned session coordination. Provider drivers retain native semantics;
//! runtime processes and connections remain owned by the runtime manager.
mod controls;
mod rpc;

use crate::{
  images::ResolvedImage,
  input::{self, Admission, DeliveryError, InputTarget},
  managed_sessions::ManagedSessions,
  model::{SubmitSessionInputRequest, ViewerProvider},
  rpc_runtime::RpcRuntimeManager,
  session_ownership::{SessionLease, SessionOwnership},
};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::OwnedMutexGuard;
pub use tokn_interaction_core::session_supervisor::SessionTransport;
use tokn_interaction_core::session_supervisor::{SessionCoordinator, SessionIdentity, SessionRouteHost};
use tokn_interaction_core::{InteractionService, Provider};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionControlRequest {
  pub session_key: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSessionControlRequest {
  pub request_id: String,
  pub session_key: String,
  pub transport: SessionTransport,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelSessionTurnRequest {
  pub request_id: String,
  pub session_key: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionControlStatus {
  pub session_key: String,
  pub provider: ViewerProvider,
  pub transport: SessionTransport,
  pub can_select: bool,
  pub can_cancel: bool,
  pub available: bool,
  pub message: String,
  pub runtime_id: Option<String>,
  pub instance_id: Option<String>,
}

/// Constructed only from a freshly verified host catalog/header, never client
/// launch paths or native identities. Inspection does not activate this target.
pub(crate) struct VerifiedSession {
  pub session_key: String,
  pub input: InputTarget,
  pub cwd: PathBuf,
}

impl VerifiedSession {
  fn provider(&self) -> Result<Provider, String> {
    match self.input.provider {
      ViewerProvider::Codex => Ok(Provider::Codex),
      ViewerProvider::Pi => Ok(Provider::Pi),
      ViewerProvider::OpenCode => Ok(Provider::OpenCode),
      ViewerProvider::Dsh => Ok(Provider::Dsh),
      _ => Err("This provider has no live RPC adapter".into()),
    }
  }

  fn core_identity(&self) -> Result<SessionIdentity, String> {
    Ok(SessionIdentity {
      scope: self.identity(),
      provider: self.provider()?,
      native_id: self.input.session_id.clone(),
    })
  }

  fn identity(&self) -> String {
    serde_json::json!([self.input.provider.as_str(), self.input.session_id]).to_string()
  }
}

/// One session entry point for all frontends. Pi's workflow driver is exclusive;
/// its private actor is not exposed alongside a competing generic adapter.
#[derive(Clone)]
pub(crate) struct SessionSupervisor {
  pub(crate) managed: ManagedSessions,
  pub(crate) runtimes: RpcRuntimeManager,
  pub(crate) creations: Arc<OnceLock<Result<tokn_interaction_core::creation::CreationManager, String>>>,
  pub(crate) creation_path: Option<PathBuf>,
  pub(crate) creation_routes: Result<tokn_interaction_core::creation::CreationRouteLedger, String>,
  #[cfg(test)]
  ownership: SessionOwnership,
  controls: Arc<controls::ControlStore>,
  coordinator: SessionCoordinator,
}

impl SessionSupervisor {
  pub(crate) fn new(directory: Option<PathBuf>, interactions: InteractionService) -> Result<Self, String> {
    let ownership = SessionOwnership::new(directory.as_ref().map(|path| path.join("session-leases")))?;
    let managed = ManagedSessions::new_with_session_ownership(
      directory.as_ref().map(|path| path.join("managed-sessions.json")),
      ownership.clone(),
    )
    .unwrap_or_else(ManagedSessions::disabled);
    let runtimes = RpcRuntimeManager::new(
      directory.as_ref().map(|path| path.join("rpc-runtimes.json")),
      interactions,
    )
    .with_session_ownership(ownership.clone())?;
    Ok(Self {
      managed,
      runtimes,
      creations: Arc::new(OnceLock::new()),
      creation_path: directory.as_ref().map(|path| path.join("session-creations.json")),
      creation_routes: tokn_interaction_core::creation::CreationRouteLedger::new(
        directory.as_ref().map(|path| path.join("session-creation-routes.json")),
      ),
      coordinator: SessionCoordinator::new(ownership.clone()),
      #[cfg(test)]
      ownership,
      controls: Arc::new(controls::ControlStore::new(
        directory.as_ref().map(|path| path.join("session-controls.json")),
      )?),
    })
  }

  pub(crate) async fn creation_gate(&self, request_id: &str) -> OwnedMutexGuard<()> {
    self.coordinator.gate(&format!("creation:{request_id}")).await
  }

  async fn gate(&self, target: &VerifiedSession) -> OwnedMutexGuard<()> {
    self.coordinator.gate(&target.identity()).await
  }

  pub(crate) async fn transport(&self, target: &VerifiedSession) -> Result<SessionTransport, String> {
    if target.input.provider != ViewerProvider::Codex {
      return Ok(SessionTransport::Rpc);
    }
    let controls = self.controls.clone();
    let identity = target.identity();
    tokio::task::spawn_blocking(move || controls.transport(&identity))
      .await
      .map_err(|_| "Could not read session transport".to_string())?
  }

  pub(crate) async fn set_transport(
    &self,
    target: &VerifiedSession,
    request: &SetSessionControlRequest,
    broker: &input::InputBroker,
  ) -> Result<(), String> {
    if target.input.provider != ViewerProvider::Codex {
      return Err("Desktop transport is available for Codex only".into());
    }
    self
      .coordinator
      .set_transport(
        target.core_identity()?,
        request.request_id.clone(),
        request.transport,
        broker.0.clone(),
        target.input.identity(),
        Arc::new(ViewerRouteHost {
          controls: self.controls.clone(),
          runtimes: self.runtimes.clone(),
          identity: target.identity(),
          session_key: target.session_key.clone(),
        }),
      )
      .await
  }

  pub(crate) async fn input_status(&self, target: &VerifiedSession) -> Result<input::InputCapability, DeliveryError> {
    if let Some(status) = self.managed.checked_input_status(&target.session_key).await {
      return status.map(|status| input::InputCapability {
        message: status.message,
        images_available: status.images_available,
      });
    }
    match (
      target.input.provider,
      self.transport(target).await.map_err(DeliveryError::not_sent)?,
    ) {
      (ViewerProvider::Codex, SessionTransport::Desktop) => input::status(&target.input).await,
      (ViewerProvider::Pi, _) => input::status(&target.input).await,
      (_, SessionTransport::Rpc) => self.rpc_input_status(target).await,
      _ => Err(DeliveryError::not_sent(
        "This transport is unavailable for this provider",
      )),
    }
  }

  pub(crate) async fn submit(
    &self,
    target: VerifiedSession,
    request: SubmitSessionInputRequest,
    images: Vec<ResolvedImage>,
    expected_transport: SessionTransport,
  ) -> Result<Admission, DeliveryError> {
    let _guard = self.gate(&target).await;
    if self.transport(&target).await.map_err(DeliveryError::not_sent)? != expected_transport {
      return Err(DeliveryError::not_sent(
        "Session transport changed before message admission; review and send again",
      ));
    }
    if self.managed.input_status(&target.session_key).is_some() {
      drop(_guard);
      return self
        .managed
        .submit_owned_input_with_model(
          &target.session_key,
          &request.request_id,
          &request.text,
          images,
          request.model_selection,
        )
        .await;
    }
    let transport = self.transport(&target).await.map_err(DeliveryError::not_sent)?;
    if target.input.provider == ViewerProvider::Codex && transport == SessionTransport::Desktop {
      let _lease = self.claim_desktop(&target).await?;
      if request.model_selection.is_some() {
        return Err(DeliveryError::not_sent(
          "Codex Desktop does not expose a verified model catalog",
        ));
      }
      drop(_guard);
      return input::codex::submit_images(&target.input, &request.request_id, &request.text, &images).await;
    }
    drop(_guard);
    if target.input.provider == ViewerProvider::Pi {
      // Independently running Pi bridges remain controlled by their native
      // owner; saved history alone never gives access to another process's IO.
      return input::deliver(target.input, request, images).await;
    }
    self.submit_rpc(&target, request, images).await
  }

  pub(crate) async fn submit_desktop_question(
    &self,
    target: &VerifiedSession,
    request_id: &str,
    text: &str,
  ) -> Result<Admission, DeliveryError> {
    let guard = self.gate(target).await;
    if target.input.provider != ViewerProvider::Codex
      || self.transport(target).await.map_err(DeliveryError::not_sent)? != SessionTransport::Desktop
    {
      return Err(DeliveryError::not_sent(
        "The question's Desktop control route changed; refresh before answering",
      ));
    }
    let _lease = self.claim_desktop(target).await?;
    drop(guard);
    input::codex::submit_question(&target.input, request_id, text).await
  }

  async fn claim_desktop(&self, target: &VerifiedSession) -> Result<SessionLease, DeliveryError> {
    self
      .coordinator
      .claim_route(&target.core_identity().map_err(DeliveryError::not_sent)?)
      .await
      .map_err(DeliveryError::not_sent)
  }

  pub(crate) async fn shutdown(&self) {
    self.coordinator.close_admission();
    self.coordinator.drain_transitions().await;
    if let Some(Ok(creations)) = self.creations.get() {
      creations.shutdown().await;
    }
    self.runtimes.shutdown().await;
    self.managed.shutdown().await;
    self.coordinator.shutdown().await;
  }
}

pub(crate) fn input_response(
  request_id: String,
  result: Result<Admission, DeliveryError>,
) -> crate::model::SubmitSessionInputResponse {
  use crate::model::{InputDeliveryStatus, SubmitSessionInputResponse};
  match result {
    Ok(_) => SubmitSessionInputResponse {
      request_id,
      status: InputDeliveryStatus::Accepted,
      message: "Agent accepted the request.".into(),
      model_result: None,
    },
    Err(error) => SubmitSessionInputResponse {
      request_id,
      status: if error.may_have_been_sent {
        InputDeliveryStatus::Unknown
      } else {
        InputDeliveryStatus::NotSent
      },
      message: error.message,
      model_result: error.model_result,
    },
  }
}

#[cfg(test)]
mod rpc_tests;
#[cfg(test)]
mod tests;

/// Host policy and persistence hooks for the shared transition transaction.
struct ViewerRouteHost {
  controls: Arc<controls::ControlStore>,
  runtimes: RpcRuntimeManager,
  identity: String,
  session_key: String,
}
impl SessionRouteHost for ViewerRouteHost {
  fn current(&self) -> tokn_interaction_core::AdapterFuture<'_, Result<SessionTransport, String>> {
    Box::pin(async move {
      let controls = self.controls.clone();
      let identity = self.identity.clone();
      tokio::task::spawn_blocking(move || controls.transport(&identity))
        .await
        .map_err(|_| "Could not read session transport".to_owned())?
    })
  }
  fn release_idle_binding(&self) -> tokn_interaction_core::AdapterFuture<'_, Result<(), String>> {
    Box::pin(self.runtimes.release_binding(&self.session_key))
  }
  fn save(&self, transport: SessionTransport) -> tokn_interaction_core::AdapterFuture<'_, Result<(), String>> {
    Box::pin(async move {
      let controls = self.controls.clone();
      let identity = self.identity.clone();
      tokio::task::spawn_blocking(move || controls.set_transport(&identity, transport))
        .await
        .map_err(|_| "Could not save session transport".to_owned())?
    })
  }
}
