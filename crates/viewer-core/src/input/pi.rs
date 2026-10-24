//! Viewer projection over the native Pi input-extension adapter.
use super::{Admission, DeliveryError, InputCapability, InputTarget};
use crate::{
  images::ResolvedImage,
  model::{SubmitSessionInputRequest, ViewerProvider},
  models::{ListSessionModelsResponse, SessionModelStatus},
};
use tokn_pi_adapter::bridge::{self, BridgeInput, BridgeTarget};

fn bridge_target(target: &InputTarget) -> Result<BridgeTarget, DeliveryError> {
  if target.provider != ViewerProvider::Pi {
    return Err(DeliveryError::not_sent("Input requires a Pi session."));
  }
  Ok(BridgeTarget {
    session_id: target.session_id.clone(),
    session_file: target.session_file.clone(),
  })
}

pub(super) async fn status(target: &InputTarget) -> Result<InputCapability, DeliveryError> {
  let capability = bridge::status(&bridge_target(target)?).await?;
  Ok(InputCapability {
    message: capability.message,
    images_available: capability.images_available,
  })
}

pub(crate) async fn model_status(target: &InputTarget) -> Result<SessionModelStatus, DeliveryError> {
  bridge::model_status(&bridge_target(target)?).await
}

pub(crate) async fn list_models(target: &InputTarget) -> Result<ListSessionModelsResponse, DeliveryError> {
  bridge::list_models(&bridge_target(target)?).await
}

pub(super) async fn submit_model_images(
  target: &InputTarget,
  request: &SubmitSessionInputRequest,
  images: &[ResolvedImage],
) -> Result<Admission, DeliveryError> {
  let request = BridgeInput {
    request_id: request.request_id.clone(),
    text: request.text.clone(),
    model_selection: request.model_selection.clone(),
  };
  let images = images
    .iter()
    .cloned()
    .map(crate::managed_sessions::image_input)
    .collect::<Vec<_>>();
  bridge::submit_model_images(&bridge_target(target)?, &request, &images).await
}
