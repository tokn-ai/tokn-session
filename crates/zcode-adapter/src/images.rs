//! Bounded native attachment staging; uploaded artifacts never imply input admission.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};

const MAX_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 24 * 1024 * 1024;
const CHUNK_BYTES: usize = 512 * 1024;

pub(super) struct PreparedImage {
  bytes: Vec<u8>,
  mime: String,
  file_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadState {
  upload_id: String,
  state: String,
  next_chunk_index: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChunkState {
  upload_id: String,
  next_chunk_index: usize,
}

#[derive(Deserialize)]
struct Committed {
  #[serde(rename = "ref")]
  artifact_ref: String,
}

pub(super) fn prepare(images: Vec<ImageInput>) -> Result<Vec<PreparedImage>, InteractionError> {
  if images.len() > MAX_IMAGES {
    return Err(InteractionError::invalid(
      "ZCode supports at most four images per input",
    ));
  }
  let mut total = 0_usize;
  images
    .into_iter()
    .enumerate()
    .map(|(index, image)| {
      let extension = match image.mime_type.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => return Err(InteractionError::invalid("Unsupported ZCode image MIME type")),
      };
      if image.data.len() > 4 * MAX_IMAGE_BYTES.div_ceil(3) {
        return Err(InteractionError::invalid("ZCode image exceeds the 8 MiB limit"));
      }
      let bytes = STANDARD
        .decode(image.data)
        .map_err(|_| InteractionError::invalid("ZCode image data must be valid base64"))?;
      total = total.saturating_add(bytes.len());
      if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES || total > MAX_TOTAL_BYTES {
        return Err(InteractionError::invalid(
          "ZCode images must be nonempty, at most 8 MiB each and 24 MiB per input",
        ));
      }
      Ok(PreparedImage {
        bytes,
        mime: image.mime_type,
        file_name: format!("input-{}.{}", index + 1, extension),
      })
    })
    .collect()
}

impl Inner {
  pub(super) async fn stage_images(
    &self,
    session_id: &str,
    images: Vec<PreparedImage>,
  ) -> Result<Vec<Value>, InteractionError> {
    let mut attachments = Vec::with_capacity(images.len());
    for image in images {
      let upload_id = uuid::Uuid::new_v4().to_string();
      let result = self.stage_image(session_id, &upload_id, image).await;
      match result {
        Ok(attachment) => attachments.push(attachment),
        Err(_) => {
          // No sendText has been dispatched. Even an uncertain commit can only
          // leave a native orphan artifact, never an admitted user input.
          if self.transport.is_live() {
            let _ = self
              .transport
              .call(
                "v4/attachment/abort",
                self.upload_identity(session_id, &upload_id),
                true,
              )
              .await;
          }
          return Err(InteractionError::new(
            ErrorKind::Rejected,
            "ZCode image upload did not complete; the input was not sent",
          ));
        }
      }
    }
    Ok(attachments)
  }

  fn upload_identity(&self, session_id: &str, upload_id: &str) -> Value {
    json!({
      "connectionId":self.descriptor.instance_id,
      "sessionId":session_id,
      "uploadId":upload_id,
    })
  }

  async fn stage_image(
    &self,
    session_id: &str,
    upload_id: &str,
    image: PreparedImage,
  ) -> Result<Value, InteractionError> {
    let mut begin = self.upload_identity(session_id, upload_id);
    begin["fileName"] = json!(image.file_name);
    begin["mime"] = json!(image.mime);
    begin["totalBytes"] = json!(image.bytes.len());
    begin["totalChunks"] = json!(image.bytes.len().div_ceil(CHUNK_BYTES));
    begin["checksum"] = json!(format!("sha256:{:x}", Sha256::digest(&image.bytes)));
    // Verified CLI 0.17.1 schema: workspace is not accepted here. The managed
    // bound session already establishes ownership and project identity.
    let state: UploadState = wire::decode(self.transport.call("v4/attachment/begin", begin, true).await?, true)?;
    if state.upload_id != upload_id || state.state != "staging" || state.next_chunk_index != 0 {
      return Err(InteractionError::uncertain(
        "ZCode returned a different attachment upload identity",
      ));
    }
    for (index, bytes) in image.bytes.chunks(CHUNK_BYTES).enumerate() {
      let mut chunk = self.upload_identity(session_id, upload_id);
      chunk["chunkIndex"] = json!(index);
      chunk["dataBase64"] = json!(STANDARD.encode(bytes));
      let next: ChunkState = wire::decode(self.transport.call("v4/attachment/chunk", chunk, true).await?, true)?;
      if next.upload_id != upload_id || next.next_chunk_index != index + 1 {
        return Err(InteractionError::uncertain(
          "ZCode returned a different attachment chunk identity",
        ));
      }
    }
    let committed: Committed = wire::decode(
      self
        .transport
        .call(
          "v4/attachment/commit",
          self.upload_identity(session_id, upload_id),
          true,
        )
        .await?,
      true,
    )?;
    if !valid_artifact_ref(session_id, &committed.artifact_ref) {
      return Err(InteractionError::uncertain(
        "ZCode returned an attachment outside the bound session",
      ));
    }
    Ok(json!({
      "ref":committed.artifact_ref,
      "fileName":image.file_name,
      "mime":image.mime,
      "bytes":image.bytes.len(),
    }))
  }
}

fn valid_artifact_ref(session_id: &str, value: &str) -> bool {
  let prefix = format!("zcode-artifact://{session_id}/");
  value.len() <= 1024
    && value.strip_prefix(&prefix).is_some_and(|id| {
      !id.is_empty()
        && !matches!(id, "." | "..")
        && id
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    })
}

#[cfg(test)]
mod tests;
