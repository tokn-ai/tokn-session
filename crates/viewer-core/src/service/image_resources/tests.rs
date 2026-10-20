use super::*;
use crate::{
  images::ImageVariant,
  repository::ViewerRepository,
  service::tests::{loaded_session, message_event_with_role},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokn_session_core::MessageProvenance;

struct ImageRepository {
  sessions: HashMap<String, LoadedSession>,
  loads: Arc<AtomicUsize>,
}

impl ViewerRepository for ImageRepository {
  fn list_session_headers(&self, _: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
    Err("Image resource reads must use the indexed catalog".into())
  }

  fn load_session(&self, locator: &SessionLocator) -> Result<LoadedSession, String> {
    self.loads.fetch_add(1, Ordering::SeqCst);
    self
      .sessions
      .get(&locator.session_id)
      .cloned()
      .ok_or("Unknown fixture session".into())
  }
}

fn png() -> Vec<u8> {
  let mut bytes = std::io::Cursor::new(Vec::new());
  image::DynamicImage::new_rgb8(2, 1)
    .write_to(&mut bytes, image::ImageFormat::Png)
    .unwrap();
  bytes.into_inner()
}

fn image_message(source: MessageImageSource) -> AgentEvent {
  let mut event = message_event_with_role("", Role::User, MessageDelivery::Unspecified);
  if let AgentEvent::Message(message) = &mut event {
    message.images.push(MessageImage {
      image_id: "image.v1.0".into(),
      mime_type: Some("image/png".into()),
      name: Some("picture.png".into()),
      source,
    });
  }
  event
}

fn fixture(events: Vec<AgentEvent>) -> (ViewerService, String, String, Arc<AtomicUsize>) {
  let session = loaded_session(events);
  let mut other = session.clone();
  other.reference.id = "unshared".into();
  other.reference.path = "/fixtures/unshared.jsonl".into();
  let session_key = encode_session_key(&locator_for_reference(&session)).unwrap();
  let other_key = encode_session_key(&locator_for_reference(&other)).unwrap();
  let loads = Arc::new(AtomicUsize::new(0));
  let service = ViewerService::new(Arc::new(ImageRepository {
    sessions: HashMap::from([
      (session.reference.id.clone(), session.clone()),
      (other.reference.id.clone(), other.clone()),
    ]),
    loads: loads.clone(),
  }));
  for loaded in [session, other] {
    let reference = loaded.reference;
    let source = index_source_key_for_path(ViewerProvider::Codex, &reference.path).unwrap();
    let header = SessionHeader {
      id: reference.id,
      path: reference.path,
      parent_session_id: None,
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      title: None,
      preview: None,
      cwd: None,
      timestamp: None,
      updated_at: None,
      updated_at_ms: None,
    };
    let metadata = session_metadata_from_header(&source, header, None, false).unwrap();
    service
      .session_index
      .replace_sources(&[SourceReplacement::new(
        SourceState::new(source, "fixture", 0),
        vec![metadata],
      )])
      .unwrap();
  }
  service
    .session_index
    .replace_sources(&[SourceReplacement::new(
      SourceState::new(index_catalog_source_key(ViewerProvider::Codex), "fixture-catalog", 0),
      Vec::new(),
    )])
    .unwrap();
  (service, session_key, other_key, loads)
}

fn locator_for_reference(session: &LoadedSession) -> SessionLocator {
  SessionLocator {
    version: 1,
    provider: ViewerProvider::Codex,
    session_id: session.reference.id.clone(),
    source_path: session.reference.path.clone(),
  }
}

fn read_request(key: &str) -> LoadSessionImageRequest {
  LoadSessionImageRequest {
    session_key: key.into(),
    event_key: encode_event_key(0),
    image_id: "image.v1.0".into(),
    variant: ImageVariant::Full,
  }
}

#[test]
fn recorded_image_reads_are_bound_to_catalog_event_and_attachment_identity() {
  let bytes = png();
  let (service, key, _, loads) = fixture(vec![
    image_message(MessageImageSource::Base64 {
      data: STANDARD.encode(&bytes),
    }),
    message_event_with_role("plain text", Role::Assistant, MessageDelivery::Final),
  ]);
  let image = service.load_session_image(read_request(&key)).unwrap();
  assert_eq!((image.width, image.height), (2, 1));
  assert_eq!(STANDARD.decode(image.data_base64).unwrap(), bytes);
  assert_eq!(image.mime_type, "image/png");
  let mut thumbnail = read_request(&key);
  thumbnail.variant = ImageVariant::Thumbnail;
  let thumbnail = service.load_session_image(thumbnail).unwrap();
  assert_eq!((thumbnail.width, thumbnail.height), (2, 1));

  for (event_key, image_id) in [
    (encode_event_key(1), "image.v1.0"),
    (encode_event_key(99), "image.v1.0"),
    (encode_event_key(0), "/tmp/arbitrary-image.png"),
    ("window.v1.6f6c64.event.v1.0".into(), "image.v1.0"),
    ("trajectory.v1.0".into(), "image.v1.0"),
  ] {
    let mut request = read_request(&key);
    request.event_key = event_key;
    request.image_id = image_id.into();
    assert!(service.load_session_image(request).is_err());
  }
  let before = loads.load(Ordering::SeqCst);
  let mut outside = read_request(&key);
  outside.session_key = encode_session_key(&SessionLocator {
    version: 1,
    provider: ViewerProvider::Codex,
    session_id: "missing".into(),
    source_path: "/fixtures/missing.jsonl".into(),
  })
  .unwrap();
  assert!(service.load_session_image(outside).is_err());
  assert_eq!(loads.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn session_shares_can_read_only_their_exact_images_and_cannot_mutate_uploads() {
  let (service, key, other, loads) = fixture(vec![image_message(MessageImageSource::Base64 {
    data: STANDARD.encode(png()),
  })]);
  let shared = service.scoped_to_sessions(std::slice::from_ref(&key)).unwrap();
  assert!(shared.load_session_image(read_request(&key)).is_ok());
  let before = loads.load(Ordering::SeqCst);
  assert!(
    shared
      .load_session_image(read_request(&other))
      .unwrap_err()
      .contains("share")
  );
  assert_eq!(loads.load(Ordering::SeqCst), before);
  let target = ImageTarget::Session { session_key: key };
  let error = shared
    .upload_image_chunk(UploadImageChunkRequest {
      target: target.clone(),
      upload_id: "upload".into(),
      offset: 0,
      total_bytes: 1,
      mime_type: "image/png".into(),
      name: None,
      data_base64: "YQ==".into(),
    })
    .await
    .unwrap_err();
  assert!(error.contains("shares"));
  assert!(
    shared
      .release_image_upload(ReleaseImageUploadRequest {
        target,
        upload_id: "upload".into()
      })
      .unwrap_err()
      .contains("shares")
  );
}

#[test]
fn hidden_unavailable_and_ambiguous_recorded_images_never_return_bytes() {
  let mut hidden = image_message(MessageImageSource::Base64 {
    data: STANDARD.encode(png()),
  });
  if let AgentEvent::Message(message) = &mut hidden {
    message.provenance = Some(MessageProvenance {
      source: json!({"kind":"extension"}),
      display: Some(false),
      native: None,
      surface_op: None,
      source_event_seqs: None,
    });
  }
  assert!(message_images(&hidden).is_empty());
  let (service, key, _, _) = fixture(vec![hidden]);
  assert!(
    service
      .load_session_image(read_request(&key))
      .unwrap_err()
      .contains("unavailable")
  );
  let unavailable = image_message(MessageImageSource::Unavailable {
    reference: Some("https://example.com/image.png".into()),
  });
  assert!(matches!(
    message_images(&unavailable)[0].availability,
    ImageAvailability::Unavailable
  ));
  let (service, key, _, _) = fixture(vec![unavailable]);
  assert!(service.load_session_image(read_request(&key)).is_err());
  let mut duplicate = image_message(MessageImageSource::Base64 {
    data: STANDARD.encode(png()),
  });
  if let AgentEvent::Message(message) = &mut duplicate {
    message.images.push(message.images[0].clone());
  }
  let (service, key, _, _) = fixture(vec![duplicate]);
  assert!(
    service
      .load_session_image(read_request(&key))
      .unwrap_err()
      .contains("ambiguous")
  );
}

#[test]
fn details_and_all_delivery_contain_image_metadata_without_image_bytes() {
  let encoded = STANDARD.encode(png());
  let mut event = image_message(MessageImageSource::DataUrl {
    url: format!("data:image/png;base64,{encoded}"),
  });
  if let AgentEvent::Message(message) = &mut event {
    message.provenance = Some(MessageProvenance {
      source: json!({"kind":"extension"}),
      display: Some(true),
      native: Some(
        json!({"type":"custom_message", "content":[{"type":"image", "mimeType":"image/png", "data":encoded}]}),
      ),
      surface_op: None,
      source_event_seqs: None,
    });
  }
  let (service, key, _, _) = fixture(vec![event]);
  let detail = service
    .load_event_detail(LoadEventDetailRequest {
      session_key: key.clone(),
      event_key: encode_event_key(0),
    })
    .unwrap();
  assert_eq!(detail.event["images"][0]["image_id"], "image.v1.0");
  assert_eq!(detail.event["images"][0]["availability"], "available");
  assert!(detail.event["images"][0].get("source").is_none());
  let detail = serde_json::to_string(&detail).unwrap();
  assert!(!detail.contains(&encoded));
  assert!(!detail.contains("data:image/"));
  let update = service
    .load_session_updates(
      serde_json::from_value(json!({
        "session_key":key, "subscription_id":"images", "level":"all", "cursor":null
      }))
      .unwrap(),
    )
    .unwrap();
  let update = serde_json::to_string(&update).unwrap();
  assert!(update.contains("image.v1.0"));
  assert!(!update.contains(&encoded));
  assert!(!update.contains("data:image/"));
}

#[test]
fn display_image_metadata_is_bounded_independently_of_native_names() {
  let mut event = image_message(MessageImageSource::Unavailable { reference: None });
  if let AgentEvent::Message(message) = &mut event {
    message.images[0].name = Some(format!("bad\n{}", "x".repeat(1000)));
    message.images[0].mime_type = Some("image/not-real".into());
    message.images[0].image_id = "i".repeat(4096);
  }
  let summary = message_images(&event).pop().unwrap();
  assert!(summary.mime_type.is_none());
  let name = summary.name.unwrap();
  assert!(name.chars().count() <= 255);
  assert!(!name.chars().any(char::is_control));
  assert!(summary.image_id.len() < 256);
  assert_eq!(summary.image_id, display_image_id(&"i".repeat(4096)));
}
