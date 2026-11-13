use super::*;
use crate::{
  images::ImageVariant,
  repository::ViewerRepository,
  service::tests::{loaded_session, message_event_with_role},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokn_session_core::{MessageImage, MessageImageSource, MessageProvenance, ToolRecordKind};

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
  fixture_for(ViewerProvider::Codex, loaded_session(events))
}

fn fixture_for(provider: ViewerProvider, session: LoadedSession) -> (ViewerService, String, String, Arc<AtomicUsize>) {
  let mut other = session.clone();
  other.reference.id = "unshared".into();
  other.reference.path = "/fixtures/unshared.jsonl".into();
  let session_key = encode_session_key(&locator_for_reference(&session, provider)).unwrap();
  let other_key = encode_session_key(&locator_for_reference(&other, provider)).unwrap();
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
    let source = index_source_key_for_path(provider, &reference.path).unwrap();
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
      SourceState::new(index_catalog_source_key(provider), "fixture-catalog", 0),
      Vec::new(),
    )])
    .unwrap();
  (service, session_key, other_key, loads)
}

fn locator_for_reference(session: &LoadedSession, provider: ViewerProvider) -> SessionLocator {
  SessionLocator {
    version: 1,
    provider,
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

#[test]
fn zcode_artifact_images_are_lazy_owner_scoped_resources_in_messages_and_tools() {
  let directory = tempfile::tempdir().unwrap();
  let cli = directory.path().join("storage/cli");
  std::fs::create_dir_all(cli.join("db")).unwrap();
  let database = cli.join("db/db.sqlite");
  std::fs::write(&database, []).unwrap();
  let source = MessageImageSource::Unavailable {
    reference: Some("zcode-artifact://sess_fixture/tool-result-image".into()),
  };
  let mut message = image_message(source.clone());
  if let AgentEvent::Message(message) = &mut message {
    message.provider = Provider::ZCode;
    message.session_id = Some("sess_fixture".into());
  }
  let mut tool = output_tool(ToolRecordKind::Result, recorded_images(&message).to_vec(), None);
  if let AgentEvent::ToolCall(tool) = &mut tool {
    tool.provider = Provider::ZCode;
    tool.session_id = Some("sess_fixture".into());
  }
  let operation = assemble_tool_operations(std::slice::from_ref(&tool)).pop().unwrap();
  let summary = operation_images(&operation).pop().unwrap();
  assert!(matches!(summary.availability, ImageAvailability::Available));
  assert_eq!(summary.source_event_key, Some(encode_event_key(0)));
  // No artifact directory exists: projections only expose a lazy capability.
  assert!(matches!(
    event_images(&message)[0].availability,
    ImageAvailability::Available
  ));
  let mut loaded = loaded_session(vec![message, tool]);
  loaded.reference.id = "sess_fixture".into();
  loaded.reference.path = database;
  let (service, key, other, _) = fixture_for(ViewerProvider::ZCode, loaded);
  let detail = service
    .load_event_detail(LoadEventDetailRequest {
      session_key: key.clone(),
      event_key: encode_event_key(0),
    })
    .unwrap();
  assert_eq!(detail.event["images"][0]["availability"], "available");
  assert!(service.load_session_image(read_request(&key)).is_err());
  let artifacts = cli.join("artifacts/sess_fixture");
  std::fs::create_dir_all(&artifacts).unwrap();
  let bytes = png();
  let encoded = STANDARD.encode(&bytes);
  std::fs::write(
    artifacts.join("prompt-attachment-upload-fixture-tool-result-image.txt"),
    format!("data:image/png;base64,{encoded}"),
  )
  .unwrap();
  let shared = service.scoped_to_sessions(std::slice::from_ref(&key)).unwrap();
  for index in [0, 1] {
    let mut request = read_request(&key);
    request.event_key = encode_event_key(index);
    let image = shared.load_session_image(request).unwrap();
    assert_eq!(STANDARD.decode(image.data_base64).unwrap(), bytes);
  }
  assert!(service.load_session_image(read_request(&other)).is_err());
  assert!(shared.load_session_image(read_request(&other)).is_err());
  assert_eq!(
    recorded_images(
      &service
        .load_verified(&decode_session_key(&key).unwrap())
        .unwrap()
        .events[0]
    )[0]
      .source,
    source
  );
  let details = serde_json::to_string(&detail).unwrap();
  assert!(!details.contains(&encoded));
  assert!(!details.contains("zcode-artifact://"));
}

#[test]
fn zcode_lazy_availability_requires_exact_provider_and_source_event_owner() {
  for (provider, owner, reference) in [
    (
      Provider::Codex,
      Some("fixture"),
      "zcode-artifact://fixture/tool-result-image",
    ),
    (
      Provider::ZCode,
      Some("fixture"),
      "zcode-artifact://other/tool-result-image",
    ),
    (Provider::ZCode, None, "zcode-artifact://fixture/tool-result-image"),
    (
      Provider::ZCode,
      Some("fixture"),
      "zcode-artifact://fixture/../tool-result-image",
    ),
  ] {
    let mut event = image_message(MessageImageSource::Unavailable {
      reference: Some(reference.into()),
    });
    if let AgentEvent::Message(message) = &mut event {
      message.provider = provider;
      message.session_id = owner.map(str::to_owned);
    }
    assert!(matches!(
      event_images(&event)[0].availability,
      ImageAvailability::Unavailable
    ));
    let mut tool = output_tool(ToolRecordKind::Result, recorded_images(&event).to_vec(), None);
    if let AgentEvent::ToolCall(tool) = &mut tool {
      tool.provider = provider;
      tool.session_id = owner.map(str::to_owned);
    }
    let operation = assemble_tool_operations(&[tool]).pop().unwrap();
    assert!(matches!(
      operation_images(&operation)[0].availability,
      ImageAvailability::Unavailable
    ));
    let (service, key, _, _) = fixture(vec![event]);
    assert!(service.load_session_image(read_request(&key)).is_err());
  }
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
  assert!(event_images(&hidden).is_empty());
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
    event_images(&unavailable)[0].availability,
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
  let summary = event_images(&event).pop().unwrap();
  assert!(summary.mime_type.is_none());
  let name = summary.name.unwrap();
  assert!(name.chars().count() <= 255);
  assert!(!name.chars().any(char::is_control));
  assert!(summary.image_id.len() < 256);
  assert_eq!(summary.image_id, display_image_id(&"i".repeat(4096)));
}

fn output_tool(kind: ToolRecordKind, images: Vec<RecordedImage>, output: Option<Value>) -> AgentEvent {
  AgentEvent::ToolCall(ToolCallEvent {
    images,
    provider: Provider::Codex,
    session_id: Some("fixture".into()),
    turn_id: None,
    message_id: None,
    parent_id: None,
    record_kind: kind,
    tool_call_id: Some("image-call".into()),
    provider_tool_name: Some("image_generation".into()),
    tool_name: Some("image_generation".into()),
    tool_kind: ToolKind::Unknown,
    transport: None,
    summary: None,
    phase: Phase::Finished,
    input: None,
    output,
    is_error: Some(false),
    native: None,
    timestamp: None,
  })
}

fn output_image(bytes: &[u8]) -> RecordedImage {
  RecordedImage {
    image_id: "image.v1.0".into(),
    mime_type: Some("image/png".into()),
    name: None,
    source: RecordedImageSource::Base64 {
      data: STANDARD.encode(bytes),
    },
  }
}

#[test]
fn assembled_tool_images_read_their_exact_source_and_keep_binary_out_of_projections() {
  let bytes = png();
  let encoded = STANDARD.encode(&bytes);
  let mut events = vec![
    output_tool(ToolRecordKind::Invocation, Vec::new(), None),
    output_tool(ToolRecordKind::Result, vec![output_image(&bytes)], Some(json!(encoded))),
  ];
  if let AgentEvent::ToolCall(tool) = &mut events[1] {
    tool.native = Some(json!({"type":"image_generation_end", "result":encoded,
      "saved_path":"/private/generated-output.png", "status":"completed"}));
  }
  let operations = assemble_tool_operations(&events);
  assert_eq!(operations.len(), 1);
  let operation = &operations[0];
  let summary = tool_operation_event_summary(0, operation);
  let image = &summary.images.as_ref().unwrap()[0];
  assert_eq!(image.source_event_key.as_deref(), Some(encode_event_key(1).as_str()));
  let revision = summary.content_revision.clone().unwrap();
  let detail = tool_operation_detail(encode_event_key(0), operation, &events).unwrap();
  assert_eq!(detail.content_revision.as_deref(), Some(revision.as_str()));
  let serialized = serde_json::to_string(&detail).unwrap();
  assert!(!serialized.contains(&encoded));
  assert!(!serialized.contains("/private/generated-output.png"));
  assert!(detail.event["images"][0].get("source").is_none());

  let (service, key, other, _) = fixture(events);
  let request = LoadSessionImageRequest {
    session_key: key.clone(),
    event_key: image.source_event_key.clone().unwrap(),
    image_id: image.image_id.clone(),
    variant: ImageVariant::Full,
  };
  assert_eq!(
    STANDARD
      .decode(service.load_session_image(request.clone()).unwrap().data_base64)
      .unwrap(),
    bytes
  );
  let mut wrong = request.clone();
  wrong.event_key = encode_event_key(0);
  assert!(
    service.load_session_image(wrong).is_err(),
    "invocation cannot impersonate its result record"
  );
  let shared = service.scoped_to_sessions(std::slice::from_ref(&key)).unwrap();
  assert!(shared.load_session_image(request.clone()).is_ok());
  let mut wrong = request;
  wrong.session_key = other;
  assert!(shared.load_session_image(wrong).is_err());
  let all = service
    .load_session_updates(
      serde_json::from_value(json!({
        "session_key":key, "subscription_id":"tool-images", "level":"all", "cursor":null
      }))
      .unwrap(),
    )
    .unwrap();
  let all = serde_json::to_string(&all).unwrap();
  assert!(!all.contains(&encoded));
  assert!(!all.contains("/private/generated-output.png"));
}

#[test]
fn newer_tool_output_does_not_replace_a_recorded_image_resource_or_resurrect_a_removed_one() {
  let first = png();
  let mut second = std::io::Cursor::new(Vec::new());
  image::DynamicImage::new_rgb8(3, 3)
    .write_to(&mut second, image::ImageFormat::Png)
    .unwrap();
  let second = second.into_inner();
  let events = vec![
    output_tool(ToolRecordKind::Invocation, Vec::new(), None),
    output_tool(
      ToolRecordKind::Progress,
      vec![output_image(&first)],
      Some(json!("first")),
    ),
    output_tool(
      ToolRecordKind::Result,
      vec![output_image(&second)],
      Some(json!("second")),
    ),
  ];
  let operations = assemble_tool_operations(&events);
  let summary = tool_operation_event_summary(0, &operations[0]);
  assert_eq!(
    summary.images.as_ref().unwrap()[0].source_event_key.as_deref(),
    Some(encode_event_key(2).as_str())
  );
  assert_ne!(event_image_revision(&events[1]), event_image_revision(&events[2]));
  let (service, key, _, _) = fixture(events.clone());
  let read = |index| {
    service
      .load_session_image(LoadSessionImageRequest {
        session_key: key.clone(),
        event_key: encode_event_key(index),
        image_id: "image.v1.0".into(),
        variant: ImageVariant::Full,
      })
      .unwrap()
  };
  assert_eq!(STANDARD.decode(read(1).data_base64).unwrap(), first);
  assert_eq!(STANDARD.decode(read(2).data_base64).unwrap(), second);
  let mut cleared = events;
  if let AgentEvent::ToolCall(tool) = &mut cleared[2] {
    tool.images.clear();
  }
  let operations = assemble_tool_operations(&cleared);
  let summary = tool_operation_event_summary(0, &operations[0]);
  assert!(
    summary.images.unwrap().is_empty(),
    "empty image coverage is authoritative"
  );
  assert!(summary.content_revision.is_none());
}

#[test]
fn generation_previews_omit_pixels_but_preserve_text_and_errors() {
  let image = output_image(&png());
  let RecordedImageSource::Base64 { data } = &image.source else {
    unreachable!()
  };
  assert!(display_tool_output(&json!(data), Some("image_generation"), [&image]).is_null());
  assert_eq!(
    display_tool_output(
      &json!({"status":"completed", "result":data, "saved_path":"/private/output.png"}),
      Some("image_generation"),
      [&image]
    ),
    json!({})
  );
  let result = display_tool_output(
    &json!({"status":"failed", "result":"Could not generate the image", "error":"Quota exhausted"}),
    Some("image_generation"),
    [&image],
  );
  assert_eq!(result["error"], "Quota exhausted");
  assert_eq!(result["result"], "Could not generate the image");
  assert_eq!(
    display_tool_output(&json!("Rendering failed"), Some("image_generation"), [&image]),
    "Rendering failed"
  );
}

#[test]
fn markdown_images_use_authoritative_message_ranges_and_the_existing_scoped_loader() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("updated-screen.png");
  let bytes = png();
  std::fs::write(&path, &bytes).unwrap();
  let source = format!("🖼 Updated\n\n![Updated Runtimes screen]({})\n\nDone.", path.display());
  let event = message_event_with_role(&source, Role::Assistant, MessageDelivery::Final);
  let events = vec![
    event.clone(),
    message_event_with_role("Other message", Role::Assistant, MessageDelivery::Final),
  ];
  let summary = event_summary(&events, 0, &event);
  let images = summary.images.as_ref().unwrap();
  assert_eq!(images.len(), 1);
  let image = &images[0];
  assert!(image.image_id.starts_with("markdown.v1."));
  assert_eq!(image.name.as_deref(), Some("Updated Runtimes screen"));
  let range = image.markdown_range.as_ref().unwrap();
  let utf16: Vec<_> = source.encode_utf16().collect();
  assert_eq!(
    String::from_utf16(&utf16[range.start..range.end]).unwrap(),
    format!("![Updated Runtimes screen]({})", path.display())
  );
  assert!(image.source_event_key.is_none());
  let revision = summary.content_revision.as_ref().unwrap();
  let (service, key, other_key, _) = fixture(events);
  let detail = service
    .load_event_detail(LoadEventDetailRequest {
      session_key: key.clone(),
      event_key: encode_event_key(0),
    })
    .unwrap();
  assert_eq!(detail.content_revision.as_ref(), Some(revision));
  assert_eq!(detail.event["images"][0]["image_id"], image.image_id);
  assert_eq!(detail.event["images"][0]["markdown_range"]["start"], range.start);
  assert!(detail.event["images"][0].get("source").is_none());
  let shared = service.scoped_to_sessions(std::slice::from_ref(&key)).unwrap();
  let request = LoadSessionImageRequest {
    session_key: key.clone(),
    event_key: encode_event_key(0),
    image_id: image.image_id.clone(),
    variant: ImageVariant::Full,
  };
  assert_eq!(
    STANDARD
      .decode(shared.load_session_image(request.clone()).unwrap().data_base64)
      .unwrap(),
    bytes
  );
  let mut outside = request.clone();
  outside.session_key = other_key;
  assert!(shared.load_session_image(outside).is_err());
  for (event_key, image_id) in [
    (encode_event_key(1), image.image_id.clone()),
    (encode_event_key(0), path.to_string_lossy().into_owned()),
    ("window.v1.6f6c64.event.v1.0".into(), image.image_id.clone()),
  ] {
    assert!(
      shared
        .load_session_image(LoadSessionImageRequest {
          event_key,
          image_id,
          ..request.clone()
        })
        .is_err()
    );
  }
}

#[test]
fn markdown_image_revisions_and_empty_coverage_follow_message_edits() {
  let first = message_event_with_role("![One](/tmp/one.png)", Role::Assistant, MessageDelivery::Final);
  let second = message_event_with_role("![One](/tmp/two.png)", Role::Assistant, MessageDelivery::Final);
  let cleared = message_event_with_role("The image was removed.", Role::Assistant, MessageDelivery::Final);
  let summarize = |event: &AgentEvent| event_summary(std::slice::from_ref(event), 0, event);
  let original = summarize(&first);
  let replaced = summarize(&second);
  assert_ne!(original.content_revision, replaced.content_revision);
  assert_ne!(
    original.images.as_ref().unwrap()[0].image_id,
    replaced.images.as_ref().unwrap()[0].image_id
  );
  let empty = summarize(&cleared);
  assert!(empty.images.unwrap().is_empty());
  assert!(empty.content_revision.is_none());
  let mut mixed = first.clone();
  if let AgentEvent::Message(message) = &mut mixed {
    message.images.push(output_image(&png()));
  }
  let mixed = summarize(&mixed);
  let images = mixed.images.unwrap();
  assert_eq!(images.len(), 2);
  assert!(images[0].markdown_range.is_none());
  assert!(images[1].markdown_range.is_some());
  let (service, key, _, _) = fixture(vec![cleared]);
  assert!(
    service
      .load_session_image(LoadSessionImageRequest {
        session_key: key,
        event_key: encode_event_key(0),
        image_id: original.images.unwrap()[0].image_id.clone(),
        variant: ImageVariant::Full
      })
      .is_err()
  );
}

#[test]
fn derived_markdown_resources_ignore_hidden_and_technical_messages() {
  let source = "![Image](/tmp/technical.png)";
  for role in [Role::Tool, Role::System, Role::Unknown] {
    let event = message_event_with_role(source, role, MessageDelivery::Unspecified);
    assert!(event_images(&event).is_empty());
    let forged_id = images::markdown::message_images(source)[0].0.image_id.clone();
    let (service, key, _, _) = fixture(vec![event]);
    assert!(
      service
        .load_session_image(LoadSessionImageRequest {
          session_key: key,
          event_key: encode_event_key(0),
          image_id: forged_id,
          variant: ImageVariant::Full
        })
        .is_err()
    );
  }
  let mut hidden = message_event_with_role(source, Role::Assistant, MessageDelivery::Final);
  if let AgentEvent::Message(message) = &mut hidden {
    message.provenance = Some(MessageProvenance {
      source: json!({"kind":"extension"}),
      display: Some(false),
      native: None,
      surface_op: None,
      source_event_seqs: None,
    });
  }
  assert!(summary_images(&hidden).is_none());
  assert!(event_images(&hidden).is_empty());
  let technical_image = image_message(MessageImageSource::Base64 {
    data: STANDARD.encode(png()),
  });
  let mut technical_image = technical_image;
  if let AgentEvent::Message(message) = &mut technical_image {
    message.role = Role::Tool;
  }
  assert_eq!(
    event_images(&technical_image).len(),
    1,
    "native images retain their existing role policy"
  );
}

#[test]
fn markdown_references_share_validation_limits_and_never_fetch_remote_or_relative_paths() {
  let directory = tempfile::tempdir().unwrap();
  let invalid = directory.path().join("not-an-image.png");
  std::fs::write(&invalid, "not an image").unwrap();
  for destination in [
    "https://example.com/image.png".into(),
    "relative/image.png".into(),
    invalid.to_string_lossy().into_owned(),
  ] {
    let event = message_event_with_role(
      &format!("![Image]({destination})"),
      Role::User,
      MessageDelivery::Unspecified,
    );
    let image = event_images(&event).pop().unwrap();
    if !destination.starts_with('/') {
      assert!(matches!(image.availability, ImageAvailability::Unavailable));
    }
    let (service, key, _, _) = fixture(vec![event]);
    assert!(
      service
        .load_session_image(LoadSessionImageRequest {
          session_key: key,
          event_key: encode_event_key(0),
          image_id: image.image_id,
          variant: ImageVariant::Full
        })
        .is_err()
    );
  }
  let embedded = message_event_with_role(
    "![Embedded](data:image/png;base64,YQ==)",
    Role::Assistant,
    MessageDelivery::Final,
  );
  assert!(
    event_images(&embedded).is_empty(),
    "embedded Markdown remains an inert placeholder; structured data URLs retain their existing support"
  );
}
