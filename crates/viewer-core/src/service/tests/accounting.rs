use super::*;
use tokn_session_codex::normalize::CodexNormalizer;

#[test]
fn codex_accounting_and_completed_bookkeeping_stay_out_of_the_reading_view() {
  let counts = |total| {
    json!({
      "input_tokens": total - 5,
      "cached_input_tokens": 0,
      "output_tokens": 5,
      "reasoning_output_tokens": 0,
      "total_tokens": total,
    })
  };
  let mut normalizer = CodexNormalizer::new();
  let mut events = [
    json!({"type":"session_meta","payload":{"id":"fixture","history_mode":"paginated"}}),
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"first"}}),
    json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"fixture","turn_id":"first","item":{
      "type":"UserMessage","id":"prompt","content":[{"type":"text","text":"Review the checklist"}],
    }}}),
    json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"fixture","turn_id":"first","item":{
      "type":"AgentMessage","id":"reply","phase":"final_answer",
      "content":[{"type":"Text","text":"The checklist is ready."}],
    }}}),
    json!({"type":"token_usage_record","payload":{
      "turn_id":"first","response_id":"response-1","usage":counts(35),"thread_token_usage":counts(60),
    }}),
    json!({"type":"event_msg","payload":{"type":"token_count","info":{
      "total_token_usage":counts(60),"last_token_usage":counts(35),
    }}}),
    json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"first"}}),
    json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"second"}}),
    json!({"type":"event_msg","payload":{"type":"token_count","info":{
      "total_token_usage":counts(70),"last_token_usage":counts(10),
    }}}),
    json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"second"}}),
  ]
  .into_iter()
  .flat_map(|mut record| {
    record["timestamp"] = json!("2026-09-01T12:00:00.100Z");
    normalizer.normalize(serde_json::from_value(record).unwrap())
  })
  .collect::<Vec<_>>();

  let directory = tempfile::tempdir().unwrap();
  let session_key = key_for_cached_source(&directory, "fixture");
  let service = service_with_session(loaded_session(events.clone()));
  let page = service
    .load_event_page(EventPageRequest {
      window_mode: None,
      session_key: session_key.clone(),
      cursor: None,
      offset: None,
      direction: PageDirection::Forward,
      limit: None,
    })
    .unwrap();
  let visible = page
    .events
    .iter()
    .filter(|row| !row.is_bookkeeping)
    .map(|row| (row.event_type.as_str(), row.summary.as_str()))
    .collect::<Vec<_>>();
  assert_eq!(
    visible,
    [
      ("message", "Review the checklist"),
      ("message", "The checklist is ready.")
    ]
  );
  assert!(page.events.iter().all(|row| row.trajectory.is_none()));

  let indices = events.iter().enumerate().filter_map(|(index, event)| match event {
    AgentEvent::Usage(usage) => Some((index, usage)),
    _ => None,
  });
  let mut counters = Vec::new();
  for (index, usage) in indices {
    let key = encode_event_key(index);
    let row = page.events.iter().find(|row| row.event_key == key).unwrap();
    assert!(row.is_bookkeeping);
    let summary = row.usage.as_ref().unwrap();
    counters.push((summary.kind.as_str(), summary.total_tokens.as_deref()));
    let detail = service
      .load_event_detail(LoadEventDetailRequest {
        session_key: session_key.clone(),
        event_key: key.clone(),
      })
      .unwrap();
    assert_eq!(detail.event_key, key);
    assert_eq!(detail.event["kind"].as_str(), Some(summary.kind.as_str()));
    assert_eq!(detail.event["total_tokens"].as_u64(), usage.total_tokens);
    assert_eq!(detail.native.as_ref(), Some(&usage.native));
  }
  assert_eq!(
    counters,
    [
      ("model_call", Some("35")),
      ("session_snapshot", Some("60")),
      ("session_snapshot", Some("70"))
    ]
  );

  // The following prompt proves the completed bookkeeping remains flat both at
  // the end of the loaded history and when the user continues.
  events.push(message_event_with_role(
    "Next prompt",
    Role::User,
    MessageDelivery::Unspecified,
  ));
  assert!(
    timeline_entries(&events)
      .iter()
      .all(|entry| matches!(entry, TimelineEntry::Event { .. }))
  );
}
