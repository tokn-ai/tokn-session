use super::*;
use crate::images::{
  self, ImageAvailability, ImageTarget, LoadSessionImageRequest, MessageImageSummary, ReleaseImageUploadRequest,
  SessionImage, UploadImageChunkRequest, UploadImageChunkResponse,
};
use tokn_session_core::{MessageImage, MessageImageSource};

impl ViewerService {
  /// Image bytes are an explicit resource read, never part of delivery levels.
  pub fn load_session_image(&self, request: LoadSessionImageRequest) -> Result<SessionImage, String> {
    self.validate_session_key(&request.session_key)?;
    let locator = decode_session_key(&request.session_key)?;
    let loaded = self.load_verified(&locator)?;
    let identity = self.window_identity(&locator, &loaded, request.event_key.starts_with("window.v1."))?;
    let local = identity.local_key(&request.event_key)?;
    let index = decode_event_key(&local)?;
    let event = loaded.events.get(index).ok_or("Image message is no longer available")?;
    if event.is_hidden() {
      return Err("Image message is unavailable".into());
    }
    let AgentEvent::Message(message) = event else {
      return Err("This event has no message images".into());
    };
    let mut matching = message
      .images
      .iter()
      .filter(|image| display_image_id(&image.image_id) == request.image_id);
    let image = matching.next().ok_or("Image is no longer available in this message")?;
    if matching.next().is_some() {
      return Err("Image identity is ambiguous".into());
    }
    images::image_response(images::read_source(image)?, request.variant)
  }

  pub async fn upload_image_chunk(&self, request: UploadImageChunkRequest) -> Result<UploadImageChunkResponse, String> {
    if self.session_scope.is_some() {
      return Err("Session shares cannot upload images".into());
    }
    match &request.target {
      ImageTarget::Session { session_key } => {
        if request.offset == 0 {
          let status = self
            .get_session_input_status(crate::model::SessionInputStatusRequest {
              session_key: session_key.clone(),
            })
            .await?;
          if !status.available || !status.images_available {
            return Err("Image input is unavailable for this session or model".into());
          }
        } else {
          // Capability discovery can involve native IPC. Check it once at
          // upload start and again at delivery, not once per network chunk.
          let service = self.clone();
          let key = session_key.clone();
          tokio::task::spawn_blocking(move || service.validate_session_key(&key))
            .await
            .map_err(|_| "Could not validate the image session")??;
        }
      }
      ImageTarget::Project { project_id } => {
        let service = self.clone();
        let project_id = project_id.clone();
        tokio::task::spawn_blocking(move || service.validated_project(&project_id))
          .await
          .map_err(|_| "Could not validate the image's project")??;
      }
    }
    let store = self.images.clone();
    tokio::task::spawn_blocking(move || store.upload(request))
      .await
      .map_err(|_| "Image upload task failed")?
  }

  pub fn release_image_upload(&self, request: ReleaseImageUploadRequest) -> Result<(), String> {
    if self.session_scope.is_some() {
      return Err("Session shares cannot release image uploads".into());
    }
    // Tokens remain bound to their original target even when the source or
    // process disappeared; disposal must not require a healthy live agent.
    self.images.release(request)
  }
}

pub(super) fn message_images(event: &AgentEvent) -> Vec<MessageImageSummary> {
  let AgentEvent::Message(message) = event else {
    return Vec::new();
  };
  if event.is_hidden() {
    return Vec::new();
  }
  message.images.iter().take(64).map(image_summary).collect()
}

fn image_summary(image: &MessageImage) -> MessageImageSummary {
  MessageImageSummary {
    image_id: display_image_id(&image.image_id),
    mime_type: image
      .mime_type
      .as_ref()
      .filter(|mime| images::IMAGE_MIME_TYPES.contains(&mime.as_str()))
      .cloned(),
    name: image.name.as_ref().map(|name| {
      name
        .chars()
        .filter(|character| !character.is_control())
        .take(255)
        .collect()
    }),
    availability: if matches!(image.source, MessageImageSource::Unavailable { .. }) {
      ImageAvailability::Unavailable
    } else {
      ImageAvailability::Available
    },
  }
}

fn display_image_id(id: &str) -> String {
  if id.len() <= 256 && !id.chars().any(char::is_control) {
    id.to_owned()
  } else {
    format!("image.sha256.{:x}", Sha256::digest(id.as_bytes()))
  }
}

pub(super) fn message_revision(message: &tokn_session_core::MessageEvent) -> String {
  if message.images.is_empty() {
    return message_content_revision(&message.text);
  }
  let mut digest = Sha256::new();
  digest.update(message.text.as_bytes());
  digest.update([0]);
  // The identity includes source content without publishing it. A changed
  // attachment at a reused legacy event position invalidates frontend blobs.
  if let Ok(bytes) = serde_json::to_vec(&message.images) {
    digest.update(bytes);
  }
  format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests;
