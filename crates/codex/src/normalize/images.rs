//! Recorded Codex image inputs. No URL is fetched or local path read here.
use serde_json::Value;
use tokn_codex_protocol::ContentItem;
use tokn_session_core::{MessageImage, MessageImageSource};

fn image_id(index: usize) -> String {
  format!("image.v1.{index}")
}

pub(super) fn content_image(content: &Value, index: usize) -> Option<MessageImage> {
  match content.get("type").and_then(Value::as_str)? {
    "image" | "input_image" | "output_image" => {
      if let Some(url) = content.get("image_url").and_then(Value::as_str) {
        Some(MessageImage::from_url(image_id(index), url.into(), None, None))
      } else {
        Some(MessageImage {
          image_id: image_id(index),
          mime_type: None,
          name: None,
          source: MessageImageSource::Unavailable {
            reference: content.get("file_id").and_then(Value::as_str).map(str::to_owned),
          },
        })
      }
    }
    "local_image" => Some(MessageImage {
      image_id: image_id(index),
      mime_type: None,
      name: None,
      source: content
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(|path| MessageImageSource::LocalFile { path: path.into() })
        .unwrap_or(MessageImageSource::Unavailable { reference: None }),
    }),
    _ => None,
  }
}

pub(super) fn response_images(content: &[ContentItem]) -> Vec<MessageImage> {
  content
    .iter()
    .enumerate()
    .filter_map(|(index, item)| content_image(&serde_json::to_value(item).ok()?, index))
    .collect()
}

pub(super) fn legacy_user_images(payload: &Value) -> Vec<MessageImage> {
  let inline = strings(payload.get("images"));
  let files = strings(payload.get("file_ids"));
  let mut inline = inline.into_iter();
  let mut files = files.into_iter();
  let mut ordered = Vec::new();
  if let Some(order) = payload.get("image_order").and_then(Value::as_array) {
    for kind in order {
      match kind.as_str() {
        Some("inline") => {
          if let Some(url) = inline.next() {
            ordered.push(MessageImage::from_url(image_id(ordered.len()), url, None, None));
          }
        }
        Some("file") => {
          if let Some(reference) = files.next() {
            ordered.push(unavailable(ordered.len(), reference));
          }
        }
        _ => {}
      }
    }
  }
  for url in inline {
    ordered.push(MessageImage::from_url(image_id(ordered.len()), url, None, None));
  }
  for reference in files {
    ordered.push(unavailable(ordered.len(), reference));
  }
  for path in strings(payload.get("local_images")) {
    ordered.push(MessageImage {
      image_id: image_id(ordered.len()),
      mime_type: None,
      name: None,
      source: MessageImageSource::LocalFile { path: path.into() },
    });
  }
  ordered
}

fn unavailable(index: usize, reference: String) -> MessageImage {
  MessageImage {
    image_id: image_id(index),
    mime_type: None,
    name: None,
    source: MessageImageSource::Unavailable {
      reference: Some(reference),
    },
  }
}

fn strings(value: Option<&Value>) -> Vec<String> {
  value
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .map(str::to_owned)
    .collect()
}
