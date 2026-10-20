//! Provider-recorded image references. Normalization does not resolve or read them.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageImage {
  /// Stable within its containing message, including across incremental reads.
  pub image_id: String,
  pub mime_type: Option<String>,
  pub name: Option<String>,
  pub source: MessageImageSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageImageSource {
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

impl MessageImage {
  pub fn from_url(image_id: String, reference: String, mime_type: Option<String>, name: Option<String>) -> Self {
    let embedded_mime = reference
      .strip_prefix("data:")
      .and_then(|value| value.split_once(';').or_else(|| value.split_once(',')))
      .map(|(mime, _)| mime.to_owned())
      .filter(|mime| mime.starts_with("image/"));
    let source = if embedded_mime.is_some() {
      MessageImageSource::DataUrl { url: reference }
    } else if let Some(path) = local_file_url(&reference) {
      MessageImageSource::LocalFile { path }
    } else {
      MessageImageSource::Unavailable {
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
      MessageImageSource::DataUrl { .. }
    ));
    assert_eq!(
      image("file:///tmp/hello%20world.png").source,
      MessageImageSource::LocalFile {
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
        MessageImageSource::Unavailable { .. }
      ));
    }
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
