use serde_json::{Value, json};
use tokn_session_codex::normalize::CodexNormalizer;
use tokn_session_core::{AgentEvent, LifecycleOutcome, MetadataKind, Phase, ToolRecordKind};

fn line(normalizer: &mut CodexNormalizer, payload: Value) -> Vec<AgentEvent> {
  normalizer.normalize(serde_json::from_value(payload).unwrap())
}

fn normalizer(paginated: bool) -> CodexNormalizer {
  let mut normalizer = CodexNormalizer::new();
  line(
    &mut normalizer,
    json!({"type":"session_meta","payload":{"id":"session","history_mode":if paginated {"paginated"} else {"legacy"}}}),
  );
  normalizer
}

fn event(payload: Value) -> Value {
  json!({"type":"event_msg","timestamp":"2026-10-20T00:00:00Z","payload":payload})
}

#[test]
fn fixture_preserves_native_failures_without_turning_notices_or_cancellation_into_errors() {
  let mut normalizer = CodexNormalizer::new();
  let events: Vec<_> = include_str!("../fixtures/diagnostics.jsonl")
    .lines()
    .flat_map(|source| normalizer.normalize(serde_json::from_str(source).unwrap()))
    .collect();
  let errors: Vec<_> = events
    .iter()
    .filter_map(|event| {
      if let AgentEvent::Error(event) = event {
        Some(event)
      } else {
        None
      }
    })
    .collect();
  // These are two explicit native records, without a shared authoritative
  // error identity. Equal text must not suppress a separate recorded failure.
  assert_eq!(errors.len(), 2);
  assert!(
    errors
      .iter()
      .all(|error| error.message == "Synthetic provider quota exceeded")
  );
  let outcomes: Vec<_> = events
    .iter()
    .filter_map(|event| {
      if let AgentEvent::Lifecycle(event) = event {
        event.outcome
      } else {
        None
      }
    })
    .collect();
  assert!(matches!(
    &outcomes[..],
    [
      LifecycleOutcome::Failed,
      LifecycleOutcome::Interrupted,
      LifecycleOutcome::Completed
    ]
  ));
  assert!(!events.iter().any(|event| matches!(event, AgentEvent::Unknown(_))));
  let diagnostic = events
    .iter()
    .find_map(|event| match event {
      AgentEvent::Metadata(event) if event.native_type == "event_msg.error" => Some(event),
      _ => None,
    })
    .unwrap();
  assert_eq!(diagnostic.native["future_diagnostic"]["request_id"], "request-1");
}

#[test]
fn warnings_and_retry_notices_are_diagnostics_with_exact_payloads() {
  for paginated in [false, true] {
    for kind in ["warning", "guardian_warning", "stream_error"] {
      let payload = json!({"type":kind,"message":"Recoverable native notice","additional_details":"Underlying transport detail","future":{"code":503}});
      let events = line(&mut normalizer(paginated), event(payload.clone()));
      assert!(
        matches!(&events[..], [AgentEvent::Metadata(diagnostic)] if matches!(diagnostic.kind, MetadataKind::Diagnostic) && diagnostic.native == payload && diagnostic.timestamp.as_deref() == Some("2026-10-20T00:00:00Z"))
      );
    }
  }
}

#[test]
fn nested_terminal_errors_have_failed_lifecycle_and_visible_diagnostics_in_both_history_modes() {
  for paginated in [false, true] {
    for kind in ["task_complete", "turn_complete"] {
      let payload = json!({"type":kind,"turn_id":"turn","error":{"message":"Synthetic failure","codex_error_info":{"http_connection_failed":{"http_status_code":502}}},"duration_ms":1});
      let events = line(&mut normalizer(paginated), event(payload.clone()));
      assert!(
        matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(diagnostic), AgentEvent::Lifecycle(lifecycle)] if error.message == "Synthetic failure" && error.native.as_ref() == Some(&payload) && diagnostic.native == payload && matches!(lifecycle.outcome, Some(LifecycleOutcome::Failed)) && lifecycle.native == payload && lifecycle.turn_id == "turn")
      );
    }
  }
}

#[test]
fn native_error_payloads_preserve_record_identity_and_repeated_failure_messages() {
  let payloads = [
    json!({"type":"error", "id":"record-1", "parent_id":"parent-1", "turn_id":"turn-1", "message":"Repeated failure", "codex_error_info":{"http_connection_failed":{"http_status_code":502}}, "future":{"request_id":"request-1"}}),
    json!({"type":"error", "id":"record-2", "parent_id":"parent-2", "turn_id":"turn-2", "message":"Repeated failure", "codex_error_info":"usage_limit_exceeded", "future":{"request_id":"request-2"}}),
  ];
  for paginated in [false, true] {
    let mut normalizer = normalizer(paginated);
    let events: Vec<_> = payloads
      .iter()
      .flat_map(|payload| line(&mut normalizer, event(payload.clone())))
      .collect();
    assert_eq!(events.len(), 4, "native detail does not add diagnostic rows");
    for (batch, payload) in events.chunks_exact(2).zip(&payloads) {
      assert!(
        matches!(batch, [AgentEvent::Error(error), AgentEvent::Metadata(diagnostic)]
        if error.message == "Repeated failure" && error.session_id.as_deref() == Some("session")
          && error.native.as_ref() == Some(payload) && diagnostic.native == *payload)
      );
    }
  }
}

#[test]
fn known_nonterminal_request_rejections_do_not_fail_the_active_turn() {
  // Newer upstream cf7e9cfe documents these auxiliary request failures. They
  // must not end the unrelated active turn; an actual completion still does.
  for paginated in [false, true] {
    for info in [
      json!("thread_rollback_failed"),
      json!({"active_turn_not_steerable":{"turn_kind":"review"}}),
      json!({"active_turn_not_steerable":{"turn_kind":"compact"}}),
    ] {
      let mut normalizer = normalizer(paginated);
      let started = line(
        &mut normalizer,
        event(json!({"type":"task_started","turn_id":"active"})),
      );
      assert!(matches!(&started[..], [AgentEvent::Lifecycle(lifecycle)] if matches!(lifecycle.phase, Phase::Started)));
      let payload = json!({"type":"error","message":"Request rejected","codex_error_info":info,"request_id":"auxiliary","future":{"retained":true}});
      let events = line(&mut normalizer, event(payload.clone()));
      assert!(
        matches!(&events[..], [AgentEvent::Metadata(diagnostic)] if matches!(diagnostic.kind, MetadataKind::Diagnostic) && diagnostic.summary.contains("Request rejected") && diagnostic.native == payload)
      );
      let completion = json!({"type":"task_complete","turn_id":"active","error":{"message":"Request rejected","codex_error_info":info}});
      let events = line(&mut normalizer, event(completion.clone()));
      assert!(
        matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(diagnostic), AgentEvent::Lifecycle(lifecycle)] if error.message == "Request rejected" && diagnostic.native == completion && matches!(lifecycle.outcome, Some(LifecycleOutcome::Failed)))
      );
    }
  }
}

#[test]
fn unfamiliar_or_malformed_request_error_codes_are_not_inferred_nonterminal() {
  for info in [
    json!("other"),
    json!(" thread_rollback_failed "),
    json!("active_turn_not_steerable"),
    json!({"thread_rollback_failed":{}}),
    json!({"active_turn_not_steerable":false}),
    json!({"active_turn_not_steerable":{"turn_kind":"future"}}),
    json!({"active_turn_not_steerable":{"turn_kind":"review"},"future":true}),
  ] {
    let payload = json!({"type":"error","message":"Native failure","codex_error_info":info});
    let events = line(&mut normalizer(false), event(payload.clone()));
    assert!(
      matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(diagnostic)] if error.message == "Native failure" && diagnostic.native == payload)
    );
  }
}

#[test]
fn missing_or_malformed_diagnostics_remain_inspectable_without_blank_errors_or_false_success() {
  for kind in ["error", "warning", "guardian_warning", "stream_error"] {
    for message in [
      Value::Null,
      json!(""),
      json!("  "),
      json!(false),
      json!({"nested":"future shape"}),
    ] {
      let payload = json!({"type":kind,"message":message,"future":"retained"});
      let events = line(&mut normalizer(false), event(payload.clone()));
      assert!(matches!(&events[..], [AgentEvent::Unknown(unknown)] if unknown.native.as_ref() == Some(&payload)));
    }
  }
  for error in [json!({"message":false}), json!({}), json!("future error")] {
    let payload = json!({"type":"task_complete","turn_id":"turn","error":error});
    let events = line(&mut normalizer(false), event(payload.clone()));
    assert!(
      matches!(&events[..], [AgentEvent::Unknown(unknown), AgentEvent::Lifecycle(lifecycle)] if unknown.native.as_ref() == Some(&payload) && lifecycle.outcome.is_none())
    );
  }
  let events = line(
    &mut normalizer(false),
    event(json!({"type":"task_complete","error":{"message":"Known failure without turn identity"}})),
  );
  assert!(matches!(
    &events[..],
    [AgentEvent::Error(_), AgentEvent::Metadata(_), AgentEvent::Unknown(_)]
  ));
}

#[test]
fn normal_abort_reasons_are_interrupted_boundaries_without_provider_errors() {
  for reason in ["interrupted", "replaced", "review_ended", "budget_limited"] {
    let payload = json!({"type":"turn_aborted","turn_id":"turn","reason":reason});
    let events = line(&mut normalizer(false), event(payload.clone()));
    assert!(
      matches!(&events[..], [AgentEvent::Metadata(diagnostic), AgentEvent::Lifecycle(lifecycle)] if diagnostic.native == payload && matches!(lifecycle.outcome, Some(LifecycleOutcome::Interrupted)))
    );
  }
  let payload = json!({"type":"turn_aborted","reason":"interrupted"});
  let events = line(&mut normalizer(false), event(payload.clone()));
  assert!(matches!(&events[..], [AgentEvent::Metadata(diagnostic)] if diagnostic.native == payload));
}

#[test]
fn optional_abort_identity_does_not_invent_a_lifecycle_and_malformed_aborts_stay_unknown() {
  for paginated in [false, true] {
    for payload in [
      json!({"type":"turn_aborted","reason":"replaced"}),
      json!({"type":"turn_aborted","turn_id":null,"reason":"replaced"}),
    ] {
      let events = line(&mut normalizer(paginated), event(payload.clone()));
      assert!(matches!(&events[..], [AgentEvent::Metadata(diagnostic)] if diagnostic.native == payload));
    }
    for id in [json!(""), json!("  "), json!(false), json!(42), json!({})] {
      let payload = json!({"type":"turn_aborted","turn_id":id,"reason":"interrupted"});
      let events = line(&mut normalizer(paginated), event(payload.clone()));
      assert!(matches!(&events[..], [AgentEvent::Unknown(unknown)] if unknown.native.as_ref() == Some(&payload)));
    }
    for reason in [Value::Null, json!(false), json!(""), json!("future")] {
      let payload = json!({"type":"turn_aborted","turn_id":"turn","reason":reason});
      let events = line(&mut normalizer(paginated), event(payload.clone()));
      assert!(matches!(&events[..], [AgentEvent::Unknown(unknown)] if unknown.native.as_ref() == Some(&payload)));
    }
    let events = line(
      &mut normalizer(paginated),
      event(json!({"type":"turn_aborted","turnId":"alias","reason":"interrupted"})),
    );
    assert!(
      matches!(&events[..], [AgentEvent::Metadata(_), AgentEvent::Lifecycle(lifecycle)] if lifecycle.turn_id == "alias" && matches!(lifecycle.outcome, Some(LifecycleOutcome::Interrupted)))
    );
  }
}

#[test]
fn explicit_image_and_tool_search_statuses_drive_tool_outcomes_without_global_errors() {
  for kind in ["image_generation_call", "tool_search_output"] {
    for (status, phase, is_error) in [
      ("failed", Phase::Finished, Some(true)),
      ("in_progress", Phase::Started, None),
      ("completed", Phase::Finished, None),
    ] {
      let payload = json!({"type":kind,"id":"item","call_id":"call","status":status,"tools":[],"execution":"client","error":{"message":"Useful tool diagnostic"}});
      let events = line(
        &mut normalizer(false),
        json!({"type":"response_item","payload":payload}),
      );
      let [AgentEvent::ToolCall(tool)] = &events[..] else {
        panic!("expected tool result, got {events:?}");
      };
      assert_eq!(tool.phase, phase, "{kind}/{status}");
      assert_eq!(tool.is_error, is_error, "{kind}/{status}");
      if status == "failed" {
        assert_eq!(
          tool.native.as_ref().unwrap()["error"]["message"],
          "Useful tool diagnostic"
        );
      }
      if status == "in_progress" {
        assert!(matches!(
          tool.record_kind,
          ToolRecordKind::Invocation | ToolRecordKind::Snapshot
        ));
      }
    }
  }
}

#[test]
fn arbitrary_tool_output_prose_does_not_manufacture_a_failure() {
  let events = line(
    &mut normalizer(false),
    json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call","output":"An example containing Error: is ordinary tool output"}}),
  );
  assert!(matches!(&events[..], [AgentEvent::ToolCall(tool)] if tool.is_error.is_none()));
}
