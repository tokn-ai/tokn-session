//! Host-owned session coordination. Provider drivers retain native semantics;
//! runtime processes and connections remain owned by the runtime manager.
mod controls;
mod rpc;

use crate::{
  images::ResolvedImage,
  input::{self, Admission, DeliveryError, InputTarget},
  managed_pi::ManagedPiSessions,
  model::{SubmitSessionInputRequest, ViewerProvider},
  rpc_runtime::RpcRuntimeManager,
  session_ownership::{SessionLease, SessionOwnership},
};
use serde::{Deserialize, Serialize};
use std::{
  collections::HashMap,
  path::PathBuf,
  sync::{Arc, Mutex, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokn_interaction_core::{InteractionService, Provider, ReceiptAdmission, ReceiptLedger};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTransport {
  #[default]
  Rpc,
  Desktop,
}

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

  fn identity(&self) -> String {
    serde_json::json!([self.input.provider.as_str(), self.input.session_id]).to_string()
  }
}

/// One session entry point for all frontends. Pi's workflow driver is exclusive;
/// its private actor is not exposed alongside a competing generic adapter.
#[derive(Clone)]
pub(crate) struct SessionSupervisor {
  pub(crate) pi: ManagedPiSessions,
  pub(crate) runtimes: RpcRuntimeManager,
  ownership: SessionOwnership,
  controls: Arc<controls::ControlStore>,
  gates: Arc<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>>,
  desktop_owners: Arc<Mutex<HashMap<String, SessionLease>>>,
  control_receipts: Arc<Mutex<ReceiptLedger<Result<(), String>>>>,
}

impl SessionSupervisor {
  pub(crate) fn new(directory: Option<PathBuf>, interactions: InteractionService) -> Result<Self, String> {
    let ownership = SessionOwnership::new(directory.as_ref().map(|path| path.join("session-leases")))?;
    let pi = ManagedPiSessions::new_with_session_ownership(
      directory.as_ref().map(|path| path.join("managed-sessions.json")),
      ownership.clone(),
    )
    .unwrap_or_else(ManagedPiSessions::disabled);
    let runtimes = RpcRuntimeManager::new(
      directory.as_ref().map(|path| path.join("rpc-runtimes.json")),
      interactions,
    )
    .with_session_ownership(ownership.clone())?;
    Ok(Self {
      pi,
      runtimes,
      ownership,
      controls: Arc::new(controls::ControlStore::new(
        directory.as_ref().map(|path| path.join("session-controls.json")),
      )?),
      gates: Arc::new(Mutex::new(HashMap::new())),
      desktop_owners: Arc::new(Mutex::new(HashMap::new())),
      control_receipts: Arc::new(Mutex::new(ReceiptLedger::new(256))),
    })
  }

  async fn gate(&self, target: &VerifiedSession) -> OwnedMutexGuard<()> {
    let gate = {
      let mut gates = self.gates.lock().unwrap_or_else(|error| error.into_inner());
      gates.retain(|_, gate| gate.strong_count() > 0);
      let identity = target.identity();
      if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
        gate
      } else {
        let gate = Arc::new(AsyncMutex::new(()));
        gates.insert(identity, Arc::downgrade(&gate));
        gate
      }
    };
    gate.lock_owned().await
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
    uuid::Uuid::parse_str(&request.request_id).map_err(|_| "Invalid session control request ID")?;
    if target.input.provider != ViewerProvider::Codex {
      return Err("Desktop transport is available for Codex only".into());
    }
    let _guard = self.gate(target).await;
    use sha2::{Digest, Sha256};
    let fingerprint = Sha256::digest(serde_json::to_vec(&request.transport).expect("transport serializes")).into();
    {
      let mut receipts = self.control_receipts.lock().unwrap_or_else(|error| error.into_inner());
      match receipts.admit(
        request.request_id.clone(),
        target.identity(),
        fingerprint,
        Err("Session transport change is still pending".into()),
      ) {
        Ok(ReceiptAdmission::Replay(previous)) => return previous,
        Ok(ReceiptAdmission::New) => {}
        Err(_) => return Err("Session control request ID conflicts or its receipt capacity is full".into()),
      }
    }
    let result = self.change_transport(target, request, broker).await;
    self
      .control_receipts
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .complete(&request.request_id, result.clone());
    result
  }

  async fn change_transport(
    &self,
    target: &VerifiedSession,
    request: &SetSessionControlRequest,
    broker: &input::InputBroker,
  ) -> Result<(), String> {
    let _transition = broker.begin_transition(&target.input)?;
    if self.transport(target).await? == request.transport {
      return Ok(());
    }
    // Release one idle binding, never stop a runtime shared with other sessions.
    self.runtimes.release_binding(&target.session_key).await?;
    let existing = self
      .desktop_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(&target.identity())
      .cloned();
    let _transition_lease = if let Some(lease) = existing {
      lease
    } else {
      let ownership = self.ownership.clone();
      let native_id = target.input.session_id.clone();
      tokio::task::spawn_blocking(move || ownership.acquire(Provider::Codex, &native_id))
        .await
        .map_err(|_| "Could not check session ownership")??
    };
    let controls = self.controls.clone();
    let identity = target.identity();
    let transport = request.transport;
    tokio::task::spawn_blocking(move || controls.set_transport(&identity, transport))
      .await
      .map_err(|_| "Could not save session transport".to_string())??;
    self
      .desktop_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .remove(&target.identity());
    Ok(())
  }

  pub(crate) async fn input_status(&self, target: &VerifiedSession) -> Result<input::InputCapability, DeliveryError> {
    if let Some(status) = self.pi.checked_input_status(&target.session_key).await {
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
    if self.pi.input_status(&target.session_key).is_some() {
      drop(_guard);
      return self
        .pi
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
    let identity = target.identity();
    {
      let owners = self.desktop_owners.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(lease) = owners.get(&identity) {
        return Ok(lease.clone());
      }
      if owners.len() >= 1024 {
        return Err(DeliveryError::not_sent(
          "Desktop session ownership capacity reached; release an idle session first",
        ));
      }
    }
    let ownership = self.ownership.clone();
    let native_id = target.input.session_id.clone();
    let lease = tokio::task::spawn_blocking(move || ownership.acquire(Provider::Codex, &native_id))
      .await
      .map_err(|_| DeliveryError::not_sent("Could not check session ownership"))?
      .map_err(DeliveryError::not_sent)?;
    self
      .desktop_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .insert(identity, lease.clone());
    Ok(lease)
  }

  pub(crate) async fn shutdown(&self) {
    self.runtimes.shutdown().await;
    self.pi.shutdown().await;
    self
      .desktop_owners
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .clear();
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
