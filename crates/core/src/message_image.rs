//! Provider-recorded image references. Normalization does not resolve or read them.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedImage {
  /// Stable within its containing source record, including incremental reads.
  pub image_id: String,
  pub mime_type: Option<String>,
  pub name: Option<String>,
  pub source: RecordedImageSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordedImageSource {
  DataUrl {
    url: String,
  },
  Base64 {
    data: String,
  },
  LocalFile {
    path: PathBuf,
  },
  /// Remote URLs and opaque provider references are never fetched implicitly.
  Unavailable {
    reference: Option<String>,
  },
}

/// Compatibility name for image references recorded in message events.
pub type MessageImage = RecordedImage;

/// Compatibility name for message image sources. The serialized shape is unchanged.
pub type MessageImageSource = RecordedImageSource;

impl RecordedImage {
  pub fn from_url(image_id: String, reference: String, mime_type: Option<String>, name: Option<String>) -> Self {
    let embedded_mime = reference
      .strip_prefix("data:")
      .and_then(|value| value.split_once(';').or_else(|| value.split_once(',')))
      .map(|(mime, _)| mime.to_owned())
      .filter(|mime| mime.starts_with("image/"));
    let source = if embedded_mime.is_some() {
      RecordedImageSource::DataUrl { url: reference }
    } else if let Some(path) = local_file_url(&reference) {
      RecordedImageSource::LocalFile { path }
    } else {
      RecordedImageSource::Unavailable {
        reference: Some(reference),
      }
    };
    Self {
      image_id,
      mime_type: mime_type.or(embedded_mime),
      name,
      source,
    }
  }
}

/// Omit known image bodies and source references from an explicit display value.
/// Call this on a clone: normalized IR and provider-native exports retain their
/// original resources. Ordinary text and non-image file references are preserved.
pub fn omit_image_resources(value: &mut serde_json::Value) {
  use serde_json::Value;
  match value {
    Value::Object(object) => {
      let generation = object.get("type").and_then(Value::as_str).is_some_and(|kind| {
        matches!(
          kind,
          "image_generation_call" | "image_generation_end" | "ImageGeneration" | "image_gen.generation"
        )
      }) || object.get("tool_name").and_then(Value::as_str) == Some("image_generation");
      if generation {
        for key in ["result", "output", "saved_path", "savedPath"] {
          if object.contains_key(key) {
            object.insert(key.into(), Value::String("[Image resource omitted]".into()));
          }
        }
      }
      let image_file = object.get("type").and_then(Value::as_str) == Some("file")
        && ["mime", "mime_type"].into_iter().any(|key| {
          object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.starts_with("image/"))
        });
      if image_file && object.contains_key("source") {
        object.insert("source".into(), Value::String("[Image resource omitted]".into()));
      }
      let image = image_file
        || object.get("type").and_then(Value::as_str).is_some_and(|kind| {
          matches!(
            kind,
            "image"
              | "input_image"
              | "output_image"
              | "local_image"
              | "localImage"
              | "image_url"
              | "data_url"
              | "base64"
              | "local_file"
          )
        });
      if image {
        for key in ["data", "url", "image_url", "path"] {
          if object.contains_key(key) {
            object.insert(key.into(), Value::String("[Image resource omitted]".into()));
          }
        }
      }
      for (key, child) in object.iter_mut() {
        if matches!(key.as_str(), "images" | "local_images") {
          if let Value::Array(values) = child {
            for value in values {
              if value.is_string() {
                *value = Value::String("[Image resource omitted]".into());
              } else {
                omit_image_resources(value);
              }
            }
            continue;
          }
        }
        omit_image_resources(child);
      }
    }
    Value::Array(values) => {
      for child in values {
        omit_image_resources(child);
      }
    }
    Value::String(text) if text.starts_with("data:image/") => {
      *text = "[Image resource omitted]".into();
    }
    _ => {}
  }
}

fn local_file_url(reference: &str) -> Option<PathBuf> {
  // Only local, absolute file URLs. Do not interpret remote authorities or
  // strip query strings into a different filesystem reference.
  let path = reference.strip_prefix("file://")?;
  let path = path
    .strip_prefix("localhost/")
    .map(|value| format!("/{value}"))
    .unwrap_or_else(|| path.to_owned());
  if !path.starts_with('/') || path.contains(['?', '#']) {
    return None;
  }
  let mut decoded = Vec::with_capacity(path.len());
  let mut bytes = path.bytes();
  while let Some(byte) = bytes.next() {
    if byte == b'%' {
      let high = (bytes.next()? as char).to_digit(16)?;
      let low = (bytes.next()? as char).to_digit(16)?;
      let byte = ((high << 4) | low) as u8;
      if byte == 0 {
        return None;
      }
      decoded.push(byte);
    } else {
      decoded.push(byte);
    }
  }
  String::from_utf8(decoded).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn classifies_recorded_references_without_fetching() {
    let image = |url: &str| MessageImage::from_url("image.v1.0".into(), url.into(), None, None);
    assert!(matches!(
      image("data:image/png;base64,YQ==").source,
      RecordedImageSource::DataUrl { .. }
    ));
    assert_eq!(
      image("file:///tmp/hello%20world.png").source,
      RecordedImageSource::LocalFile {
        path: PathBuf::from("/tmp/hello world.png")
      }
    );
    for reference in [
      "https://example.com/image.png",
      "file://remote/tmp/image.png",
      "file:///tmp/image.png?x",
      "file:///tmp/%00.png",
      "file:///tmp/%xx.png",
      "data:text/plain;base64,YQ==",
    ] {
      assert!(matches!(
        image(reference).source,
        RecordedImageSource::Unavailable { .. }
      ));
    }
  }

  #[test]
  fn display_omits_image_file_sources_but_keeps_document_paths_and_text() {
    let original = serde_json::json!({"content":[
      {"type":"text", "text":"Readable result"},
      {"type":"image", "data":"mcp-binary", "mimeType":"image/png"},
      {"type":"output_image", "image_url":"data:image/png;base64,output-binary"},
      {"type":"file", "mime":"image/png", "url":"file:///private/image.png", "source":{"path":"/private/attached-image.png"}},
      {"type":"file", "mime_type":"image/jpeg", "path":"/private/photo.jpg", "source":{"url":"opaque-image"}},
      {"type":"file", "mime":"application/pdf", "url":"file:///project/report.pdf", "source":{"path":"/project/report.pdf"}},
      {"type":"file", "path":"/project/untyped.txt", "source":{"path":"/project/untyped.txt"}}
    ], "error":"Keep this diagnostic"});
    let mut display = original.clone();
    omit_image_resources(&mut display);
    assert_eq!(display["content"][0], original["content"][0]);
    assert_eq!(display["error"], original["error"]);
    for (index, field) in [
      (1, "data"),
      (2, "image_url"),
      (3, "url"),
      (3, "source"),
      (4, "path"),
      (4, "source"),
    ] {
      assert_eq!(display["content"][index][field], "[Image resource omitted]");
    }
    assert_eq!(display["content"][5], original["content"][5]);
    assert_eq!(display["content"][6], original["content"][6]);
  }

  #[test]
  fn display_omits_generation_bodies_and_saved_paths_but_keeps_diagnostics() {
    let mut display = serde_json::json!({"type":"image_generation_call", "result":"generated-binary", "output":"output-binary",
      "saved_path":"/private/output.png", "savedPath":"/private/second.png", "status":"failed", "error":"partial failure"});
    omit_image_resources(&mut display);
    for field in ["result", "output", "saved_path", "savedPath"] {
      assert_eq!(display[field], "[Image resource omitted]");
    }
    assert_eq!(display["status"], "failed");
    assert_eq!(display["error"], "partial failure");
  }

  #[test]
  fn canonical_and_compatibility_names_keep_the_recorded_image_wire_shape() {
    let image = RecordedImage {
      image_id: "output.v1.0".into(),
      mime_type: Some("image/png".into()),
      name: Some("diagram.png".into()),
      source: RecordedImageSource::Base64 { data: "YQ==".into() },
    };
    let legacy: MessageImage = image.clone();
    let source: MessageImageSource = RecordedImageSource::Base64 { data: "YQ==".into() };
    assert_eq!(legacy.source, source);
    let serialized = serde_json::to_value(&image).unwrap();
    assert_eq!(
      serialized,
      serde_json::json!({
        "image_id":"output.v1.0", "mime_type":"image/png", "name":"diagram.png",
        "source":{"type":"base64", "data":"YQ=="}
      })
    );
    assert_eq!(serde_json::from_value::<MessageImage>(serialized).unwrap(), image);
  }

  #[test]
  fn legacy_tool_records_default_to_no_output_images() {
    let event: crate::ToolCallEvent = serde_json::from_value(serde_json::json!({
      "provider":"pi", "tool_kind":"unknown", "phase":"finished"
    }))
    .unwrap();
    assert!(event.images.is_empty());
    assert!(serde_json::to_value(event).unwrap().get("images").is_none());
  }

  #[test]
  fn legacy_messages_default_to_no_images() {
    let event: crate::MessageEvent = serde_json::from_value(serde_json::json!({
      "provider":"pi", "session_id":null, "message_id":null, "parent_id":null,
      "role":"user", "delivery":"unspecified", "phase":"finished", "text":"hello", "timestamp":null
    }))
    .unwrap();
    assert!(event.images.is_empty());
    assert!(serde_json::to_value(event).unwrap().get("images").is_none());
  }
}
