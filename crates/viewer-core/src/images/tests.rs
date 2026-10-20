use super::*;
use serde_json::json;
use tokn_session_core::{MessageImage, MessageImageSource};

fn png(width: u32, height: u32) -> Vec<u8> {
  let mut output = Cursor::new(Vec::new());
  image::DynamicImage::new_rgb8(width, height)
    .write_to(&mut output, image::ImageFormat::Png)
    .unwrap();
  output.into_inner()
}

fn target(key: &str) -> ImageTarget {
  ImageTarget::Session {
    session_key: key.into(),
  }
}

fn chunk(id: &str, bytes: &[u8], offset: usize, total: usize) -> UploadImageChunkRequest {
  UploadImageChunkRequest {
    target: target("session-a"),
    upload_id: id.into(),
    offset,
    total_bytes: total,
    mime_type: "image/png".into(),
    name: Some("picture.png".into()),
    data_base64: STANDARD.encode(bytes),
  }
}

fn upload(store: &ImageStore) -> (String, String) {
  let id = uuid::Uuid::new_v4().to_string();
  let bytes = png(3, 2);
  let response = store.upload(chunk(&id, &bytes, 0, bytes.len())).unwrap();
  (id, response.image_id.unwrap())
}

#[test]
fn chunks_are_ordered_retryable_and_bound_to_their_target() {
  let store = ImageStore::temporary().unwrap();
  let id = uuid::Uuid::new_v4().to_string();
  let bytes = png(3, 2);
  let midpoint = bytes.len() / 2;
  assert!(
    store
      .upload(chunk(&id, &bytes[midpoint..], midpoint, bytes.len()))
      .is_err()
  );
  let first = store.upload(chunk(&id, &bytes[..midpoint], 0, bytes.len())).unwrap();
  assert_eq!(first.received_bytes, midpoint);
  assert!(first.image_id.is_none());
  assert_eq!(
    store
      .upload(chunk(&id, &bytes[..midpoint], 0, bytes.len()))
      .unwrap()
      .received_bytes,
    midpoint
  );
  let mut conflict = bytes[..midpoint].to_vec();
  conflict[0] ^= 1;
  assert!(store.upload(chunk(&id, &conflict, 0, bytes.len())).is_err());
  let mut wrong_target = chunk(&id, &bytes[midpoint..], midpoint, bytes.len());
  wrong_target.target = target("other-session");
  assert!(store.upload(wrong_target).is_err());
  let complete = store
    .upload(chunk(&id, &bytes[midpoint..], midpoint, bytes.len()))
    .unwrap();
  let image_id = complete.image_id.unwrap();
  assert_ne!(image_id, id);
  assert_eq!(
    store
      .upload(chunk(&id, &bytes[midpoint..], midpoint, bytes.len()))
      .unwrap()
      .image_id
      .as_ref(),
    Some(&image_id)
  );
  assert!(
    store
      .resolve(&target("other-session"), std::slice::from_ref(&image_id))
      .is_err()
  );
  assert!(
    store
      .resolve(&target("session-a"), &[image_id.clone(), image_id.clone()])
      .is_err()
  );
  let resolved = store.resolve(&target("session-a"), &[image_id]).unwrap();
  assert_eq!(*resolved[0].bytes, bytes);
}

#[test]
fn invalid_uploads_never_become_deliverable() {
  let store = ImageStore::temporary().unwrap();
  for (bytes, mime) in [(b"<svg></svg>".to_vec(), "image/png"), (png(2, 2), "image/jpeg")] {
    let id = uuid::Uuid::new_v4().to_string();
    let mut request = chunk(&id, &bytes, 0, bytes.len());
    request.mime_type = mime.into();
    assert!(store.upload(request).is_err());
    assert!(store.0.lock().unwrap().uploads[&id].image_id.is_none());
  }
  let mut request = chunk(&uuid::Uuid::new_v4().to_string(), &[1], 0, MAX_IMAGE_BYTES + 1);
  assert!(store.upload(request.clone()).is_err());
  request.total_bytes = 1;
  request.name = Some("bad\nname.png".into());
  assert!(store.upload(request).is_err());
}

#[test]
fn release_removes_unused_images_but_retained_provider_files_survive() {
  let directory = tempfile::tempdir().unwrap();
  let store = ImageStore::persistent(directory.path().join("images")).unwrap();
  let (unused_upload, unused_id) = upload(&store);
  let unused_path = store.resolve(&target("session-a"), &[unused_id]).unwrap()[0]
    .path
    .clone();
  assert!(
    store
      .release(ReleaseImageUploadRequest {
        target: target("wrong"),
        upload_id: unused_upload.clone()
      })
      .is_err()
  );
  store
    .release(ReleaseImageUploadRequest {
      target: target("session-a"),
      upload_id: unused_upload,
    })
    .unwrap();
  assert!(!unused_path.exists());
  let (upload_id, image_id) = upload(&store);
  let resolved = store
    .resolve(&target("session-a"), std::slice::from_ref(&image_id))
    .unwrap();
  let path = resolved[0].path.clone();
  store.retain(std::slice::from_ref(&image_id)).unwrap();
  store.retain(std::slice::from_ref(&image_id)).unwrap();
  store
    .release(ReleaseImageUploadRequest {
      target: target("session-a"),
      upload_id,
    })
    .unwrap();
  drop(store);
  assert_eq!(std::fs::read(&path).unwrap(), *resolved[0].bytes);
  assert!(retained_marker(&path).exists());
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
      std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
      0o700
    );
  }
}

#[test]
fn expired_and_mutated_uploads_cannot_be_sent() {
  let store = ImageStore::temporary().unwrap();
  let (upload_id, image_id) = upload(&store);
  let path = store.0.lock().unwrap().uploads[&upload_id].path.clone();
  std::fs::write(path, png(4, 4)).unwrap();
  assert!(
    store
      .resolve(&target("session-a"), std::slice::from_ref(&image_id))
      .is_err()
  );
  store.0.lock().unwrap().uploads.get_mut(&upload_id).unwrap().touched = Instant::now() - UPLOAD_TTL;
  assert!(store.resolve(&target("session-a"), &[image_id]).is_err());
}

#[test]
fn previews_validate_rasters_and_bound_thumbnail_dimensions() {
  let bytes = png(960, 720);
  let thumbnail = image_response(bytes.clone(), ImageVariant::Thumbnail).unwrap();
  assert_eq!((thumbnail.width, thumbnail.height), (480, 360));
  assert_eq!(thumbnail.mime_type, "image/png");
  let full = image_response(bytes.clone(), ImageVariant::Full).unwrap();
  assert_eq!(STANDARD.decode(full.data_base64).unwrap(), bytes);
  assert!(image_response(b"not an image".to_vec(), ImageVariant::Full).is_err());
  let mut image = MessageImage::from_url("0".into(), "https://example.com/private.png".into(), None, None);
  assert!(read_source(&image).is_err());
  image.source = MessageImageSource::LocalFile {
    path: "relative.png".into(),
  };
  assert!(read_source(&image).is_err());
  image.source = MessageImageSource::DataUrl {
    url: format!("data:image/jpeg;base64,{}", STANDARD.encode(png(2, 2))),
  };
  assert!(read_source(&image).is_err());
}

#[test]
fn timeline_details_omit_binary_sources_without_redacting_unrelated_data() {
  let mut value = json!({
    "content":[{"type":"image", "data":"secret-bytes", "mimeType":"image/png"},
      {"type":"input_image", "image_url":"data:image/png;base64,secret"}],
    "images":["/private/image.png", {"source":{"type":"base64", "data":"secret-bytes"}}],
    "generation":{"type":"ImageGeneration", "result":"secret-pixels"},
    "tool":{"type":"tool_call", "tool_name":"image_generation", "output":"secret-pixels"},
    "input":{"data":"ordinary tool input"}
  });
  sanitize_display_value(&mut value);
  let serialized = value.to_string();
  assert!(!serialized.contains("secret"));
  assert!(!serialized.contains("/private/image.png"));
  assert!(serialized.contains("ordinary tool input"));
}
