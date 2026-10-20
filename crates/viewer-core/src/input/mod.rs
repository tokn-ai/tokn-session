//! Native live-session input shared by desktop and HTTP clients.

pub(crate) mod codex;
mod pi;

use crate::images::ResolvedImage;
use crate::model::{InputDeliveryStatus, SubmitSessionInputRequest, SubmitSessionInputResponse, ViewerProvider};
use sha2::{Digest, Sha256};
use std::{
  collections::{HashMap, HashSet, VecDeque},
  future::Future,
  path::PathBuf,
  sync::{Arc, Mutex},
};

pub(crate) const MAX_INPUT_LENGTH: usize = 16 * 1024;
const MAX_RECENT_REQUESTS: usize = 1024;

#[derive(Debug)]
pub(crate) struct InputTarget {
  pub provider: ViewerProvider,
  pub session_id: String,
  pub session_file: PathBuf,
}

impl InputTarget {
  pub(crate) fn identity(&self) -> String {
    // Client keys can have equivalent encodings. Codex ownership is by thread
    // ID; Pi ownership is by the catalog's exact session file and ID.
    if self.provider == ViewerProvider::Codex {
      serde_json::json!([self.provider.as_str(), self.session_id]).to_string()
    } else {
      serde_json::json!([self.provider.as_str(), self.session_id, self.session_file]).to_string()
    }
  }
}

#[derive(Clone, Debug)]
pub(crate) struct DeliveryError {
  pub message: String,
  pub may_have_been_sent: bool,
}

impl DeliveryError {
  pub fn not_sent(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      may_have_been_sent: false,
    }
  }

  pub fn uncertain(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      may_have_been_sent: true,
    }
  }
}

#[derive(Clone, Debug)]
pub(crate) enum Admission {
  Accepted,
  Started,
  QueuedFollowUp,
}

#[derive(Clone, Debug)]
pub(crate) struct InputCapability {
  pub message: String,
  pub images_available: bool,
}

pub(crate) async fn status(target: &InputTarget) -> Result<InputCapability, DeliveryError> {
  match target.provider {
    ViewerProvider::Codex => codex::status(target).await.map(|message| InputCapability {
      message,
      images_available: true,
    }),
    ViewerProvider::Pi => pi::status(target).await,
    _ => Err(DeliveryError::not_sent(
      "Message input is not available for this provider yet.",
    )),
  }
}

async fn deliver(
  target: InputTarget,
  request: SubmitSessionInputRequest,
  images: Vec<ResolvedImage>,
) -> Result<Admission, DeliveryError> {
  if request.image_ids.len() != images.len()
    || request
      .image_ids
      .iter()
      .zip(&images)
      .any(|(id, image)| id != &image.image_id)
  {
    return Err(DeliveryError::not_sent(
      "Image attachments did not match the message request",
    ));
  }
  match target.provider {
    ViewerProvider::Codex => codex::submit_images(&target, &request.request_id, &request.text, &images).await,
    ViewerProvider::Pi => pi::submit_images(&target, &request.request_id, &request.text, &images).await,
    _ => Err(DeliveryError::not_sent(
      "Message input is not available for this provider yet.",
    )),
  }
}

pub(crate) fn validate_request(request: &mut SubmitSessionInputRequest) -> Result<(), String> {
  if uuid::Uuid::parse_str(&request.request_id).is_err() {
    return Err("Invalid message request ID".into());
  }
  if request.text.trim().is_empty() && request.image_ids.is_empty() {
    return Err("Message cannot be empty".into());
  }
  if request.text.chars().count() > MAX_INPUT_LENGTH {
    return Err(format!("Message must be at most {MAX_INPUT_LENGTH} characters"));
  }
  if request
    .text
    .chars()
    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
  {
    return Err("Message contains unsupported control characters".into());
  }
  validate_image_ids(&request.image_ids)?;
  Ok(())
}

pub(crate) fn validate_image_ids(ids: &[String]) -> Result<(), String> {
  if ids.len() > crate::images::MAX_IMAGES {
    return Err(format!("Attach at most {} images", crate::images::MAX_IMAGES));
  }
  let mut seen = HashSet::new();
  for id in ids {
    if uuid::Uuid::parse_str(id).is_err() || !seen.insert(id) {
      return Err("Invalid or duplicate image attachment ID".into());
    }
  }
  Ok(())
}

/// Preserve text-only fingerprints while binding image messages to their ordered
/// immutable upload identities. Clients can never substitute host paths.
pub(crate) fn message_fingerprint(text: &str, image_ids: &[String]) -> [u8; 32] {
  if image_ids.is_empty() {
    return Sha256::digest(text.as_bytes()).into();
  }
  Sha256::digest(serde_json::to_vec(&(text, image_ids)).expect("string message fingerprint")).into()
}

#[derive(Default)]
struct RequestState {
  recent: HashMap<String, RecordedRequest>,
  completed: VecDeque<String>,
  in_flight: HashSet<String>,
}

struct RecordedRequest {
  session_key: String,
  text_hash: [u8; 32],
  response: SubmitSessionInputResponse,
}

#[derive(Clone, Default)]
pub(crate) struct InputBroker(Arc<Mutex<RequestState>>);

impl InputBroker {
  pub async fn submit_images(
    &self,
    target: InputTarget,
    mut request: SubmitSessionInputRequest,
    images: Vec<ResolvedImage>,
  ) -> SubmitSessionInputResponse {
    request.session_key = target.identity();
    let delivery_request = request.clone();
    self
      .submit_with(request, deliver(target, delivery_request, images))
      .await
  }

  pub(crate) fn receipt(
    &self,
    target: &InputTarget,
    request: &SubmitSessionInputRequest,
  ) -> Option<SubmitSessionInputResponse> {
    let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
    let previous = state.recent.get(&request.request_id)?;
    let hash: [u8; 32] = message_fingerprint(&request.text, &request.image_ids);
    Some(
      if previous.session_key == target.identity() && previous.text_hash == hash {
        previous.response.clone()
      } else {
        SubmitSessionInputResponse {
          request_id: request.request_id.clone(),
          status: InputDeliveryStatus::NotSent,
          message: "This request ID was already used for a different message.".into(),
        }
      },
    )
  }

  pub(crate) async fn submit_custom<F>(
    &self,
    target: &InputTarget,
    mut request: SubmitSessionInputRequest,
    delivery: F,
  ) -> SubmitSessionInputResponse
  where
    F: Future<Output = Result<Admission, DeliveryError>> + Send + 'static,
  {
    request.session_key = target.identity();
    self.submit_with(request, delivery).await
  }

  async fn submit_with<F>(&self, request: SubmitSessionInputRequest, delivery: F) -> SubmitSessionInputResponse
  where
    F: Future<Output = Result<Admission, DeliveryError>> + Send + 'static,
  {
    let text_hash: [u8; 32] = message_fingerprint(&request.text, &request.image_ids);
    let request_id = request.request_id.clone();
    let response = |status, message: &str| SubmitSessionInputResponse {
      request_id: request_id.clone(),
      status,
      message: message.into(),
    };
    {
      let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(previous) = state.recent.get(&request.request_id) {
        return if previous.session_key == request.session_key && previous.text_hash == text_hash {
          previous.response.clone()
        } else {
          response(
            InputDeliveryStatus::NotSent,
            "This request ID was already used for a different message.",
          )
        };
      }
      if state.in_flight.contains(&request.session_key) {
        return response(
          InputDeliveryStatus::NotSent,
          "A message is already being sent to this session.",
        );
      }
      while state.recent.len() >= MAX_RECENT_REQUESTS {
        let Some(oldest) = state.completed.pop_front() else {
          return response(
            InputDeliveryStatus::NotSent,
            "Too many messages are being sent. Try again shortly.",
          );
        };
        state.recent.remove(&oldest);
      }
      state.in_flight.insert(request.session_key.clone());
      state.recent.insert(
        request.request_id.clone(),
        RecordedRequest {
          session_key: request.session_key.clone(),
          text_hash,
          response: response(
            InputDeliveryStatus::Pending,
            "This message is still being sent. Check the conversation before sending again.",
          ),
        },
      );
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
      let (status, message) = match outcome {
        Ok(Admission::Accepted) => (InputDeliveryStatus::Accepted, "Codex App accepted the message.".into()),
        Ok(Admission::Started) => (
          InputDeliveryStatus::Accepted,
          "Message accepted. Pi started a turn.".into(),
        ),
        Ok(Admission::QueuedFollowUp) => (
          InputDeliveryStatus::Accepted,
          "Message queued after Pi's current turn.".into(),
        ),
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
        request_id: request.request_id.clone(),
        status,
        message,
      };
      let mut state = broker.0.lock().unwrap_or_else(|error| error.into_inner());
      state.in_flight.remove(&request.session_key);
      if let Some(record) = state.recent.get_mut(&request.request_id) {
        record.response = result.clone();
      }
      state.completed.push_back(request.request_id);
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
