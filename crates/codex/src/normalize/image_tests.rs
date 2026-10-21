use super::*;
use tokn_session_core::RecordedImageSource;

fn normalize(rows: Vec<Value>, canonical: bool, historical: bool) -> Vec<AgentEvent> {
  let mut normalizer = if historical {
    CodexNormalizer::new_historical()
  } else {
    CodexNormalizer::new()
  };
  let mut events = normalizer.normalize(
    serde_json::from_value(json!({"type":"session_meta", "payload":{
      "id":"images", "history_mode":if canonical { "paginated" } else { "legacy" }
    }}))
    .unwrap(),
  );
  for row in rows {
    events.extend(normalizer.normalize(serde_json::from_value(row).unwrap()));
  }
  events
}

fn tools(events: &[AgentEvent]) -> Vec<&ToolCallEvent> {
  events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::ToolCall(tool) => Some(tool),
      _ => None,
    })
    .collect()
}

fn raw_generation(result: &str) -> Value {
  json!({"type":"response_item", "payload":{"type":"image_generation_call", "id":"image-call", "status":"completed", "result":result}})
}

fn legacy_generation(result: &str) -> Value {
  json!({"type":"event_msg", "payload":{"type":"image_generation_end", "call_id":"image-call", "status":"completed", "result":result, "saved_path":"/tmp/generated.png"}})
}

fn canonical_item(item: Value, started: bool) -> Value {
  json!({"type":"event_msg", "payload":{"type":if started { "item_started" } else { "item_completed" }, "thread_id":"images", "turn_id":"turn", "item":item}})
}

#[test]
fn function_outputs_and_mcp_images_remain_tool_results_in_native_order() {
  for historical in [false, true] {
    let output = json!([
      {"type":"input_text", "text":"first"},
      {"type":"input_image", "image_url":"data:image/png;base64,YQ=="},
      {"type":"input_image", "file_id":"opaque"},
      {"type":"future_image", "opaque":"untouched"}
    ]);
    let mcp = json!({"Ok":{"isError":true, "content":[
      {"type":"image", "mimeType":"image/png", "data":"Yg=="},
      {"type":"text", "text":"caption"}, {"type":"image", "mimeType":"image/png"}
    ]}});
    let events = normalize(
      vec![
        json!({"type":"response_item", "payload":{"type":"function_call_output", "call_id":"view", "output":output}}),
        json!({"type":"event_msg", "payload":{"type":"mcp_tool_call_end", "call_id":"mcp", "invocation":{"server":"tools", "tool":"images"}, "result":mcp}}),
      ],
      false,
      historical,
    );
    assert!(!events.iter().any(|event| matches!(event, AgentEvent::Message(_))));
    let tools = tools(&events);
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].tool_call_id.as_deref(), Some("view"));
    assert_eq!(
      tools[0]
        .images
        .iter()
        .map(|image| image.image_id.as_str())
        .collect::<Vec<_>>(),
      ["image.v1.1", "image.v1.2"]
    );
    assert!(matches!(
      &tools[0].images[0].source,
      RecordedImageSource::DataUrl { .. }
    ));
    assert!(
      matches!(&tools[0].images[1].source, RecordedImageSource::Unavailable { reference: Some(reference) } if reference == "opaque")
    );
    assert_eq!(tools[0].output.as_ref(), Some(&output));
    assert_eq!(tools[1].is_error, Some(true));
    assert_eq!(tools[1].images.len(), 2);
    assert!(matches!(&tools[1].images[0].source, RecordedImageSource::Base64 { data } if data == "Yg=="));
    assert!(matches!(
      &tools[1].images[1].source,
      RecordedImageSource::Unavailable { .. }
    ));
    assert_eq!(tools[1].output.as_ref(), Some(&mcp));
  }
}

#[test]
fn structured_custom_outputs_and_terminal_image_views_expose_recorded_resources() {
  let events = normalize(
    vec![
      json!({"type":"response_item", "payload":{"type":"custom_tool_call_output", "call_id":"custom", "output":[{"type":"input_image", "image_url":"data:image/png;base64,YQ=="}]}}),
      json!({"type":"event_msg", "payload":{"type":"view_image_tool_call", "call_id":"view", "path":"/tmp/viewed.png"}}),
      json!({"type":"event_msg", "payload":{"type":"view_image_tool_call", "call_id":"relative", "path":"relative.png"}}),
    ],
    false,
    false,
  );
  let legacy_tools = tools(&events);
  assert_eq!(legacy_tools.len(), 3);
  assert!(matches!(
    &legacy_tools[0].images[0].source,
    RecordedImageSource::DataUrl { .. }
  ));
  assert_eq!(legacy_tools[0].tool_call_id.as_deref(), Some("custom"));
  assert!(legacy_tools[0].native.is_some());
  assert!(
    matches!(&legacy_tools[1].images[0].source, RecordedImageSource::LocalFile { path } if path.to_str() == Some("/tmp/viewed.png"))
  );
  assert!(matches!(
    &legacy_tools[2].images[0].source,
    RecordedImageSource::Unavailable { .. }
  ));
  let viewed = json!({"type":"ImageView", "id":"view", "path":"file:///tmp/viewed%20image.png"});
  let events = normalize(
    vec![
      canonical_item(viewed.clone(), true),
      canonical_item(viewed, false),
      json!({"type":"event_msg", "payload":{"type":"view_image_tool_call", "call_id":"view", "path":"/tmp/viewed image.png"}}),
    ],
    true,
    true,
  );
  let tools = tools(&events);
  assert_eq!(tools.len(), 2);
  assert!(tools[0].images.is_empty());
  assert!(
    matches!(&tools[1].images[0].source, RecordedImageSource::LocalFile { path } if path.to_str() == Some("/tmp/viewed image.png"))
  );
}

#[test]
fn legacy_native_generation_items_do_not_duplicate_their_raw_and_end_fanout() {
  for native in [
    json!({"type":"ImageGeneration", "id":"image-call", "status":"completed", "result":"YQ==", "saved_path":"/tmp/generated.png"}),
    json!({"type":"Extension", "kind":"image_gen.generation", "id":"image-call", "status":"completed", "result":"YQ==", "savedPath":"/tmp/generated.png"}),
  ] {
    for rows in [
      vec![canonical_item(native.clone(), false), legacy_generation("YQ==")],
      vec![
        raw_generation("YQ=="),
        canonical_item(native, false),
        legacy_generation("YQ=="),
      ],
    ] {
      let events = normalize(rows, false, true);
      assert_eq!(tools(&events).len(), 1);
      assert_eq!(tools(&events)[0].images.len(), 1);
    }
  }
}

#[test]
fn canonical_mcp_and_generation_images_keep_tool_identity_and_only_finished_attachments() {
  let mcp = json!({"type":"McpToolCall", "id":"mcp", "server":"tools", "tool":"images", "arguments":{}, "status":"completed", "result":{"content":[{"type":"image", "mimeType":"image/png", "data":"YQ=="}]}});
  let hosted = json!({"type":"ImageGeneration", "id":"hosted", "status":"completed", "result":"Yg==", "saved_path":"/tmp/saved.png"});
  let extension = json!({"type":"Extension", "kind":"image_gen.generation", "id":"extension", "status":"completed", "result":"", "savedPath":"/tmp/extension.png"});
  let events = normalize(
    vec![
      canonical_item(hosted.clone(), true),
      canonical_item(hosted, false),
      canonical_item(extension, false),
      canonical_item(mcp, false),
      json!({"type":"event_msg", "payload":{"type":"mcp_tool_call_begin", "call_id":"mcp", "invocation":{"server":"tools", "tool":"images", "arguments":{}}}}),
      json!({"type":"event_msg", "payload":{"type":"mcp_tool_call_end", "call_id":"mcp", "invocation":{"server":"tools", "tool":"images"}, "result":{"Ok":{"content":[{"type":"image", "mimeType":"image/png", "data":"YQ=="}]}}}}),
      legacy_generation("Yg=="),
      raw_generation("Yg=="),
    ],
    true,
    true,
  );
  let tools = tools(&events);
  assert_eq!(tools.len(), 4);
  assert!(tools[0].images.is_empty());
  assert_eq!(tools[1].tool_call_id.as_deref(), Some("hosted"));
  assert!(matches!(&tools[1].images[0].source, RecordedImageSource::Base64 { data } if data == "Yg=="));
  assert_eq!(tools[2].tool_call_id.as_deref(), Some("extension"));
  assert!(
    matches!(&tools[2].images[0].source, RecordedImageSource::LocalFile { path } if path.to_str() == Some("/tmp/extension.png"))
  );
  assert_eq!(tools[3].tool_call_id.as_deref(), Some("mcp"));
  assert_eq!(tools[3].images[0].mime_type.as_deref(), Some("image/png"));
  assert!(!events.iter().any(|event| matches!(event, AgentEvent::Message(_))));
}

#[test]
fn raw_and_legacy_generation_fanout_is_deduplicated_without_correlating_reused_terminal_ids() {
  for rows in [
    vec![raw_generation("YQ=="), legacy_generation("YQ==")],
    vec![legacy_generation("YQ=="), raw_generation("YQ==")],
  ] {
    let events = normalize(rows, false, true);
    assert_eq!(tools(&events).len(), 1);
    assert_eq!(tools(&events)[0].images.len(), 1);
  }
  for rows in [
    vec![raw_generation("YQ=="), raw_generation("YQ==")],
    vec![legacy_generation("YQ=="), legacy_generation("YQ==")],
    vec![raw_generation("YQ=="), legacy_generation("Yg==")],
    vec![
      raw_generation("YQ=="),
      json!({"type":"event_msg", "payload":{"type":"user_message", "message":"new turn"}}),
      legacy_generation("YQ=="),
    ],
  ] {
    assert_eq!(tools(&normalize(rows, false, false)).len(), 2);
  }
}

#[test]
fn malformed_images_remain_discoverable_and_unknown_result_references_are_unavailable() {
  let events = normalize(
    vec![
      raw_generation("https://example.com/output.png"),
      json!({"type":"response_item", "payload":{"type":"image_generation_call", "id":"saved", "status":"completed", "saved_path":"/tmp/saved.png"}}),
      json!({"type":"event_msg", "payload":{"type":"image_generation_end", "call_id":"malformed", "status":"completed", "result":{"future":true}}}),
      json!({"type":"response_item", "payload":{"type":"function_call_output", "call_id":"plain", "output":"[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,YQ==\"}]"}}),
    ],
    false,
    true,
  );
  let tools = tools(&events);
  assert!(matches!(
    &tools[0].images[0].source,
    RecordedImageSource::Unavailable { .. }
  ));
  assert!(matches!(
    &tools[1].images[0].source,
    RecordedImageSource::LocalFile { .. }
  ));
  assert!(
    tools[2].images.is_empty(),
    "plain tool strings are not interpreted as structured image outputs"
  );
  assert!(events.iter().any(|event| matches!(event, AgentEvent::Unknown(event) if event.native_type.as_deref() == Some("event_msg.image_generation_end") && event.native.as_ref().unwrap()["result"]["future"] == true)));
}
