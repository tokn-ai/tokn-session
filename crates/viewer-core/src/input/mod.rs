//! Native live-session input shared by desktop and HTTP clients.

pub(crate) mod codex;
pub(crate) mod pi;

use crate::images::ResolvedImage;
use crate::model::{SubmitSessionInputRequest, SubmitSessionInputResponse, ViewerProvider};
use std::{future::Future, path::PathBuf};

pub(crate) use tokn_interaction_core::admission::validate_request;
pub(crate) use tokn_interaction_core::managed_sessions::MAX_INPUT_LENGTH;

#[derive(Clone, Debug)]
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

pub(crate) use tokn_interaction_core::managed_sessions::{Admission, DeliveryError};

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

pub(crate) async fn deliver(
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
    ViewerProvider::Codex => {
      if request.model_selection.is_some() {
        return Err(DeliveryError::not_sent(
          "Codex model selection is unavailable without a verified Desktop catalog",
        ));
      }
      codex::submit_images(&target, &request.request_id, &request.text, &images).await
    }
    ViewerProvider::Pi => pi::submit_model_images(&target, &request, &images).await,
    _ => Err(DeliveryError::not_sent(
      "Message input is not available for this provider yet.",
    )),
  }
}

/// Host boundary wrapper: verification produces an opaque receipt scope; the
/// shared core owns admission, cancellation-safe delivery, and retained receipts.
#[derive(Clone, Default)]
pub(crate) struct InputBroker(pub(crate) tokn_interaction_core::admission::InputBroker);
impl InputBroker {
  pub(crate) fn has_unresolved(&self, target: &InputTarget) -> bool {
    self.0.has_unresolved(&target.identity())
  }
  #[cfg(test)]
  pub(crate) fn begin_transition(
    &self,
    target: &InputTarget,
  ) -> Result<tokn_interaction_core::admission::TransportTransition, String> {
    self.0.begin_transition(&target.identity())
  }
  pub(crate) fn receipt(
    &self,
    target: &InputTarget,
    request: &SubmitSessionInputRequest,
  ) -> Option<SubmitSessionInputResponse> {
    self.0.receipt(&target.identity(), request)
  }
  pub(crate) fn control_receipt(
    &self,
    target: &InputTarget,
    request: &SubmitSessionInputRequest,
  ) -> Option<SubmitSessionInputResponse> {
    self.0.control_receipt(&target.identity(), request)
  }
  pub(crate) async fn submit_custom<F>(
    &self,
    target: &InputTarget,
    request: SubmitSessionInputRequest,
    delivery: F,
  ) -> SubmitSessionInputResponse
  where
    F: Future<Output = Result<Admission, DeliveryError>> + Send + 'static,
  {
    self.0.submit_custom(&target.identity(), request, delivery).await
  }
  pub(crate) async fn submit_control<F>(
    &self,
    target: &InputTarget,
    request: SubmitSessionInputRequest,
    delivery: F,
  ) -> SubmitSessionInputResponse
  where
    F: Future<Output = Result<Admission, DeliveryError>> + Send + 'static,
  {
    self.0.submit_control(&target.identity(), request, delivery).await
  }
}
#[cfg(test)]
mod tests;
