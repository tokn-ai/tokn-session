use super::*;
use crate::session_supervisor::{
  CancelSessionTurnRequest, SessionControlRequest, SessionControlStatus, SessionTransport, SetSessionControlRequest,
  VerifiedSession,
};
use crate::{input::InputTarget, model::SubmitSessionInputResponse};

impl ViewerService {
  /// Catalog/header verification is kept at the host boundary. No full history
  /// parse, runtime startup, or native resume occurs during inspection.
  pub(super) fn verified_control_target(&self, key: &str) -> Result<VerifiedSession, String> {
    self.check_managed_workspace()?;
    let locator = decode_session_key(key)?;
    let canonical = encode_session_key(&locator)?;
    // A just-created Pi history is already verified by its durable owner, but
    // may precede the provider-wide catalog sentinel's first commit.
    let input = if (locator.provider == ViewerProvider::Pi && self.sessions.managed.input_status(&canonical).is_some())
      || self
        .sessions
        .creations
        .get()
        .and_then(|manager| manager.as_ref().ok())
        .is_some_and(|manager| {
          manager
            .list()
            .iter()
            .any(|receipt| receipt.session_key.as_deref() == Some(&canonical))
        }) {
      if !locator.source_path.is_absolute() {
        return Err("Managed session history must have an absolute path".into());
      }
      InputTarget {
        provider: locator.provider,
        session_id: locator.session_id,
        session_file: locator.source_path,
      }
    } else {
      self.resolve_input_target(key)?
    };
    let header = self
      .repository
      .session_header_exact(input.provider, &input.session_file, &input.session_id)
      .map_err(|_| "Session history is unavailable")?;
    if header.id != input.session_id {
      return Err("Session identity changed; refresh the conversation".into());
    }
    if input.provider == ViewerProvider::OpenCode {
      self.verify_opencode_control(&decode_session_key(&canonical)?)?;
    }
    if input.provider == ViewerProvider::Codex
      && (header.parent_session_id.as_deref().is_some_and(|id| !id.is_empty())
        || header
          .agent_path
          .as_deref()
          .is_some_and(|path| !path.is_empty() && path != "/root"))
    {
      return Err("Control Codex subagents through their parent thread".into());
    }
    let cwd = header
      .cwd
      .map(PathBuf::from)
      .filter(|path| path.is_absolute())
      // Some older histories have no working directory. Existing Desktop/Pi
      // controls can still work; RPC activation requires its verified folder.
      .unwrap_or_default();
    Ok(VerifiedSession {
      session_key: canonical,
      input,
      cwd,
    })
  }

  pub(super) async fn control_target(&self, key: String) -> Result<VerifiedSession, String> {
    let service = self.clone();
    tokio::task::spawn_blocking(move || service.verified_control_target(&key))
      .await
      .map_err(|_| "Could not verify session control target".to_string())?
  }

  pub async fn get_session_control(&self, request: SessionControlRequest) -> Result<SessionControlStatus, String> {
    let target = self.control_target(request.session_key).await?;
    self.control_status(&target).await
  }

  async fn control_status(&self, target: &VerifiedSession) -> Result<SessionControlStatus, String> {
    let transport = self.sessions.transport(target).await?;
    let binding = self.sessions.runtimes.binding(&target.session_key);
    let connected = binding
      .as_ref()
      .is_some_and(|_| self.sessions.runtimes.binding_snapshot(&target.session_key).is_some());
    let readiness = self.sessions.input_status(target).await;
    Ok(SessionControlStatus {
      session_key: target.session_key.clone(),
      provider: target.input.provider,
      transport,
      can_select: target.input.provider == ViewerProvider::Codex,
      can_cancel: transport == SessionTransport::Rpc
        && (binding.is_some() || self.sessions.managed.input_status(&target.session_key).is_some()),
      available: readiness.is_ok(),
      message: if let Err(error) = readiness {
        error.message
      } else {
        match transport {
          SessionTransport::Desktop => "Codex Desktop receives messages for this session.".into(),
          SessionTransport::Rpc if connected => "Connected through agent RPC.".into(),
          SessionTransport::Rpc => "Send will connect and resume through agent RPC.".into(),
        }
      },
      runtime_id: binding.as_ref().map(|binding| binding.target.runtime_id.clone()),
      instance_id: binding.map(|binding| binding.target.instance_id),
    })
  }

  pub async fn set_session_control(&self, request: SetSessionControlRequest) -> Result<SessionControlStatus, String> {
    self.check_managed_workspace()?;
    let service = self.clone();
    // Switching releases an idle binding and saves one host preference. A
    // disconnected client must not interrupt that ownership transition.
    tokio::spawn(async move {
      let target = service.control_target(request.session_key.clone()).await?;
      if service.input_broker.has_unresolved(&target.input) {
        return Err("A submission is pending or its delivery is uncertain; keep its transport until reconciled".into());
      }
      // Desktop has no native idle proof through follower IPC. Refuse takeover
      // while recorded activity is running or another submission is unresolved.
      if service.sessions.transport(&target).await? == SessionTransport::Desktop {
        if service
          .session_index
          .session(&index_session_key(&decode_session_key(&target.session_key)?)?)
          .map_err(|error| error.to_string())?
          .is_some_and(|row| {
            Activity::from_marker(row.attention_marker.as_deref()).is_some_and(|activity| activity.running)
          })
        {
          return Err("Wait for Codex Desktop to finish this session before switching transport".into());
        }
      }
      service
        .sessions
        .set_transport(&target, &request, &service.input_broker)
        .await?;
      service.control_status(&target).await
    })
    .await
    .map_err(|_| "Session transport change was interrupted; refresh its state".to_string())?
  }

  pub async fn cancel_session_turn(
    &self,
    request: CancelSessionTurnRequest,
  ) -> Result<SubmitSessionInputResponse, String> {
    uuid::Uuid::parse_str(&request.request_id).map_err(|_| "Invalid cancellation request ID")?;
    let target = self.control_target(request.session_key).await?;
    let broker_target = InputTarget {
      provider: target.input.provider,
      session_id: target.input.session_id.clone(),
      session_file: target.input.session_file.clone(),
    };
    let message = crate::model::SubmitSessionInputRequest {
      request_id: request.request_id.clone(),
      session_key: target.session_key.clone(),
      text: "session-control:cancel-turn".into(),
      image_ids: Vec::new(),
      model_selection: None,
    };
    let sessions = self.sessions.clone();
    let request_id = request.request_id;
    Ok(
      self
        .input_broker
        .submit_control(&broker_target, message, async move {
          sessions.cancel(&target, request_id).await
        })
        .await,
    )
  }
}
