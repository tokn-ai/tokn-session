use super::*;
use crate::images::{
  self, ImageAvailability, ImageTarget, LoadSessionImageRequest, MessageImageSummary, ReleaseImageUploadRequest,
  SessionImage, UploadImageChunkRequest, UploadImageChunkResponse,
};
use tokn_session_core::{RecordedImage, RecordedImageSource};

impl ViewerService {
  /// Image bytes are an explicit resource read, never part of delivery levels.
  pub fn load_session_image(&self, request: LoadSessionImageRequest) -> Result<SessionImage, String> {
    self.validate_session_key(&request.session_key)?;
    let locator = decode_session_key(&request.session_key)?;
    let loaded = self.load_verified(&locator)?;
    let identity = self.window_identity(&locator, &loaded, request.event_key.starts_with("window.v1."))?;
    let local = identity.local_key(&request.event_key)?;
    let index = decode_event_key(&local)?;
    let event = loaded.events.get(index).ok_or("Image event is no longer available")?;
    if event.is_hidden() {
      return Err("Image event is unavailable".into());
    }
    let mut matching = recorded_images(event)
      .iter()
      .filter(|image| display_image_id(&image.image_id) == request.image_id);
    let image = matching.next().ok_or("Image is no longer available in this event")?;
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

pub(super) fn event_images(event: &AgentEvent) -> Vec<MessageImageSummary> {
  if event.is_hidden() {
    return Vec::new();
  }
  recorded_images(event).iter().take(64).map(image_summary).collect()
}

pub(super) fn summary_images(event: &AgentEvent) -> Option<Vec<MessageImageSummary>> {
  if !event.is_hidden() && (matches!(event, AgentEvent::ToolCall(_)) || !recorded_images(event).is_empty()) {
    Some(event_images(event))
  } else {
    None
  }
}

fn recorded_images(event: &AgentEvent) -> &[RecordedImage] {
  match event {
    AgentEvent::Message(message) => &message.images,
    AgentEvent::ToolCall(tool) => &tool.images,
    _ => &[],
  }
}

pub(super) fn operation_images(operation: &ToolOperation) -> Vec<MessageImageSummary> {
  operation
    .images
    .iter()
    .take(64)
    .map(|image| {
      let mut summary = image_summary(&image.image);
      summary.source_event_key = Some(encode_event_key(image.source_event_index));
      summary
    })
    .collect()
}

pub(super) fn event_image_revision(event: &AgentEvent) -> Option<String> {
  (!event.is_hidden() && !recorded_images(event).is_empty())
    .then(|| image_revision(recorded_images(event)))
    .flatten()
}

pub(super) fn operation_image_revision(operation: &ToolOperation) -> Option<String> {
  if operation.images.is_empty() {
    return None;
  }
  // Source keys already identify the owning records. Excluding window-local
  // positions here preserves the resource revision when older history prepends.
  image_revision(&operation.images.iter().map(|entry| &entry.image).collect::<Vec<_>>())
}

pub(super) fn display_tool_output<'a>(
  output: &Value,
  tool_name: Option<&str>,
  images: impl IntoIterator<Item = &'a RecordedImage>,
) -> Value {
  let mut output = output.clone();
  if tool_name == Some("image_generation") {
    let images = images.into_iter().collect::<Vec<_>>();
    let is_image_data = |value: &Value| {
      value.as_str().is_some_and(|text| {
        images.iter().any(|image| match &image.source {
          RecordedImageSource::Base64 { data } => text == data,
          RecordedImageSource::DataUrl { url } => text == url,
          _ => false,
        })
      })
    };
    if is_image_data(&output) {
      return Value::Null;
    }
    if let Value::Object(object) = &mut output {
      if object.get("result").is_some_and(is_image_data) {
        object.remove("result");
        // The card already owns operation status; image-only results need no
        // duplicate JSON box. Other text/error fields remain visible.
        object.remove("status");
      }
      object.remove("saved_path");
      object.remove("savedPath");
    }
  }
  images::sanitize_display_value(&mut output);
  output
}

struct ImageDigest<'a>(&'a mut Sha256);

impl std::io::Write for ImageDigest<'_> {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    self.0.update(bytes);
    Ok(bytes.len())
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

fn image_revision(images: &(impl Serialize + ?Sized)) -> Option<String> {
  let mut digest = Sha256::new();
  // Stream into the digest rather than allocating another base64-sized buffer
  // for every semantic/detail projection of an output image.
  serde_json::to_writer(ImageDigest(&mut digest), images).ok()?;
  Some(format!("{:x}", digest.finalize()))
}

fn image_summary(image: &RecordedImage) -> MessageImageSummary {
  MessageImageSummary {
    image_id: display_image_id(&image.image_id),
    source_event_key: None,
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
    availability: if matches!(image.source, RecordedImageSource::Unavailable { .. }) {
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
  let _ = serde_json::to_writer(ImageDigest(&mut digest), &message.images);
  format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests;
