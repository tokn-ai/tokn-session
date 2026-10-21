use serde_json::{Value, json};
use tokn_session_core::{AgentEvent, MetadataKind, Provider, Role};
use tokn_session_pi::{
  PiSessionSource,
  event::{PiMessage, PiSessionItem, PiSessionLine},
  normalize::PiNormalizer,
};

fn normalize(message: Value) -> (Value, Vec<AgentEvent>) {
  let native = json!({
    "type":"message", "id":"assistant-1", "parentId":"user-1",
    "timestamp":"2026-10-19T00:00:02.000Z", "message":message,
  });
  let events = normalize_record(native.clone());
  (native, events)
}

fn normalize_record(native: Value) -> Vec<AgentEvent> {
  let mut normalizer = PiNormalizer::new();
  normalizer.normalize(serde_json::from_value(json!({"type":"session", "id":"pi-errors"})).unwrap());
  normalizer.normalize(serde_json::from_value(native).unwrap())
}

fn assert_model_request(event: &AgentEvent, native: &Value) {
  let AgentEvent::ProviderChanged(model) = event else {
    panic!("expected recorded request model")
  };
  assert_eq!(model.model_origin, Some(tokn_session_core::ModelOrigin::Request));
  assert_eq!(model.model_provider.as_deref(), native["message"]["provider"].as_str());
  assert_eq!(model.model_id.as_deref(), native["message"]["model"].as_str());
  assert_eq!(model.native_id.as_deref(), native["id"].as_str());
  assert_eq!(model.native_parent_id.as_deref(), native["parentId"].as_str());
}

#[test]
fn connection_errors_remain_visible_in_history_and_incremental_normalization() {
  let fixture = include_str!("fixtures/assistant_errors.jsonl");
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/assistant_errors.jsonl");
  let loaded = PiSessionSource::new(None).load_session_path(&path).unwrap();
  let mut incremental = PiNormalizer::new();
  let mut live_events = Vec::new();
  for (index, line) in fixture.lines().enumerate() {
    let native: Value = serde_json::from_str(line).unwrap();
    let decoded: PiSessionLine = serde_json::from_str(line).unwrap();
    assert_eq!(decoded.native(), &native);
    if index >= 2 {
      // The fixture exercises the typed assistant contract, not a recovered
      // Unknown payload that happens to contain error-looking field names.
      let PiSessionItem::Message(item) = decoded.item() else {
        panic!("expected message")
      };
      let Some(PiMessage::Assistant(message)) = &item.message else {
        panic!("expected typed assistant")
      };
      assert!(message.content.is_empty());
      assert_eq!(message.stop_reason.as_deref(), Some("error"));
      assert_eq!(message.error_message.as_deref(), Some("Connection error."));
    }
    let events = incremental.normalize(decoded);
    if index >= 2 {
      let diagnostic_events = if index == 2 {
        assert_model_request(&events[0], &native);
        &events[1..]
      } else {
        // Repeated retries keep their own diagnostics without repeating the
        // same model observation before every failed response.
        &events[..]
      };
      let [
        AgentEvent::Error(error),
        AgentEvent::Metadata(diagnostic),
        AgentEvent::Usage(usage),
      ] = diagnostic_events
      else {
        panic!("failed empty assistant must retain error, correlation, and usage: {events:?}")
      };
      assert!(matches!(error.provider, Provider::Pi));
      assert_eq!(error.session_id.as_deref(), Some("pi-errors"));
      assert_eq!(error.message, "Connection error.");
      assert_eq!(error.native.as_ref(), Some(&native));
      assert_eq!(error.timestamp.as_deref(), native["timestamp"].as_str());
      assert!(matches!(diagnostic.kind, MetadataKind::Diagnostic));
      assert_eq!(diagnostic.native, native);
      assert_eq!(usage.message_id.as_deref(), native["id"].as_str());
      assert_eq!(usage.record_id, usage.message_id);
      assert_eq!(usage.native, native["message"]["usage"]);
      assert_eq!(usage.total_tokens, Some(0));
    }
    live_events.extend(events);
  }
  assert_eq!(loaded.reference.id, "pi-errors");
  assert_eq!(
    loaded
      .events
      .iter()
      .filter(|event| matches!(event, AgentEvent::Error(_)))
      .count(),
    4
  );
  assert!(
    !loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Unknown(_)))
  );
  assert_eq!(
    serde_json::to_value(loaded.events).unwrap(),
    serde_json::to_value(live_events).unwrap()
  );
}

#[test]
fn failed_response_without_usage_retains_the_exact_native_message() {
  let (native, events) = normalize(json!({
    "role":"assistant", "content":[], "provider":"zai", "model":"glm-5.2",
    "stopReason":"error", "errorMessage":"Connection error.", "diagnostics":{"retry":4},
  }));
  let [model, AgentEvent::Error(error), AgentEvent::Metadata(diagnostic)] = &events[..] else {
    panic!("expected visible error and native diagnostic")
  };
  assert_model_request(model, &native);
  assert_eq!(error.message, "Connection error.");
  assert_eq!(error.native.as_ref(), Some(&native));
  assert_eq!(diagnostic.native, native);
  assert_eq!(diagnostic.native["id"], "assistant-1");
  assert_eq!(diagnostic.native["parentId"], "user-1");
  assert_eq!(diagnostic.native["message"]["diagnostics"]["retry"], 4);
}

#[test]
fn explicit_failure_has_a_readable_fallback_when_error_text_is_absent_or_blank() {
  for error_message in [Value::Null, json!(""), json!(" \n\t ")] {
    let (_, events) = normalize(json!({
      "role":"assistant", "content":[], "stopReason":"error", "errorMessage":error_message,
    }));
    assert!(
      matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(_)]
      if error.message == "Pi assistant response failed")
    );
  }
}

#[test]
fn explicit_error_text_is_not_lost_when_stop_reason_is_missing() {
  let (_, events) = normalize(json!({
    "role":"assistant", "content":[], "errorMessage":"Provider rejected this request.",
  }));
  assert!(
    matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(_)]
    if error.message == "Provider rejected this request.")
  );
}

#[test]
fn partial_response_and_usage_survive_a_model_failure() {
  let (native, events) = normalize(json!({
    "role":"assistant", "content":[{"type":"text","text":"A partial reply."}],
    "stopReason":"error", "errorMessage":"Connection interrupted.",
    "usage":{"input":10,"output":3,"cacheRead":2,"cacheWrite":0,"totalTokens":15,"cost":{"total":0.01}},
  }));
  let [
    AgentEvent::Message(message),
    AgentEvent::Error(error),
    AgentEvent::Metadata(diagnostic),
    AgentEvent::Usage(usage),
  ] = &events[..]
  else {
    panic!("expected content, failure, native correlation, and usage")
  };
  assert!(matches!(message.role, Role::Assistant));
  assert_eq!(message.text, "A partial reply.");
  assert_eq!(message.message_id.as_deref(), Some("assistant-1"));
  assert_eq!(message.parent_id.as_deref(), Some("user-1"));
  assert_eq!(error.message, "Connection interrupted.");
  assert_eq!(error.native.as_ref(), Some(&native));
  assert_eq!(diagnostic.native, native);
  assert_eq!(usage.message_id, message.message_id);
  assert_eq!(usage.input_tokens, 12);
  assert_eq!(usage.output_tokens, 3);
  assert_eq!(usage.native, native["message"]["usage"]);
}

#[test]
fn abort_is_a_correlated_cancellation_diagnostic_instead_of_a_model_error() {
  for content in [json!([]), json!([{"type":"text","text":"Partial before Stop."}])] {
    let (native, events) = normalize(json!({
      "role":"assistant", "content":content, "stopReason":"aborted", "errorMessage":"Request aborted.",
      "usage":{"input":10,"output":1,"totalTokens":11},
    }));
    assert!(!events.iter().any(|event| matches!(
      event,
      AgentEvent::Error(_) | AgentEvent::Unknown(_) | AgentEvent::Lifecycle(_)
    )));
    let diagnostic = events
      .iter()
      .find_map(|event| match event {
        AgentEvent::Metadata(event) => Some(event),
        _ => None,
      })
      .unwrap();
    assert!(matches!(diagnostic.kind, MetadataKind::Diagnostic));
    assert_eq!(diagnostic.summary, "Pi assistant response cancelled");
    assert_eq!(diagnostic.native, native);
    assert!(
      matches!(events.last(), Some(AgentEvent::Usage(usage)) if usage.message_id.as_deref() == Some("assistant-1"))
    );
    if !content.as_array().unwrap().is_empty() {
      assert!(matches!(events.first(), Some(AgentEvent::Message(message)) if message.text == "Partial before Stop."));
    }
  }
  let (_, events) = normalize(json!({"role":"assistant", "content":[], "stopReason":"aborted"}));
  assert!(matches!(&events[..], [AgentEvent::Metadata(event)] if event.summary == "Pi assistant response cancelled"));
}

#[test]
fn ordinary_stop_reasons_and_blank_error_fields_do_not_manufacture_failures() {
  for stop_reason in [
    Value::Null,
    json!("stop"),
    json!("length"),
    json!("toolUse"),
    json!("future_reason"),
  ] {
    let (_, events) = normalize(json!({
      "role":"assistant", "content":[{"type":"text","text":"Normal reply."}],
      "stopReason":stop_reason, "errorMessage":" \t ",
    }));
    assert!(matches!(&events[..], [AgentEvent::Message(message)] if message.text == "Normal reply."));
  }
}

#[test]
fn failures_do_not_hide_unsupported_content_or_invalid_accounting() {
  let (_, events) = normalize(json!({
    "role":"assistant", "content":[{"type":"future_block","value":42}],
    "stopReason":"error", "errorMessage":"Connection error.", "usage":{"input":-1,"output":0},
  }));
  let [
    AgentEvent::Unknown(content),
    AgentEvent::Error(_),
    AgentEvent::Metadata(_),
    AgentEvent::Unknown(usage),
  ] = &events[..]
  else {
    panic!("unknown content/accounting must remain visible")
  };
  assert_eq!(content.native.as_ref().unwrap()["value"], 42);
  assert_eq!(usage.native.as_ref().unwrap()["input"], -1);
}

#[test]
fn native_run_failure_empty_text_is_not_unknown_content() {
  // Pi 0.80.6 Agent.handleRunFailure uses a recognized empty text block,
  // rather than content:[], for both failed and aborted assistant responses.
  for reason in ["error", "aborted"] {
    for with_usage in [false, true] {
      let mut message = json!({
        "role":"assistant", "content":[{"type":"text","text":""}],
        "api":"synthetic-api", "provider":"synthetic-provider", "model":"synthetic-model",
        "stopReason":reason, "errorMessage":"Synthetic run failure", "timestamp":1792368002000_u64,
      });
      if with_usage {
        message["usage"] = json!({
          "input":0, "output":0, "cacheRead":0, "cacheWrite":0, "totalTokens":0,
          "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0},
        });
      }
      let (native, events) = normalize(message);
      assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::Unknown(_) | AgentEvent::Message(_) | AgentEvent::Lifecycle(_)
      )));
      assert_eq!(
        events
          .iter()
          .filter(|event| matches!(event, AgentEvent::Error(_)))
          .count(),
        usize::from(reason == "error")
      );
      let diagnostic = events
        .iter()
        .find_map(|event| match event {
          AgentEvent::Metadata(event) => Some(event),
          _ => None,
        })
        .unwrap();
      assert_eq!(diagnostic.native, native);
      assert_eq!(
        diagnostic.summary,
        if reason == "error" {
          "Pi assistant response failed"
        } else {
          "Pi assistant response cancelled"
        }
      );
      assert_eq!(
        events
          .iter()
          .filter(|event| matches!(event, AgentEvent::Usage(_)))
          .count(),
        usize::from(with_usage)
      );
    }
  }
}

#[test]
fn known_outcomes_remove_only_empty_recognized_text_and_keep_other_content() {
  for reason in ["error", "aborted"] {
    let (native, events) = normalize(json!({
      "role":"assistant", "stopReason":reason, "errorMessage":"Synthetic failure",
      "content":[
        {"type":"text","text":"","future_annotation":"preserved in diagnostic"},
        {"type":"text","text":"Partial reply."},
        {"type":"text"},
        {"type":"text","text":42},
        {"type":"future_block","value":42}
      ],
      "usage":{"input":4,"output":2,"totalTokens":6}
    }));
    assert!(matches!(&events[0], AgentEvent::Message(event) if event.text == "Partial reply."));
    let unknowns: Vec<_> = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Unknown(event) => Some(event),
        _ => None,
      })
      .collect();
    assert_eq!(unknowns.len(), 3);
    assert_eq!(unknowns[0].native_type.as_deref(), Some("message.content.text"));
    assert_eq!(unknowns[1].native.as_ref().unwrap()["text"], 42);
    assert_eq!(unknowns[2].native.as_ref().unwrap()["type"], "future_block");
    assert!(matches!(events.last(), Some(AgentEvent::Usage(usage)) if usage.total_tokens == Some(6)));
    let diagnostic = events
      .iter()
      .find_map(|event| match event {
        AgentEvent::Metadata(event) => Some(event),
        _ => None,
      })
      .unwrap();
    assert_eq!(diagnostic.native, native);
  }
}

#[test]
fn legacy_error_records_choose_nonblank_text_and_keep_the_exact_envelope() {
  for (message, error, expected) in [
    (
      json!("Primary failure"),
      json!({"message":"Secondary failure"}),
      "Primary failure".to_string(),
    ),
    (
      json!(""),
      json!("String payload failure"),
      "String payload failure".to_string(),
    ),
    (
      json!(" \n\t "),
      json!({"code":429,"message":"Structured failure"}),
      json!({"code":429,"message":"Structured failure"}).to_string(),
    ),
    (Value::Null, json!(404), "404".to_string()),
    (
      Value::Null,
      json!(["Multiple", "failures"]),
      json!(["Multiple", "failures"]).to_string(),
    ),
  ] {
    let native = json!({
      "type":"error", "id":"legacy-error", "parentId":"assistant-1", "timestamp":"2026-10-19T00:00:03.000Z",
      "message":message, "error":error, "provider_metadata":{"request_id":"synthetic-request"}
    });
    let events = normalize_record(native.clone());
    let [AgentEvent::Error(error), AgentEvent::Metadata(diagnostic)] = &events[..] else {
      panic!("expected one visible error plus native diagnostic: {events:?}")
    };
    assert_eq!(error.message, expected);
    assert_eq!(error.native.as_ref(), Some(&native));
    assert_eq!(error.session_id.as_deref(), Some("pi-errors"));
    assert_eq!(error.timestamp.as_deref(), native["timestamp"].as_str());
    assert!(matches!(diagnostic.kind, MetadataKind::Diagnostic));
    assert_eq!(diagnostic.native_type, "error");
    assert_eq!(diagnostic.native, native);
    assert_eq!(diagnostic.timestamp, error.timestamp);
  }
}

#[test]
fn legacy_errors_without_meaningful_payload_have_a_readable_fallback() {
  for payload in [
    Value::Null,
    json!(""),
    json!(" \n\t "),
    json!({}),
    json!([]),
    json!(false),
  ] {
    let native = json!({"type":"error", "id":"legacy-error", "message":" \t ", "error":payload});
    let events = normalize_record(native.clone());
    assert!(
      matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(diagnostic)] if error.message == "unknown pi error" && diagnostic.native == native)
    );
  }
  let events = normalize_record(json!({"type":"error"}));
  assert!(
    matches!(&events[..], [AgentEvent::Error(error), AgentEvent::Metadata(_)] if error.message == "unknown pi error")
  );
}

#[test]
fn separate_identical_failed_attempts_survive_a_later_success() {
  let mut normalizer = PiNormalizer::new();
  normalizer.normalize(serde_json::from_value(json!({"type":"session", "id":"pi-errors"})).unwrap());
  let mut events = Vec::new();
  for (id, parent_id, reason) in [
    ("attempt-1", "user-1", "error"),
    ("attempt-2", "attempt-1", "error"),
    ("success", "attempt-2", "stop"),
  ] {
    let message = if reason == "error" {
      json!({"role":"assistant","content":[{"type":"text","text":""}],"stopReason":reason,"errorMessage":"Identical retry failure"})
    } else {
      json!({"role":"assistant","content":[{"type":"text","text":"Recovered reply."}],"stopReason":reason})
    };
    events.extend(normalizer.normalize(
      serde_json::from_value(json!({"type":"message","id":id,"parentId":parent_id,"message":message})).unwrap(),
    ));
  }
  assert_eq!(
    events
      .iter()
      .filter(|event| matches!(event, AgentEvent::Error(error) if error.message == "Identical retry failure"))
      .count(),
    2
  );
  let ids: Vec<_> = events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::Metadata(event) => event.native["id"].as_str(),
      _ => None,
    })
    .collect();
  assert_eq!(ids, ["attempt-1", "attempt-2"]);
  let failed_records: Vec<_> = events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::Error(event) => event.native.as_ref(),
      _ => None,
    })
    .collect();
  assert_eq!(failed_records.len(), 2);
  assert_eq!(failed_records[0]["id"], "attempt-1");
  assert_eq!(failed_records[0]["parentId"], "user-1");
  assert_eq!(failed_records[1]["id"], "attempt-2");
  assert_eq!(failed_records[1]["parentId"], "attempt-1");
  assert!(matches!(events.last(), Some(AgentEvent::Message(event)) if event.text == "Recovered reply."));
  assert!(
    !events
      .iter()
      .any(|event| matches!(event, AgentEvent::Unknown(_) | AgentEvent::Lifecycle(_)))
  );
}

#[test]
fn tool_error_flags_remain_operation_outcomes_instead_of_provider_errors() {
  for (flag, text) in [
    (Some(true), "Operation aborted"),
    (Some(true), "Tool execution was blocked"),
    (Some(false), "error: literal successful output"),
    (None, "error: disposition unspecified"),
  ] {
    let mut message = json!({
      "role":"toolResult", "toolCallId":"tool-1", "toolName":"bash",
      "content":[{"type":"text","text":text}], "details":{"error":"diagnostic payload"}
    });
    if let Some(flag) = flag {
      message["isError"] = json!(flag);
    }
    let (_, events) = normalize(message);
    let [AgentEvent::ToolCall(result)] = &events[..] else {
      panic!("tool result must not emit a provider failure: {events:?}")
    };
    assert_eq!(result.is_error, flag);
    assert_eq!(result.tool_call_id.as_deref(), Some("tool-1"));
    assert_eq!(result.message_id.as_deref(), Some("assistant-1"));
    assert_eq!(result.parent_id.as_deref(), Some("user-1"));
    assert_eq!(result.output.as_ref().unwrap()["content"][0]["text"], text);
    assert_eq!(
      result.output.as_ref().unwrap()["details"]["error"],
      "diagnostic payload"
    );
  }
}

#[test]
fn redacted_thinking_is_withheld_from_failure_and_cancellation_native_payloads() {
  for reason in ["error", "aborted"] {
    let (native, events) = normalize(json!({
      "role":"assistant", "provider":"synthetic-provider", "model":"synthetic-model",
      "stopReason":reason, "errorMessage":"Synthetic connection failure", "errorCode":"connection_reset",
      "diagnostics":{"code":"SYNTHETIC_FAILURE", "request_id":"synthetic-request"},
      "content":[
        {"type":"thinking", "redacted":true,
          "thinking":"withheld-thinking", "thinkingSignature":"withheld-thinking-signature",
          "text":"withheld-text", "signature":"withheld-signature", "data":"withheld-data",
          "future_payload":{"secret":"withheld-future-payload"}},
        {"type":"thinking", "redacted":false, "thinking":"Visible reasoning",
          "thinkingSignature":"visible-signature", "future_payload":{"safe":true}},
        {"type":"text", "text":"Partial visible reply."},
        {"type":"future_block", "safe_payload":42}
      ],
      "usage":{"input":4,"output":2,"totalTokens":6}
    }));
    let mut expected = native.clone();
    expected["message"]["content"][0] = json!({"type":"thinking", "redacted":true});
    let diagnostic = events
      .iter()
      .find_map(|event| match event {
        AgentEvent::Metadata(event) => Some(event),
        _ => None,
      })
      .unwrap();
    assert_eq!(diagnostic.native, expected);
    let errors: Vec<_> = events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Error(event) => Some(event),
        _ => None,
      })
      .collect();
    if reason == "error" {
      let [error] = &errors[..] else {
        panic!("expected exactly one visible failure")
      };
      assert_eq!(error.message, "Synthetic connection failure");
      assert_eq!(error.native.as_ref(), Some(&expected));
    } else {
      assert!(errors.is_empty());
    }
    for payload in std::iter::once(&diagnostic.native).chain(errors.iter().filter_map(|error| error.native.as_ref())) {
      let serialized = serde_json::to_string(payload).unwrap();
      assert!(
        !serialized.contains("withheld-"),
        "withheld thinking leaked through native diagnostics"
      );
      assert_eq!(payload["id"], "assistant-1");
      assert_eq!(payload["parentId"], "user-1");
      assert_eq!(payload["message"]["errorCode"], "connection_reset");
      assert_eq!(payload["message"]["diagnostics"]["code"], "SYNTHETIC_FAILURE");
      assert_eq!(payload["message"]["content"][1], native["message"]["content"][1]);
      assert_eq!(payload["message"]["content"][2], native["message"]["content"][2]);
      assert_eq!(payload["message"]["content"][3], native["message"]["content"][3]);
    }
    assert_model_request(&events[0], &native);
    assert!(matches!(&events[1], AgentEvent::Reasoning(event) if event.redacted == Some(true) && event.text.is_none()));
    assert!(matches!(events.last(), Some(AgentEvent::Usage(event)) if event.total_tokens == Some(6)));
  }
}
