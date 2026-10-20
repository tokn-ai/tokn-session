//! Recorded Markdown images use the same scoped resource reads as native images.
//! Parsing discovers references only; it never opens a file or fetches a URL.
use super::MarkdownImageRange;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};
use tokn_session_core::RecordedImage;

pub(crate) fn message_images(text: &str) -> Vec<(RecordedImage, MarkdownImageRange)> {
  if !text.contains("![") {
    return Vec::new();
  }
  let options =
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_FOOTNOTES;
  let mut images = Vec::new();
  let mut pending = None;
  let mut depth = 0;
  let mut alt = String::new();
  let mut previous_byte = 0;
  let mut previous_utf16 = 0;
  for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
    match event {
      Event::Start(Tag::Image { dest_url, .. }) => {
        if depth == 0 {
          pending = Some((dest_url.into_string(), range));
          alt.clear();
        }
        depth += 1;
      }
      Event::End(TagEnd::Image) => {
        depth -= 1;
        if depth != 0 {
          continue;
        }
        let Some((destination, span)) = pending.take() else {
          continue;
        };
        // Embedded Markdown bytes need display-text redaction and rebased
        // ranges before they can be a separate resource. Keep their existing
        // inert placeholder; native structured image data remains supported.
        if destination.starts_with("data:") {
          continue;
        }
        // JS/HAST positions count UTF-16 code units, while Rust's parser uses
        // UTF-8 byte offsets. Convert incrementally for non-ASCII messages.
        let start = previous_utf16 + text[previous_byte..span.start].encode_utf16().count();
        let end = start + text[span.clone()].encode_utf16().count();
        previous_byte = span.end;
        previous_utf16 = end;
        let image_id = format!("markdown.v1.{start}.{end}.{:x}", Sha256::digest(destination.as_bytes()));
        // Plain absolute Markdown destinations are URL paths. Reuse the strict
        // file-URL decoder for escapes, authorities, queries, and NUL checks.
        let reference = if destination.starts_with('/') && !destination.starts_with("//") {
          format!("file://{destination}")
        } else {
          destination
        };
        let name: String = alt
          .chars()
          .filter(|character| !character.is_control())
          .take(255)
          .collect();
        let image = RecordedImage::from_url(image_id, reference, None, (!name.is_empty()).then_some(name));
        images.push((image, MarkdownImageRange { start, end }));
        if images.len() == 64 {
          break;
        }
      }
      Event::Text(value) | Event::Code(value) if depth > 0 => {
        // Bound labels even when a malformed/large reference contains huge alt
        // text. Image destinations themselves retain the source's exact bytes.
        if alt.len() < 1024 {
          alt.extend(value.chars().take(255));
        }
      }
      Event::SoftBreak | Event::HardBreak if depth > 0 => alt.push(' '),
      _ => {}
    }
  }
  images
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;
  use tokn_session_core::RecordedImageSource;

  #[test]
  fn local_markdown_references_keep_exact_utf16_ranges_and_decoded_paths() {
    let source = "🖼️ Updated\n\n![Updated Runtimes screen](/Users/clouds/runtime%20screen.png)\n\n![second](file:///tmp/second.png)";
    let images = message_images(source);
    assert_eq!(images.len(), 2);
    for ((image, range), expected) in images.iter().zip([
      "![Updated Runtimes screen](/Users/clouds/runtime%20screen.png)",
      "![second](file:///tmp/second.png)",
    ]) {
      let utf16: Vec<_> = source.encode_utf16().collect();
      assert_eq!(String::from_utf16(&utf16[range.start..range.end]).unwrap(), expected);
      assert!(image.image_id.starts_with("markdown.v1."));
    }
    assert_eq!(images[0].0.name.as_deref(), Some("Updated Runtimes screen"));
    assert_eq!(
      images[0].0.source,
      RecordedImageSource::LocalFile {
        path: PathBuf::from("/Users/clouds/runtime screen.png")
      }
    );
  }

  #[test]
  fn parses_reference_images_and_linked_images_without_matching_code_or_html() {
    let source = "`![code](/tmp/code.png)`\n\n```md\n![fence](/tmp/fence.png)\n```\n\n<img src=\"/tmp/html.png\">\n\n[![**linked**](/tmp/linked.png)](https://example.com)\n\n![Reference][picture]\n\n[picture]: </tmp/hello world.png>\n";
    let images = message_images(source);
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].0.name.as_deref(), Some("linked"));
    assert_eq!(images[1].0.name.as_deref(), Some("Reference"));
    assert_eq!(
      images[1].0.source,
      RecordedImageSource::LocalFile {
        path: "/tmp/hello world.png".into()
      }
    );
    let start = source.find("![Reference]").unwrap();
    assert_eq!(images[1].1.start, start);
    assert_eq!(images[1].1.end, start + "![Reference][picture]".len());
  }

  #[test]
  fn rejects_remote_relative_and_malformed_local_destinations() {
    for destination in [
      "https://example.com/image.png",
      "//remote/image.png",
      "relative/image.png",
      "../image.png",
      "file://remote/tmp/image.png",
      "/tmp/image.png?query",
      "/tmp/image.png#fragment",
      "/tmp/%00.png",
      "/tmp/%xx.png",
    ] {
      let images = message_images(&format!("![Image]({destination})"));
      assert_eq!(images.len(), 1, "{destination}");
      assert!(
        matches!(images[0].0.source, RecordedImageSource::Unavailable { .. }),
        "{destination}"
      );
    }
  }

  #[test]
  fn bounds_metadata_and_binds_ids_to_each_occurrence_and_destination() {
    let repeated = "![Image](/tmp/one.png)\n".repeat(70);
    let images = message_images(&repeated);
    assert_eq!(images.len(), 64);
    assert_ne!(images[0].0.image_id, images[1].0.image_id);
    assert_ne!(
      message_images("![A](/tmp/one.png)")[0].0.image_id,
      message_images("![A](/tmp/two.png)")[0].0.image_id
    );
  }

  #[test]
  fn embedded_markdown_bytes_remain_outside_the_resource_contract() {
    let source = "![Embedded](data:image/png;base64,YQ==)\n![Local](/tmp/local.png)";
    let images = message_images(source);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].1.start, source.find("![Local]").unwrap());
  }

  #[test]
  fn nested_alt_images_do_not_create_extra_resources() {
    let images = message_images("![outer ![inner](/tmp/inner.png)](/tmp/outer.png)");
    assert_eq!(images.len(), 1);
    assert_eq!(
      images[0].0.source,
      RecordedImageSource::LocalFile {
        path: "/tmp/outer.png".into()
      }
    );
  }
}
