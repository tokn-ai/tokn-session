use serde_json::{Value, json};
use tokn_session_core::{AgentEvent, CompactionState, LiveSessionEvent, Phase, Provider};

use crate::{
  OpenCodeLiveNormalizer, OpenCodeSessionSource,
  normalize::OpenCodeNormalizer,
  row::{OpenCodeMessageRow, OpenCodePartRow},
};

fn message(error: Value, parts: Vec<Value>) -> OpenCodeMessageRow {
  row(json!({"role":"assistant", "parentID":"parent-1", "error":error}), parts)
}

fn row(data: Value, parts: Vec<Value>) -> OpenCodeMessageRow {
  OpenCodeMessageRow {
    id: "message-1".into(),
    time_created: Some(100),
    data: serde_json::from_value(data).unwrap(),
    parts: parts
      .into_iter()
      .enumerate()
      .map(|(index, data)| OpenCodePartRow {
        id: format!("part-{index}"),
        time_created: Some(100),
        data: serde_json::from_value(data).unwrap(),
      })
      .collect(),
  }
}

#[test]
fn historical_failures_preserve_content_usage_native_and_provider_identity() {
  for provider in [Provider::OpenCode, Provider::ZCode] {
    let error = json!({"name":"APIError", "data":{"message":"Connection error.", "code":"CONNECTION_RESET", "statusCode":503, "isRetryable":true}});
    let data = json!({"role":"assistant", "parentID":"parent-1", "error":error,
      "tokens":{"input":10,"output":2,"cache":{"read":3,"write":0}}});
    let mut normalizer = OpenCodeNormalizer::with_provider("session-1".into(), provider);
    let events = normalizer.normalize_message(row(data.clone(), vec![json!({"type":"text", "text":"Partial reply"})]));
    let [
      AgentEvent::Message(message),
      AgentEvent::Usage(usage),
      AgentEvent::Error(failure),
      AgentEvent::Metadata(diagnostic),
    ] = &events[..]
    else {
      panic!("expected one correlated failure with its original content/accounting: {events:?}")
    };
    assert_eq!(failure.provider, provider);
    assert_eq!(failure.message, "Connection error.");
    assert_eq!(failure.native, Some(json!({"id":"message-1", "data":data})));
    assert_eq!(diagnostic.provider, provider);
    assert_eq!(diagnostic.native, json!({"id":"message-1", "data":data}));
    assert_eq!(message.message_id.as_deref(), Some("message-1"));
    assert_eq!(message.parent_id.as_deref(), Some("parent-1"));
    assert_eq!(message.text, "Partial reply");
    assert_eq!(usage.provider, provider);
    assert_eq!(usage.message_id, message.message_id);
    assert_eq!(usage.input_tokens, 13);
    assert_eq!(usage.output_tokens, 2);
  }
}

#[test]
fn explicitly_hidden_zcode_errors_and_diagnostics_keep_machine_native_details() {
  for field in ["transcriptVisibility", "uiVisibility"] {
    let mut data = json!({"role":"assistant", "error":{"name":"APIError", "data":{"message":"failed"}},
      "semantics":{}, "privatePayload":"hidden source content"});
    data["semantics"][field] = json!("hidden");
    let mut normalizer = OpenCodeNormalizer::with_provider("session-1".into(), Provider::ZCode);
    let events = normalizer.normalize_message(row(data.clone(), vec![]));
    let native = json!({"id":"message-1", "data":data});
    assert!(
      matches!(&events[..], [AgentEvent::Error(failure), AgentEvent::Metadata(diagnostic)]
      if failure.native.as_ref() == Some(&native) && diagnostic.native == native)
    );
    assert!(events.iter().all(AgentEvent::is_hidden));
  }
}

#[test]
fn tagged_cancellation_is_diagnostic_in_history_and_interrupted_in_compaction() {
  for provider in [Provider::OpenCode, Provider::ZCode] {
    let error = json!({"name":"MessageAbortedError", "data":{"message":"Stopped"}});
    let mut normalizer = OpenCodeNormalizer::with_provider("session-1".into(), provider);
    let events = normalizer.normalize_message(message(
      error.clone(),
      vec![json!({"type":"text","text":"Partial reply"})],
    ));
    assert!(
      matches!(&events[..], [AgentEvent::Message(_), AgentEvent::Metadata(diagnostic)]
      if diagnostic.summary == "Provider response interrupted" && diagnostic.native["data"]["error"] == error)
    );
    let data = json!({"role":"assistant", "parentID":"parent-1", "summary":true, "finish":"stop", "error":error});
    let events = normalizer.normalize_message(row(data.clone(), vec![]));
    assert!(
      matches!(&events[..], [AgentEvent::Compaction(event), AgentEvent::Metadata(diagnostic)]
      if event.provider == provider && event.state == CompactionState::Interrupted
        && event.reason.as_deref() == Some("Stopped") && diagnostic.native == json!({"id":"message-1", "data":data}))
    );
  }
}

#[test]
fn blank_errors_have_useful_fallbacks_and_repeated_records_are_not_deduplicated() {
  for (error, expected) in [
    (
      json!({"name":"APIError", "data":{"message":" \n "},"message":"outer explanation"}),
      "outer explanation",
    ),
    (
      json!({"name":"MessageOutputLengthError", "data":{}}),
      "MessageOutputLengthError",
    ),
    (json!({"name":"APIError", "data":{"message":""}}), "APIError"),
    (json!({}), "Provider request failed"),
    (json!("plain failure"), "plain failure"),
    (json!(" "), "Provider request failed"),
    (json!({"name":"APIError", "data":{"message":"cancelled"}}), "cancelled"),
    (
      json!({"name":" MessageAbortedError ", "data":{"message":"not a valid cancellation tag"}}),
      "not a valid cancellation tag",
    ),
  ] {
    let mut normalizer = OpenCodeNormalizer::new("session-1".into());
    for id in ["first", "second"] {
      let data = json!({"role":"assistant", "parentID":format!("parent-{id}"), "error":error});
      let mut record = row(data.clone(), vec![]);
      record.id = id.into();
      let events = normalizer.normalize_message(record);
      assert!(
        matches!(&events[..], [AgentEvent::Error(failure), AgentEvent::Metadata(diagnostic)]
        if failure.message == expected && diagnostic.native["id"] == id
          && failure.native.as_ref() == Some(&json!({"id":id, "data":data})))
      );
    }
  }
}

#[test]
fn recovered_assistant_errors_do_not_duplicate_and_invalid_errors_remain_lossless() {
  let mut normalizer = OpenCodeNormalizer::new("session-1".into());
  let data = json!({"role":"assistant", "providerID":42, "error":{"name":"APIError", "data":{"message":"failed"}}});
  let events = normalizer.normalize_message(row(data.clone(), vec![]));
  assert_eq!(
    events
      .iter()
      .filter(|event| matches!(event, AgentEvent::Error(_)))
      .count(),
    1
  );
  assert!(
    matches!(&events[..], [AgentEvent::Unknown(_), AgentEvent::Error(failure), AgentEvent::Metadata(diagnostic)]
    if diagnostic.native == json!({"id":"message-1", "data":data})
      && failure.native.as_ref() == Some(&diagnostic.native))
  );
  assert!(normalizer.normalize_message(message(Value::Null, vec![])).is_empty());
  for error in [json!(false), json!(42), json!([])] {
    let events = normalizer.normalize_message(message(error.clone(), vec![]));
    assert!(
      matches!(&events[..], [AgentEvent::Unknown(event)] if event.native.as_ref().unwrap()["data"]["error"] == error)
    );
  }
}

#[test]
fn reasoning_only_response_terminates_after_its_parts_for_typed_and_recovered_assistants() {
  for provider in [Provider::OpenCode, Provider::ZCode] {
    for cancelled in [false, true] {
      for recovered in [false, true] {
        let mut data = json!({"role":"assistant", "parentID":"parent-1", "error":{
          "name":if cancelled { "MessageAbortedError" } else { "APIError" }, "data":{"message":"terminal outcome"}
        }, "tokens":{"input":1,"output":2}});
        if recovered {
          data["providerID"] = json!(42);
        }
        let mut normalizer = OpenCodeNormalizer::with_provider("session-1".into(), provider);
        let events =
          normalizer.normalize_message(row(data, vec![json!({"type":"reasoning", "text":"Partial reasoning"})]));
        assert!(matches!(events.first(), Some(AgentEvent::Reasoning(_))));
        assert!(matches!(events.last(), Some(AgentEvent::Metadata(_))));
        assert_eq!(events.last().unwrap().is_response_cancelled(), cancelled);
        let usage = events
          .iter()
          .position(|event| matches!(event, AgentEvent::Usage(_)))
          .unwrap();
        let terminal = events
          .iter()
          .position(|event| matches!(event, AgentEvent::Error(_) | AgentEvent::Metadata(_)))
          .unwrap();
        assert!(usage < terminal);
        assert_eq!(
          events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Error(_)))
            .count(),
          usize::from(!cancelled)
        );
      }
    }
  }
}

#[test]
fn malformed_summary_errors_keep_the_entire_row_without_claiming_a_compaction_outcome() {
  for provider in [Provider::OpenCode, Provider::ZCode] {
    for error in [json!(false), json!(42), json!([])] {
      let record = row(
        json!({"role":"assistant", "summary":true, "finish":"stop", "error":error}),
        vec![json!({"type":"text", "text":"Partial summary"})],
      );
      let native = serde_json::to_value(&record).unwrap();
      let mut normalizer = OpenCodeNormalizer::with_provider("session-1".into(), provider);
      let events = normalizer.normalize_message(record);
      assert!(matches!(&events[..], [AgentEvent::Unknown(event)]
        if event.provider == provider && event.native_type.as_deref() == Some("compaction.summary")
          && event.native.as_ref() == Some(&native)));
    }
  }
}

#[test]
fn live_errors_and_cancellations_preserve_the_full_run_envelope() {
  for (error, failure) in [
    (
      json!({"name":"APIError", "data":{"message":"failed", "code":"CONNECTION_RESET"}}),
      true,
    ),
    (
      json!({"name":"MessageAbortedError", "data":{"message":"Stopped"}}),
      false,
    ),
    (json!({"name":"MessageOutputLengthError", "data":{}}), true),
  ] {
    for id in ["first", "second"] {
      let native = json!({"type":"error", "sessionID":"session-1", "timestamp":100,
        "messageID":id, "parentID":format!("parent-{id}"), "error":error, "future":true});
      let events = OpenCodeLiveNormalizer::normalize_line(&native.to_string()).unwrap();
      let failures = events
        .iter()
        .filter_map(|event| match event {
          LiveSessionEvent::Event(AgentEvent::Error(error)) => Some(error),
          _ => None,
        })
        .collect::<Vec<_>>();
      assert_eq!(failures.len(), usize::from(failure));
      for failure in failures {
        assert_eq!(failure.native.as_ref(), Some(&native));
      }
      let Some(LiveSessionEvent::Event(AgentEvent::Metadata(diagnostic))) = events.last() else {
        panic!("expected native diagnostic")
      };
      assert_eq!(diagnostic.native, native);
      assert_eq!(diagnostic.provider, Provider::OpenCode);
      assert_eq!(diagnostic.session_id.as_deref(), Some("session-1"));
    }
  }
  for error in [Value::Null, json!(42), json!(false), json!([])] {
    let native = json!({"type":"error", "sessionID":"session-1", "error":error});
    let events = OpenCodeLiveNormalizer::normalize_line(&native.to_string()).unwrap();
    assert!(matches!(&events[..], [LiveSessionEvent::Unknown(event)] if event.native.as_ref() == Some(&native)));
  }
}

#[test]
fn malformed_terminal_tools_finish_while_unknown_statuses_stay_ambiguous() {
  for provider in [Provider::OpenCode, Provider::ZCode] {
    let mut normalizer = OpenCodeNormalizer::with_provider("session-1".into(), provider);
    for state in [
      json!({"status":"error", "error":42, "input":{"command":"test"}, "metadata":{"interrupted":true}}),
      json!({"status":"error", "error":"Tool execution aborted", "metadata":{"interrupted":true}}),
    ] {
      let part = json!({"type":"tool", "tool":"bash", "callID":"call-1", "state":state});
      let events = normalizer.normalize_message(message(Value::Null, vec![part.clone()]));
      assert!(matches!(&events[..], [AgentEvent::ToolCall(tool)]
        if tool.provider == provider && matches!(tool.phase, Phase::Finished) && tool.is_error == Some(true)
          && tool.tool_call_id.as_deref() == Some("call-1") && tool.message_id.as_deref() == Some("message-1")
          && tool.parent_id.as_deref() == Some("parent-1") && tool.native.as_ref() == Some(&state)));
      let live = json!({"type":"tool_use", "sessionID":"session-1", "part":part});
      let events = OpenCodeLiveNormalizer::normalize_line(&live.to_string()).unwrap();
      assert!(
        matches!(&events[..], [LiveSessionEvent::Event(AgentEvent::ToolCall(tool))]
        if matches!(tool.phase, Phase::Finished) && tool.is_error == Some(true) && tool.native.as_ref() == Some(&state))
      );
    }
    let events = normalizer.normalize_message(message(
      Value::Null,
      vec![json!({"type":"tool", "state":{"status":"future", "error":"not authoritative"}})],
    ));
    assert!(
      matches!(&events[..], [AgentEvent::ToolCall(tool)] if matches!(tool.phase, Phase::Updated) && tool.is_error.is_none())
    );
  }
}

#[test]
fn sqlite_history_dispatch_keeps_errors_without_optional_native_loading() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("opencode.db");
  let connection = rusqlite::Connection::open(&database).unwrap();
  connection.execute_batch(
    "CREATE TABLE session(id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
     CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
     CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
     INSERT INTO session VALUES('session-1', NULL, '/synthetic/project', 'Errors', 100, 101);",
  ).unwrap();
  let native = json!({"role":"assistant", "parentID":"parent-1", "error":{"name":"APIError", "data":{"message":"failed", "future":true}}});
  connection
    .execute(
      "INSERT INTO message VALUES('message-1', 'session-1', 101, ?1)",
      [native.to_string()],
    )
    .unwrap();
  drop(connection);
  let source = OpenCodeSessionSource::new(Some(database));
  let loaded = source.load_session_exact("session-1").unwrap();
  assert!(
    matches!(&loaded.events[..], [AgentEvent::SessionStarted(_), AgentEvent::Error(failure), AgentEvent::Metadata(diagnostic)]
    if diagnostic.native == json!({"id":"message-1", "data":native})
      && failure.native.as_ref() == Some(&diagnostic.native))
  );
}
