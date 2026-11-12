//! Native V4 guidance and follow-up admission, with no host-side delivery guess.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandAck {
  command_id: String,
  status: String,
  #[serde(rename = "revisionAtDecision")]
  _revision_at_decision: u64,
  reason_code: Option<String>,
  result: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputAccepted {
  #[serde(rename = "type")]
  kind: String,
  delivery: String,
  input_id: String,
}

impl Inner {
  pub(super) async fn deliver_followup(
    &self,
    session_id: &str,
    request_id: &str,
    input: MessageInput,
    delivery: DeliveryMode,
  ) -> Result<Reply, InteractionError> {
    let requested_delivery = match delivery {
      DeliveryMode::Guide => "guide",
      DeliveryMode::Queue => "queue",
      _ => {
        return Err(InteractionError::unsupported(
          "ZCode requires native guide or queue delivery",
        ));
      }
    };
    if input.expected_revision.is_some() {
      return Err(InteractionError::unsupported(
        "ZCode guide and queue do not support a revision or turn fence",
      ));
    }
    if input.model.is_some() {
      return Err(InteractionError::unsupported(
        "ZCode guide and queue use the session's current model",
      ));
    }
    let ack = self
      .native_command(
        session_id,
        request_id,
        "sendText",
        json!({"text":input.text,"requestedDelivery":requested_delivery}),
      )
      .await?;
    let receipt = input_receipt(ack, request_id, delivery)?;
    Ok(Reply::Admission(receipt))
  }

  pub(super) async fn stop_turn(&self, session_id: &str, request_id: &str) -> Result<Reply, InteractionError> {
    self.native_command(session_id, request_id, "stop", json!({})).await?;
    self.retire_interactions(session_id, true);
    Ok(Reply::Acknowledged)
  }

  async fn native_command(
    &self,
    session_id: &str,
    command_id: &str,
    kind: &str,
    payload: Value,
  ) -> Result<CommandAck, InteractionError> {
    validate_command_id(command_id)?;
    let params = command_envelope(
      self.workspace(),
      &self.descriptor.instance_id,
      session_id,
      command_id,
      kind,
      payload,
    );
    // Native V4 returns the CommandAck directly as the RPC result.
    let ack: CommandAck = wire::decode(self.transport.call("v4/command", params, true).await?, true)?;
    validate_ack(&ack, command_id)?;
    Ok(ack)
  }
}

fn validate_command_id(command_id: &str) -> Result<(), InteractionError> {
  if command_id.is_empty()
    || command_id.len() > 128
    || command_id.trim() != command_id
    || command_id.chars().any(char::is_control)
  {
    return Err(InteractionError::invalid(
      "ZCode requires a bounded native command identity",
    ));
  }
  Ok(())
}

fn command_envelope(
  workspace: Value,
  client_id: &str,
  session_id: &str,
  command_id: &str,
  kind: &str,
  payload: Value,
) -> Value {
  json!({
    "workspace":workspace,
    "commandId":command_id,
    "clientId":client_id,
    "sessionId":session_id,
    "type":kind,
    "payload":payload,
    "issuedAt":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
  })
}

fn validate_ack(ack: &CommandAck, expected_id: &str) -> Result<(), InteractionError> {
  if ack.command_id != expected_id {
    return Err(InteractionError::uncertain(
      "ZCode returned a different command admission identity",
    ));
  }
  if matches!(ack.status.as_str(), "failed" | "rejected")
    && ack.reason_code.as_deref() == Some("heldQueueDispositionRequired")
  {
    // The native guard rejects before prompt admission or held-queue mutation.
    // Other execution failures remain uncertain and must never be replayed.
    return Err(InteractionError::new(
      ErrorKind::QueueHeld,
      "ZCode paused queued messages after Stop. Open ZCode to keep or clear the held queue before sending again.",
    ));
  }
  match ack.status.as_str() {
    "accepted" | "duplicate" => Ok(()),
    "rejected" => Err(InteractionError::new(
      ErrorKind::Rejected,
      "ZCode did not admit the command",
    )),
    "stale" | "noop" => Err(InteractionError::new(
      ErrorKind::Conflict,
      "ZCode command does not match the current session state",
    )),
    _ => Err(InteractionError::uncertain(
      "ZCode did not confirm whether the command took effect",
    )),
  }
}

fn input_receipt(ack: CommandAck, request_id: &str, delivery: DeliveryMode) -> Result<InputReceipt, InteractionError> {
  let accepted: InputAccepted = wire::decode(
    ack
      .result
      .ok_or_else(|| InteractionError::uncertain("ZCode omitted the native input admission result"))?,
    true,
  )?;
  if accepted.kind != "inputAccepted" || accepted.input_id != request_id {
    return Err(InteractionError::uncertain(
      "ZCode returned a different input admission identity",
    ));
  }
  let disposition = match (delivery, accepted.delivery.as_str()) {
    (DeliveryMode::Guide | DeliveryMode::Queue, "startNow") => InputDisposition::Started,
    (DeliveryMode::Queue, "queue") => InputDisposition::Queued,
    // A queued guide can be consumed in the current turn or become a later
    // follow-up. This ACK does not distinguish those native outcomes.
    (DeliveryMode::Guide, "queue") => InputDisposition::Accepted,
    _ => {
      return Err(InteractionError::uncertain(
        "ZCode returned an unexpected native delivery result",
      ));
    }
  };
  Ok(InputReceipt {
    disposition,
    model: None,
    model_ignored: false,
    native_id: Some(accepted.input_id),
  })
}

#[cfg(test)]
mod tests;
