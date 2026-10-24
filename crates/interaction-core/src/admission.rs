//! Provider-neutral message/control admission, receipts, and transition fencing.
use crate::managed_sessions::{Admission, DeliveryError, ModelInputDisposition};
pub use crate::managed_sessions::{MAX_INPUT_LENGTH, validate_image_ids};
use crate::{ReceiptAdmission, ReceiptError, ReceiptLedger};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
  collections::HashSet,
  future::Future,
  sync::{Arc, Mutex},
};

pub fn validate_request(request: &mut SubmitSessionInputRequest) -> Result<(), String> {
  uuid::Uuid::parse_str(&request.request_id).map_err(|_| "Invalid message request ID")?;
  crate::managed_sessions::validate_message(&request.text, !request.image_ids.is_empty())?;
  validate_image_ids(&request.image_ids)
}

#[derive(Clone, Debug, Deserialize)]
pub struct SubmitSessionInputRequest {
  pub session_key: String,
  pub request_id: String,
  pub text: String,
  #[serde(default)]
  pub image_ids: Vec<String>,
  #[serde(default)]
  pub model_selection: Option<crate::managed_sessions::MessageModelSelection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeliveryStatus {
  Accepted,
  NotSent,
  Unknown,
  Pending,
}

#[derive(Clone, Debug, Serialize)]
pub struct SubmitSessionInputResponse {
  pub request_id: String,
  pub status: InputDeliveryStatus,
  pub message: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub model_result: Option<crate::managed_sessions::MessageModelResult>,
}

/// Preserve text-only fingerprints while binding image messages to their ordered
/// immutable upload identities. Clients can never substitute host paths.
pub fn message_fingerprint(text: &str, image_ids: &[String]) -> [u8; 32] {
  if image_ids.is_empty() {
    return Sha256::digest(text.as_bytes()).into();
  }
  Sha256::digest(serde_json::to_vec(&(text, image_ids)).expect("string message fingerprint")).into()
}

fn input_fingerprint(request: &SubmitSessionInputRequest) -> [u8; 32] {
  let Some(selection) = &request.model_selection else {
    return message_fingerprint(&request.text, &request.image_ids);
  };
  Sha256::digest(serde_json::to_vec(&(&request.text, &request.image_ids, selection)).expect("message fingerprint"))
    .into()
}

fn request_fingerprint(request: &SubmitSessionInputRequest, control: bool) -> [u8; 32] {
  let message = input_fingerprint(request);
  if control {
    Sha256::digest([b"session-control:".as_slice(), message.as_slice()].concat()).into()
  } else {
    message
  }
}

const MAX_RECENT_REQUESTS: usize = 1024;

struct RequestState {
  receipts: ReceiptLedger<SubmitSessionInputResponse>,
  in_flight: HashSet<String>,
  pending_controls: std::collections::HashMap<String, usize>,
  uncertain_sessions: HashSet<String>,
  transitions: HashSet<String>,
}

impl Default for RequestState {
  fn default() -> Self {
    Self {
      receipts: ReceiptLedger::new(MAX_RECENT_REQUESTS),
      in_flight: HashSet::new(),
      pending_controls: std::collections::HashMap::new(),
      uncertain_sessions: HashSet::new(),
      transitions: HashSet::new(),
    }
  }
}

#[derive(Clone, Default)]
pub struct InputBroker(Arc<Mutex<RequestState>>);

/// Reserves the broker while a transport releases ownership and saves its new
/// route. Controls cannot enter after the switch has passed its idle check.
pub struct TransportTransition {
  broker: InputBroker,
  identity: String,
}

impl Drop for TransportTransition {
  fn drop(&mut self) {
    self
      .broker
      .0
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .transitions
      .remove(&self.identity);
  }
}

impl InputBroker {
  pub fn begin_transition(&self, target: &str) -> Result<TransportTransition, String> {
    let identity = target.to_owned();
    let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
    if state.in_flight.contains(&identity)
      || state.pending_controls.contains_key(&identity)
      || state.uncertain_sessions.contains(&identity)
      || state.transitions.contains(&identity)
    {
      return Err(
        "A submission is pending or uncertain, or its transport is changing; reconcile it before switching".into(),
      );
    }
    state.transitions.insert(identity.clone());
    Ok(TransportTransition {
      broker: self.clone(),
      identity,
    })
  }

  pub fn has_unresolved(&self, target: &str) -> bool {
    let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
    state.in_flight.contains(&target.to_owned())
      || state.pending_controls.contains_key(&target.to_owned())
      || state.uncertain_sessions.contains(&target.to_owned())
  }

  pub fn receipt(&self, target: &str, request: &SubmitSessionInputRequest) -> Option<SubmitSessionInputResponse> {
    self.lookup_receipt(target, request, false)
  }

  pub fn control_receipt(
    &self,
    target: &str,
    request: &SubmitSessionInputRequest,
  ) -> Option<SubmitSessionInputResponse> {
    self.lookup_receipt(target, request, true)
  }

  fn lookup_receipt(
    &self,
    target: &str,
    request: &SubmitSessionInputRequest,
    control: bool,
  ) -> Option<SubmitSessionInputResponse> {
    let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
    match state.receipts.lookup(
      &request.request_id,
      &target.to_owned(),
      &request_fingerprint(request, control),
    ) {
      Ok(receipt) => receipt,
      Err(_) => Some(SubmitSessionInputResponse {
        model_result: None,
        request_id: request.request_id.clone(),
        status: InputDeliveryStatus::NotSent,
        message: "This request ID was already used for a different message.".into(),
      }),
    }
  }

  pub async fn submit_custom<F>(
    &self,
    target: &str,
    mut request: SubmitSessionInputRequest,
    delivery: F,
  ) -> SubmitSessionInputResponse
  where
    F: Future<Output = Result<Admission, DeliveryError>> + Send + 'static,
  {
    request.session_key = target.to_owned();
    self.submit_with_mode(request, delivery, false).await
  }

  /// Answers and cancellation can unblock a message whose native admission is
  /// pending. They share receipts/uncertainty, but never wait on message admission.
  pub async fn submit_control<F>(
    &self,
    target: &str,
    mut request: SubmitSessionInputRequest,
    delivery: F,
  ) -> SubmitSessionInputResponse
  where
    F: Future<Output = Result<Admission, DeliveryError>> + Send + 'static,
  {
    request.session_key = target.to_owned();
    self.submit_with_mode(request, delivery, true).await
  }

  pub async fn submit_with<F>(&self, request: SubmitSessionInputRequest, delivery: F) -> SubmitSessionInputResponse
  where
    F: Future<Output = Result<Admission, DeliveryError>> + Send + 'static,
  {
    self.submit_with_mode(request, delivery, false).await
  }

  async fn submit_with_mode<F>(
    &self,
    request: SubmitSessionInputRequest,
    delivery: F,
    control: bool,
  ) -> SubmitSessionInputResponse
  where
    F: Future<Output = Result<Admission, DeliveryError>> + Send + 'static,
  {
    let text_hash = request_fingerprint(&request, control);
    let request_id = request.request_id.clone();
    let response = |status, message: &str| SubmitSessionInputResponse {
      model_result: None,
      request_id: request_id.clone(),
      status,
      message: message.into(),
    };
    {
      let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
      match state
        .receipts
        .lookup(&request.request_id, &request.session_key, &text_hash)
      {
        Ok(Some(previous)) => return previous,
        Err(_) => {
          return response(
            InputDeliveryStatus::NotSent,
            "This request ID was already used for a different message.",
          );
        }
        Ok(None) => {}
      }
      if state.transitions.contains(&request.session_key) {
        return response(
          InputDeliveryStatus::NotSent,
          "Session transport is changing; review its control route before sending",
        );
      }
      if !control
        && (state.in_flight.contains(&request.session_key) || state.pending_controls.contains_key(&request.session_key))
      {
        return response(
          InputDeliveryStatus::NotSent,
          "A message is already pending for this session.",
        );
      }
      let pending = response(
        InputDeliveryStatus::Pending,
        "This message is still being sent. Check the conversation before sending again.",
      );
      match state.receipts.admit(
        request.request_id.clone(),
        request.session_key.clone(),
        text_hash,
        pending,
      ) {
        Ok(ReceiptAdmission::Replay(previous)) => return previous,
        Ok(ReceiptAdmission::New) => {
          if control {
            *state.pending_controls.entry(request.session_key.clone()).or_default() += 1;
          } else {
            state.in_flight.insert(request.session_key.clone());
          }
        }
        Err(error) => {
          return response(
            InputDeliveryStatus::NotSent,
            match error {
              ReceiptError::Conflict => "This request ID was already used for a different message.",
              ReceiptError::Full => "Too many messages are being sent. Try again shortly.",
            },
          );
        }
      }
    }

    let broker = self.clone();
    // Once delivery starts it must settle and record its result even if an
    // HTTP client disconnects or the selected conversation changes.
    let pending = tokio::spawn(async move {
      let outcome = tokio::spawn(delivery).await.unwrap_or_else(|_| {
        Err(DeliveryError::uncertain(
          "Delivery was interrupted. The message may have been sent.",
        ))
      });
      let (outcome, model_result) = match outcome {
        Ok(Admission::WithModel { disposition, result }) => (
          Ok(match disposition {
            ModelInputDisposition::Started => Admission::Started,
            ModelInputDisposition::Steered => Admission::Steered,
          }),
          Some(result),
        ),
        Err(mut error) => {
          let result = error.model_result.take();
          (Err(error), result)
        }
        other => (other, None),
      };
      let (status, message) = match outcome {
        Ok(Admission::Accepted) => (InputDeliveryStatus::Accepted, "Agent accepted the input.".into()),
        Ok(Admission::Started) => (
          InputDeliveryStatus::Accepted,
          "Message accepted. Started a turn.".into(),
        ),
        Ok(Admission::QueuedFollowUp) => (
          InputDeliveryStatus::Accepted,
          "Message queued after the current turn.".into(),
        ),
        Ok(Admission::Steered) => (
          InputDeliveryStatus::Accepted,
          "Message accepted as steering for the active turn.".into(),
        ),
        Ok(Admission::WithModel { .. }) => unreachable!("model admission was unwrapped"),
        Err(error) => (
          if error.may_have_been_sent {
            InputDeliveryStatus::Unknown
          } else {
            InputDeliveryStatus::NotSent
          },
          error.message,
        ),
      };
      let result = SubmitSessionInputResponse {
        model_result,
        request_id: request.request_id.clone(),
        status,
        message,
      };
      let mut state = broker.0.lock().unwrap_or_else(|error| error.into_inner());
      if control {
        if let Some(count) = state.pending_controls.get_mut(&request.session_key) {
          *count = count.saturating_sub(1);
          if *count == 0 {
            state.pending_controls.remove(&request.session_key);
          }
        }
      } else {
        state.in_flight.remove(&request.session_key);
      }
      if result.status == InputDeliveryStatus::Unknown {
        state.uncertain_sessions.insert(request.session_key.clone());
        state.receipts.retain_outcome(&request.request_id, result.clone());
      } else {
        state.receipts.complete(&request.request_id, result.clone());
      }
      result
    });
    pending.await.unwrap_or_else(|_| {
      response(
        InputDeliveryStatus::Unknown,
        "Delivery was interrupted. Check the conversation before sending again.",
      )
    })
  }
}

#[cfg(test)]
mod tests;
