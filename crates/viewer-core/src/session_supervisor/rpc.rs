use super::*;
use crate::models::{ListSessionModelsResponse, SessionModelState, SessionModelStatus};
use tokn_interaction_core::{
  Command, DeliveryMode, ErrorKind, ImageInput, InputDisposition, InteractionError, MessageInput, Operation,
  PendingInteraction, Reply,
};

impl SessionSupervisor {
  pub(super) async fn rpc_input_status(
    &self,
    target: &VerifiedSession,
  ) -> Result<input::InputCapability, DeliveryError> {
    target.provider().map_err(DeliveryError::not_sent)?;
    if let Some(binding) = self.runtimes.binding(&target.session_key) {
      let snapshot = self
        .runtimes
        .binding_snapshot(&target.session_key)
        .ok_or_else(|| DeliveryError::not_sent("RPC runtime disconnected; reconnect it before sending"))?;
      if snapshot.recovery_required {
        return Err(DeliveryError::not_sent(
          "RPC state requires recovery; reconnect the runtime before sending",
        ));
      }
      if !binding.capabilities.supports(Operation::Message) {
        return Err(DeliveryError::not_sent("This runtime does not support message input"));
      }
      let images_available = if matches!(target.input.provider, ViewerProvider::Codex | ViewerProvider::OpenCode) {
        self.rpc_models(target, true).await.ok().is_some_and(|catalog| {
          catalog.status.state.model.as_ref().is_some_and(|current| {
            catalog.models.iter().any(|choice| {
              choice.supports_images
                && choice.model.model_id == current.model_id
                && choice.model.model_provider == current.model_provider
            })
          })
        })
      } else {
        false
      };
      return Ok(input::InputCapability {
        message: "Connected through agent RPC. Send starts or steers this session.".into(),
        images_available,
      });
    }
    self
      .runtimes
      .activation_ready(target.provider().map_err(DeliveryError::not_sent)?, &target.cwd)
      .await
      .map_err(DeliveryError::not_sent)?;
    Ok(input::InputCapability {
      message: "Send will connect the configured RPC runtime and resume this session.".into(),
      images_available: target.input.provider == ViewerProvider::Codex,
    })
  }

  pub(super) async fn submit_rpc(
    &self,
    target: &VerifiedSession,
    request: SubmitSessionInputRequest,
    images: Vec<ResolvedImage>,
  ) -> Result<Admission, DeliveryError> {
    // Model intent is never simulated by a racing metadata/read/set sequence.
    // Pi's exclusive driver retains its existing admission workflow separately.
    if request.model_selection.is_some() {
      return Err(DeliveryError::not_sent(
        "This RPC runtime cannot atomically apply a draft model on Send",
      ));
    }
    let binding = self
      .runtimes
      .ensure_session(
        &request.request_id,
        &target.session_key,
        target.provider().map_err(DeliveryError::not_sent)?,
        &target.input.session_id,
        &target.cwd,
      )
      .await
      .map_err(DeliveryError::not_sent)?;
    use base64::Engine;
    let images = images
      .into_iter()
      .map(|image| ImageInput {
        mime_type: image.mime_type,
        data: base64::engine::general_purpose::STANDARD.encode(image.bytes.as_slice()),
      })
      .collect();
    let reply = self
      .runtimes
      .execute_binding(
        &binding,
        request.request_id,
        Command::Message {
          input: MessageInput {
            text: request.text,
            images,
            ..Default::default()
          },
          delivery: DeliveryMode::StartOrSteer,
        },
      )
      .await
      .map_err(delivery_error)?;
    admission(reply)
  }

  /// Query only an existing binding. Model inspection never resumes a thread.
  pub(crate) async fn rpc_models(
    &self,
    target: &VerifiedSession,
    catalog: bool,
  ) -> Result<ListSessionModelsResponse, String> {
    let Some(binding) = self.runtimes.binding(&target.session_key) else {
      return Ok(ListSessionModelsResponse {
        status: SessionModelStatus::unavailable("Send a message to connect this session's RPC runtime"),
        models: Vec::new(),
      });
    };
    let state = match self
      .runtimes
      .execute_binding(&binding, uuid::Uuid::new_v4().to_string(), Command::State)
      .await
    {
      Ok(Reply::State(state)) => state,
      Ok(_) => return Err("RPC runtime returned an unexpected model state".into()),
      Err(error) => return Err(error.message),
    };
    let models = if catalog && binding.capabilities.supports(Operation::Models) {
      match self
        .runtimes
        .execute_binding(&binding, uuid::Uuid::new_v4().to_string(), Command::Models)
        .await
      {
        Ok(Reply::Models(models)) => models,
        Ok(_) => return Err("RPC runtime returned an unexpected model catalog".into()),
        Err(error) => return Err(error.message),
      }
    } else {
      Vec::new()
    };
    Ok(ListSessionModelsResponse {
      status: SessionModelStatus {
        state: SessionModelState::live(state.model, &binding.target.instance_id),
        can_select: binding.capabilities.atomic_model_on_send,
        can_list: binding.capabilities.supports(Operation::Models),
        reason: (!binding.capabilities.atomic_model_on_send).then(|| {
          "This RPC runtime exposes its model catalog but cannot atomically apply a draft model on Send".into()
        }),
        runtime_generation: Some(binding.target.instance_id),
        updates_pi_default: false,
        supports_model_input: binding.capabilities.atomic_model_on_send,
      },
      models,
    })
  }

  pub(crate) async fn pending_rpc(&self, target: &VerifiedSession) -> Result<Vec<PendingInteraction>, String> {
    let binding = self
      .runtimes
      .binding(&target.session_key)
      .ok_or("No connected RPC session owns this request")?;
    match self
      .runtimes
      .execute_binding(&binding, uuid::Uuid::new_v4().to_string(), Command::PendingInteractions)
      .await
    {
      Ok(Reply::Interactions(interactions)) => Ok(interactions),
      Ok(_) => Err("RPC runtime returned unexpected pending input".into()),
      Err(error) => Err(error.message),
    }
  }

  pub(crate) async fn respond_rpc(
    &self,
    target: &VerifiedSession,
    request_id: String,
    interaction_id: String,
    response: tokn_interaction_core::InteractionResponse,
  ) -> Result<Admission, DeliveryError> {
    if self.transport(target).await.map_err(DeliveryError::not_sent)? != SessionTransport::Rpc {
      return Err(DeliveryError::not_sent("This session's control transport changed"));
    }
    let binding = self
      .runtimes
      .binding(&target.session_key)
      .ok_or_else(|| DeliveryError::not_sent("The live request no longer has an RPC owner"))?;
    match self
      .runtimes
      .execute_binding(
        &binding,
        request_id,
        Command::Respond {
          interaction_id,
          response,
        },
      )
      .await
      .map_err(delivery_error)?
    {
      Reply::Acknowledged => Ok(Admission::Accepted),
      Reply::Pending => Err(DeliveryError::uncertain(
        "The answer is still awaiting RPC confirmation",
      )),
      _ => Err(DeliveryError::uncertain(
        "RPC returned an unexpected answer acknowledgement",
      )),
    }
  }

  pub(crate) async fn cancel(&self, target: &VerifiedSession, request_id: String) -> Result<Admission, DeliveryError> {
    if self.pi.input_status(&target.session_key).is_some() {
      self
        .pi
        .stop(crate::workspace::ManagedSessionRequest {
          request_id: None,
          session_key: Some(target.session_key.clone()),
        })
        .await
        .map_err(DeliveryError::uncertain)?;
      return Ok(Admission::Accepted);
    }
    if self.transport(target).await.map_err(DeliveryError::not_sent)? != SessionTransport::Rpc {
      return Err(DeliveryError::not_sent("Stop this turn in Codex Desktop"));
    }
    let binding = self
      .runtimes
      .binding(&target.session_key)
      .ok_or_else(|| DeliveryError::not_sent("This session has no connected RPC runtime"))?;
    match self
      .runtimes
      .execute_binding(&binding, request_id, Command::CancelTurn)
      .await
      .map_err(delivery_error)?
    {
      Reply::Acknowledged => Ok(Admission::Accepted),
      _ => Err(DeliveryError::uncertain("Cancellation is awaiting native confirmation")),
    }
  }
}

pub(super) fn delivery_error(error: InteractionError) -> DeliveryError {
  if error.kind == ErrorKind::Uncertain {
    DeliveryError::uncertain(error.message)
  } else {
    DeliveryError::not_sent(error.message)
  }
}

fn admission(reply: Reply) -> Result<Admission, DeliveryError> {
  match reply {
    Reply::Admission(receipt) => Ok(match receipt.disposition {
      InputDisposition::Started => Admission::Started,
      InputDisposition::Steered => Admission::Steered,
      InputDisposition::Queued => Admission::QueuedFollowUp,
      InputDisposition::Accepted => Admission::Accepted,
    }),
    Reply::Pending => Err(DeliveryError::uncertain(
      "Message admission is still pending; check the conversation before retrying",
    )),
    _ => Err(DeliveryError::uncertain(
      "RPC runtime returned an unexpected message acknowledgement",
    )),
  }
}
