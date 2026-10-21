//! Recorded Codex image inputs and outputs. No URL is fetched or local path read here.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use tokn_codex_protocol::{ContentItem, ResponseItem, RolloutItem};
use tokn_session_core::{MessageImage, MessageImageSource, RecordedImage, RecordedImageSource};

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

/// Extract only explicitly structured output blocks. Arbitrary strings and
/// nested JSON fields are not evidence of an image reference.
pub(super) fn output_images(output: &Value) -> Vec<RecordedImage> {
  let output = output.get("Ok").or_else(|| output.get("ok")).unwrap_or(output);
  let blocks = output
    .as_array()
    .or_else(|| output.get("content").and_then(Value::as_array));
  blocks
    .into_iter()
    .flatten()
    .enumerate()
    .filter_map(|(index, block)| {
      if block.get("type").and_then(Value::as_str) == Some("image")
        && (block.get("data").is_some() || block.get("mimeType").is_some())
      {
        return Some(RecordedImage {
          image_id: image_id(index),
          mime_type: block.get("mimeType").and_then(Value::as_str).map(str::to_owned),
          name: None,
          source: block
            .get("data")
            .and_then(Value::as_str)
            .filter(|data| !data.is_empty())
            .map(|data| RecordedImageSource::Base64 { data: data.into() })
            .unwrap_or(RecordedImageSource::Unavailable { reference: None }),
        });
      }
      content_image(block, index)
    })
    .collect()
}

pub(super) fn path_images(path: Option<&str>) -> Vec<RecordedImage> {
  match path.filter(|path| !path.is_empty()) {
    Some(path) if path.starts_with("file://") => vec![RecordedImage::from_url(image_id(0), path.into(), None, None)],
    path => generated_images(None, path),
  }
}

pub(super) fn generated_images(result: Option<&str>, saved_path: Option<&str>) -> Vec<RecordedImage> {
  let result = result.map(str::trim).filter(|result| !result.is_empty());
  let source = if let Some(result) = result {
    if result.starts_with("data:") {
      return vec![RecordedImage::from_url(image_id(0), result.into(), None, None)];
    }
    if is_base64(result) {
      RecordedImageSource::Base64 { data: result.into() }
    } else {
      RecordedImageSource::Unavailable {
        reference: Some(result.into()),
      }
    }
  } else if let Some(path) = saved_path.filter(|path| !path.is_empty()) {
    if std::path::Path::new(path).is_absolute() {
      RecordedImageSource::LocalFile { path: path.into() }
    } else {
      RecordedImageSource::Unavailable {
        reference: Some(path.into()),
      }
    }
  } else {
    return Vec::new();
  };
  vec![RecordedImage {
    image_id: image_id(0),
    mime_type: None,
    name: None,
    source,
  }]
}

fn is_base64(value: &str) -> bool {
  if value.len() % 4 != 0 {
    return false;
  }
  let value = value
    .strip_suffix("==")
    .or_else(|| value.strip_suffix('='))
    .unwrap_or(value);
  !value.is_empty()
    && value
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
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

/// Native legacy fanout can persist both the Responses image item and its
/// legacy end event. Only consume a matching pair from different formats;
/// repeated records from the same format remain independent observations.
#[derive(Default)]
pub(super) struct GenerationDedup {
  observations: VecDeque<GenerationObservation>,
}

struct GenerationObservation {
  call_id: [u8; 32],
  sources: u8,
  fingerprint: [u8; 32],
}

#[derive(Clone, Copy, PartialEq)]
enum GenerationSource {
  Raw,
  Legacy,
  HostedItem,
  ExtensionItem,
}

impl GenerationDedup {
  pub(super) fn observe(&mut self, item: &RolloutItem, canonical: bool) -> bool {
    let boundary = matches!(item, RolloutItem::SessionMeta(_))
      || matches!(item, RolloutItem::ResponseItem(ResponseItem::Message(message)) if message.role.as_deref() == Some("user"))
      || matches!(item, RolloutItem::EventMessage(event) if matches!(event.event_type.as_deref(), Some("user_message" | "task_started" | "turn_started" | "thread_rolled_back")));
    if boundary || canonical {
      self.observations.clear();
    }
    if canonical {
      return false;
    }
    let (source, call_id, status, result, saved_path) = match item {
      RolloutItem::ResponseItem(ResponseItem::ImageGenerationCall(image)) => (
        GenerationSource::Raw,
        image.id.as_deref(),
        image.status.as_deref(),
        image.result.as_deref(),
        image.extra.get("saved_path").and_then(Value::as_str),
      ),
      RolloutItem::EventMessage(event)
        if event.event_type.as_deref() == Some("image_generation_end")
          && event.native.get("result").is_some_and(Value::is_string)
          && event
            .native
            .get("saved_path")
            .is_none_or(|value| value.is_null() || value.is_string()) =>
      {
        (
          GenerationSource::Legacy,
          event.native.get("call_id").and_then(Value::as_str),
          event.native.get("status").and_then(Value::as_str),
          event.native.get("result").and_then(Value::as_str),
          event.native.get("saved_path").and_then(Value::as_str),
        )
      }
      RolloutItem::EventMessage(event) if event.event_type.as_deref() == Some("item_completed") => {
        let native = &event.native["item"];
        let (source, saved_path) = match (native["type"].as_str(), native["kind"].as_str()) {
          (Some("ImageGeneration"), _) => (GenerationSource::HostedItem, "saved_path"),
          (Some("Extension"), Some("image_gen.generation")) => (GenerationSource::ExtensionItem, "savedPath"),
          _ => return false,
        };
        if !native.get("result").is_some_and(Value::is_string)
          || native
            .get(saved_path)
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
          return false;
        }
        (
          source,
          native["id"].as_str(),
          native["status"].as_str(),
          native["result"].as_str(),
          native[saved_path].as_str(),
        )
      }
      _ => return false,
    };
    let Some(call_id) = call_id.filter(|id| !id.is_empty()) else {
      return false;
    };
    let call_id: [u8; 32] = Sha256::digest(call_id.as_bytes()).into();
    let Some(status @ ("completed" | "failed" | "error" | "cancelled" | "canceled")) = status else {
      return false;
    };
    let mut digest = Sha256::new();
    digest.update(status.as_bytes());
    digest.update([0]);
    // A nonempty result is the exact image body; a saved copy of that same
    // result may appear only in the end event. Compare the path only when it
    // is the sole available image source.
    if let Some(result) = result.map(str::trim).filter(|result| !result.is_empty()) {
      digest.update(b"result\0");
      digest.update(result.as_bytes());
    } else {
      digest.update(b"path\0");
      digest.update(saved_path.unwrap_or_default().as_bytes());
    }
    let fingerprint: [u8; 32] = digest.finalize().into();
    let source = 1 << source as u8;
    if let Some(index) = self
      .observations
      .iter()
      .position(|previous| previous.call_id == call_id)
    {
      let previous = self.observations.remove(index).expect("located generation observation");
      if previous.sources & source == 0 && previous.fingerprint == fingerprint {
        self.observations.push_back(GenerationObservation {
          call_id,
          sources: previous.sources | source,
          fingerprint,
        });
        return true;
      }
    }
    self.observations.push_back(GenerationObservation {
      call_id,
      sources: source,
      fingerprint,
    });
    if self.observations.len() > 64 {
      self.observations.pop_front();
    }
    false
  }
}
