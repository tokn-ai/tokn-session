use serde_json::{Value, json};
use tokn_session_dsh::DshSessionSource;

fn normalize(records: Vec<Value>) -> Vec<Value> {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.jsonl");
  let header = json!({"type":"session","version":0,"id":"events","createdAt":0,"delegationDepth":0});
  let mut lines = vec![header.to_string()];
  lines.extend(records.into_iter().map(|record| record.to_string()));
  std::fs::write(&path, lines.join("\n") + "\n").unwrap();
  let session = DshSessionSource::new(None).load_session_path(&path).unwrap();
  session
    .events
    .into_iter()
    .skip(1)
    .map(|event| serde_json::to_value(event).unwrap())
    .collect()
}

fn record(kind: &str, seq: u64, data: Value) -> Value {
  json!({"type":kind,"seq":seq,"time":1000+seq,"data":data})
}

#[test]
fn compaction_plugin_records_and_checkpoint_are_not_user_messages() {
  let mut checkpoint = record(
    "user/message",
    3,
    json!({"id":"summary","content":[{"type":"text","text":"retained context"}],
    "source":{"kind":"plugin","plugin":"compact","compactionId":"op"}}),
  );
  checkpoint["surfaceOp"] = json!({"op":"replace","start":0,"end":1});
  let events = normalize(vec![
    record("compaction/start", 2, json!({"compactionId":"op","turn":null})),
    checkpoint,
    record("compaction/end", 4, json!({"compactionId":"op","turn":null})),
    record("compaction/start", 5, json!({"compactionId":"broken"})),
  ]);
  assert_eq!(
    events.iter().map(|e| e["type"].as_str().unwrap()).collect::<Vec<_>>(),
    ["compaction", "compaction", "compaction", "unknown"]
  );
  assert_eq!(events[1]["summary"], "retained context");
  assert_eq!(events[2]["state"], "completed");
}

fn assistant(seq: u64, step: u64, usage: Option<Value>) -> Value {
  let mut value = record(
    "assistant/message",
    seq,
    json!({"turn":1,"step":step,"message":{
    "id":format!("message-{step}"),"role":"assistant","source":{"kind":"model"},
    "content":[{"type":"text","text":"answer"}]}}),
  );
  if let Some(usage) = usage {
    value["data"]["usage"] = usage;
  }
  value
}

fn usage(seq: u64, step: u64, tokens: Value) -> Value {
  record(
    "assistant/chunk",
    seq,
    json!({"turn":1,"step":step,"chunk":{"type":"usage","usage":tokens}}),
  )
}

#[test]
fn lifecycle_preserves_outcomes_and_does_not_claim_step_success() {
  let mut records = vec![
    record("turn/start", 0, json!({"turn":1})),
    record("step/start", 1, json!({"turn":1,"step":2})),
    record("step/end", 2, json!({"turn":1,"step":2})),
  ];
  let reasons = [
    (json!({"kind":"completed"}), "completed"),
    (json!({"kind":"aborted","reason":{"kind":"user"}}), "cancelled"),
    (json!({"kind":"interrupted"}), "interrupted"),
    (json!({"kind":"blocked"}), "blocked"),
    (json!({"kind":"max-tokens"}), "token_limit"),
    (
      json!({"kind":"error","error":{"code":"HTTP","message":"failed"}}),
      "failed",
    ),
  ];
  for (index, (reason, _)) in reasons.iter().enumerate() {
    records.push(record(
      "turn/end",
      3 + index as u64,
      json!({"turn":index+1,"reason":reason}),
    ));
  }
  let events = normalize(records);
  let lifecycle: Vec<_> = events.iter().filter(|event| event["type"] == "lifecycle").collect();
  assert_eq!(lifecycle.len(), 9);
  assert_eq!(lifecycle[0]["scope"], "turn");
  assert_eq!(lifecycle[0]["phase"], "started");
  assert_eq!(lifecycle[1]["step_id"], "2");
  assert_eq!(lifecycle[2]["phase"], "finished");
  assert_eq!(lifecycle[2]["outcome"], Value::Null);
  for (event, (reason, outcome)) in lifecycle[3..].iter().zip(reasons) {
    assert_eq!(event["outcome"], outcome);
    assert_eq!(event["native"]["data"]["reason"], reason);
  }
  assert_eq!(events.iter().filter(|event| event["type"] == "error").count(), 1);
  assert!(!events.iter().any(|event| event["type"] == "unknown"));
}

#[test]
fn native_legacy_turn_endings_preserve_failures_cancellations_and_raw_attribution() {
  // These are the precise pre-react-loop shapes migrated by the pinned DSH
  // session-persistence coordinator, alongside the current error contract.
  let cases = [
    (
      json!({"kind":"error","step":1,"failure":{"message":"old provider failure","code":"SERVER"}}),
      "failed",
      Some("old provider failure"),
    ),
    (
      json!({"kind":"error","step":1,"message":"old thrown value"}),
      "failed",
      Some("old thrown value"),
    ),
    (
      json!({"kind":"error","step":0,"message":"old coded error","code":"CODED"}),
      "failed",
      Some("old coded error"),
    ),
    (
      json!({"kind":"error","step":0,"failure":{"message":"rate limited","code":"RATE_LIMIT","status":429,"providerRetryAfterMs":1000,"requestId":"request-1"}}),
      "failed",
      Some("rate limited"),
    ),
    (json!({"kind":"aborted"}), "cancelled", None),
    (json!({"kind":"disposed"}), "cancelled", None),
    (
      json!({"kind":"error","error":{"message":"current failure","code":"HTTP","requestId":"current-request"}}),
      "failed",
      Some("current failure"),
    ),
    (json!({"kind":"aborted","reason":{"kind":"user"}}), "cancelled", None),
  ];
  let records = cases
    .iter()
    .enumerate()
    .map(|(index, (reason, _, _))| record("turn/end", index as u64, json!({"turn":index + 1,"reason":reason})))
    .collect::<Vec<_>>();
  let events = normalize(records.clone());
  let lifecycle = events
    .iter()
    .filter(|event| event["type"] == "lifecycle")
    .collect::<Vec<_>>();
  assert_eq!(lifecycle.len(), cases.len());
  let errors = events
    .iter()
    .filter(|event| event["type"] == "error")
    .collect::<Vec<_>>();
  assert_eq!(
    errors.len(),
    cases.iter().filter(|(_, _, message)| message.is_some()).count()
  );
  for (index, ((_, outcome, message), original)) in cases.iter().zip(&records).enumerate() {
    assert_eq!(lifecycle[index]["outcome"], *outcome);
    assert_eq!(lifecycle[index]["scope"], "turn");
    assert_eq!(lifecycle[index]["turn_id"], (index + 1).to_string());
    assert_eq!(lifecycle[index]["native"], *original);
    if let Some(message) = message {
      let matching = errors
        .iter()
        .filter(|error| error["timestamp"] == original["time"].as_u64().unwrap().to_string())
        .collect::<Vec<_>>();
      assert_eq!(matching.len(), 1);
      assert_eq!(matching[0]["message"], *message);
      assert_eq!(matching[0]["native"], *original);
    }
  }
  assert!(!events.iter().any(|event| event["type"] == "unknown"));
}

#[test]
fn repeated_failure_messages_keep_their_exact_turn_end_records() {
  let first = record(
    "turn/end",
    4,
    json!({"turn":3,"reason":{"kind":"error","error":{
      "message":"request failed","code":"HTTP","requestId":"request-first"}}}),
  );
  let cancelled = record(
    "turn/end",
    5,
    json!({"turn":4,"reason":{"kind":"aborted","reason":{"kind":"user"}}}),
  );
  let mut second = record(
    "turn/end",
    6,
    json!({"turn":5,"reason":{"kind":"error","error":{
      "message":"request failed","code":"RATE_LIMIT","requestId":"request-second"}}}),
  );
  // Neither the visible message nor the timestamp identifies a source record.
  second["time"] = first["time"].clone();
  let events = normalize(vec![first.clone(), cancelled.clone(), second.clone()]);
  assert_eq!(events.len(), 5);
  for (error, lifecycle, original) in [(&events[0], &events[1], &first), (&events[3], &events[4], &second)] {
    assert_eq!(error["type"], "error");
    assert_eq!(error["message"], "request failed");
    assert_eq!(error["native"], *original);
    assert_eq!(lifecycle["type"], "lifecycle");
    assert_eq!(lifecycle["outcome"], "failed");
    assert_eq!(lifecycle["native"], error["native"]);
  }
  assert_eq!(events[2]["type"], "lifecycle");
  assert_eq!(events[2]["outcome"], "cancelled");
  assert_eq!(events[2]["native"], cancelled);
}

#[test]
fn malformed_or_unfamiliar_legacy_endings_remain_unknown_without_inferred_errors() {
  let reasons = [
    json!({"kind":"error","message":"no step"}),
    json!({"kind":"error","step":-1,"message":"bad step"}),
    json!({"kind":"error","step":1,"message":"bad code","code":42}),
    json!({"kind":"error","step":1,"failure":{"message":"bad request","code":"HTTP","requestId":42}}),
    json!({"kind":"error","step":1,"failure":{"message":"future failure","code":"HTTP","future":true}}),
    json!({"kind":"error","step":1,"message":"ambiguous","failure":{"message":"other","code":"HTTP"}}),
    json!({"kind":"error","step":1,"message":"legacy fallback forbidden","error":false}),
    json!({"kind":"aborted","reason":null}),
    json!({"kind":"disposed","future":true}),
  ];
  let mut records = reasons
    .into_iter()
    .enumerate()
    .map(|(index, reason)| record("turn/end", index as u64, json!({"turn":1,"reason":reason})))
    .collect::<Vec<_>>();
  records.push(record("turn/end", 20, json!({"turn":0,"reason":{"kind":"aborted"}})));
  records.push(record(
    "turn/end",
    21,
    json!({"turn":1,"reason":{"kind":"aborted"},"future":true}),
  ));
  for turn in [json!(0), json!(9_007_199_254_740_992_u64)] {
    records.push(record(
      "turn/end",
      22,
      json!({"turn":turn,"reason":{"kind":"error","error":{"message":"bad turn","code":"HTTP"}}}),
    ));
    records.push(record(
      "turn/end",
      23,
      json!({"turn":turn,"reason":{"kind":"completed"}}),
    ));
  }
  let events = normalize(records.clone());
  assert_eq!(events.len(), records.len());
  for (event, record) in events.iter().zip(records) {
    assert_eq!(event["type"], "unknown");
    assert_eq!(event["native"], record);
  }
}

#[test]
fn blank_current_and_legacy_failures_use_codes_or_a_visible_fallback() {
  let cases = [
    (
      json!({"kind":"error","error":{"message":" \n ","code":" HTTP "}}),
      "HTTP",
    ),
    (
      json!({"kind":"error","error":{"message":"","code":" "}}),
      "DSH turn failed",
    ),
    (
      json!({"kind":"error","step":1,"failure":{"message":" ","code":" RATE_LIMIT "}}),
      "RATE_LIMIT",
    ),
    (
      json!({"kind":"error","step":1,"failure":{"message":"","code":""}}),
      "DSH turn failed",
    ),
    (
      json!({"kind":"error","step":1,"message":" \n ","code":" CODED "}),
      "CODED",
    ),
    (json!({"kind":"error","step":1,"message":" "}), "DSH turn failed"),
  ];
  for (reason, expected) in cases {
    let original = record("turn/end", 1, json!({"turn":1,"reason":reason}));
    let events = normalize(vec![original.clone()]);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "error");
    assert_eq!(events[0]["message"], expected);
    assert_eq!(events[1]["type"], "lifecycle");
    assert_eq!(events[1]["outcome"], "failed");
    assert_eq!(events[1]["native"], original);
  }
}

#[test]
fn aborted_tool_results_preserve_native_failure_identity_without_provider_errors() {
  for (index, code) in ["TOOL_ABORTED", "TOOL_ABORTED_BEFORE_DISPATCH"].iter().enumerate() {
    let events = normalize(vec![
      record(
        "tool/call",
        0,
        json!({"turn":1,"step":1,"callId":"call","name":"read_file","arguments":"{}"}),
      ),
      record(
        "tool/result",
        1,
        json!({"turn":1,"step":1,
        "message":{"id":"result","role":"user","source":{"kind":"tool","callId":"call"},
          "content":[{"type":"tool-result","toolCallId":"call","isError":true,"content":[{"type":"text","text":"tool call aborted"}]}]},
        "error":{"name":"AbortError","code":code},"meta":{"index":index}}),
      ),
    ]);
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["type"], "tool_call");
    assert_eq!(events[1]["phase"], "finished");
    assert_eq!(events[1]["is_error"], true);
    assert_eq!(events[1]["tool_call_id"], "call");
    assert_eq!(events[1]["output"]["error"]["code"], *code);
    assert_eq!(events[1]["output"]["meta"]["index"], index);
  }
}

#[test]
fn usage_prefers_assembled_snapshots_and_includes_cache_once() {
  let authoritative = json!({"inputTokens":10,"outputTokens":5,"cacheReadTokens":20,"cacheWriteTokens":3,"reasoningTokens":2,"future":true});
  let events = normalize(vec![
    usage(0, 1, json!({"inputTokens":1,"outputTokens":1})),
    assistant(1, 1, Some(authoritative.clone())),
    // Even a later streamed snapshot must not override assembled usage.
    usage(2, 1, json!({"inputTokens":99,"outputTokens":99})),
  ]);
  assert_eq!(events.len(), 2);
  assert_eq!(events[0]["type"], "message");
  let usage = &events[1];
  assert_eq!(usage["type"], "usage");
  assert_eq!(usage["input_tokens"], 33);
  assert_eq!(usage["kind"], "model_call");
  assert_eq!(
    usage["total_tokens"],
    usage["input_tokens"].as_u64().unwrap() + usage["output_tokens"].as_u64().unwrap()
  );
  assert_eq!(usage["output_tokens"], 5);
  assert_eq!(usage["reasoning_tokens"], 2);
  assert_eq!(usage["message_id"], "message-1");
  assert_eq!(usage["timestamp"], "1001");
  assert_eq!(usage["native"], authoritative);
}

#[test]
fn usage_falls_back_to_last_stream_snapshot_even_with_usage_less_assembled_message() {
  let events = normalize(vec![
    usage(0, 1, json!({"inputTokens":1,"outputTokens":1})),
    usage(1, 1, json!({"inputTokens":2,"outputTokens":3})),
    assistant(2, 1, None),
    usage(3, 2, json!({"inputTokens":4,"outputTokens":5})),
  ]);
  let usage: Vec<_> = events.iter().filter(|event| event["type"] == "usage").collect();
  assert_eq!(usage.len(), 2);
  assert_eq!(usage[0]["input_tokens"], 2);
  assert_eq!(usage[0]["message_id"], Value::Null);
  assert_eq!(usage[1]["step_id"], "2");
  assert_eq!(usage[1]["input_tokens"], 4);
}

#[test]
fn plugin_message_and_reasoning_keep_provenance_without_duplicate_unknowns() {
  let source = json!({"kind":"plugin","name":"reminder","future":42});
  let mut user = record(
    "user/message",
    0,
    json!({"id":"user","role":"user","source":source,
    "content":[{"type":"text","text":"plugin reminder"}]}),
  );
  user["surfaceOp"] = json!({"op":"replace","start":0,"end":2});
  user["sourceEventSeqs"] = json!([7, 8]);
  let mut answer = assistant(1, 1, None);
  answer["data"]["message"]["content"] = json!([{"type":"reasoning","text":"thinking"}]);
  let events = normalize(vec![user, answer]);
  assert_eq!(events.len(), 2);
  assert_eq!(events[0]["type"], "message");
  assert_eq!(events[0]["provenance"]["source"], source);
  assert_eq!(events[0]["provenance"]["surface_op"]["op"], "replace");
  assert_eq!(events[0]["provenance"]["source_event_seqs"], json!([7, 8]));
  assert_eq!(events[1]["type"], "reasoning");
  assert_eq!(events[1]["provenance"]["source"]["kind"], "model");
}

#[test]
fn known_metadata_preserves_native_without_inventing_user_messages() {
  let pending = json!({"id":"pending","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"queued, not yet delivered"}]});
  let records = vec![
    record(
      "agent/inbox/spliced",
      0,
      json!({"target":"next-turn","start":0,"inserted":[pending],"extra":42}),
    ),
    record(
      "session/title",
      1,
      json!({"title":"Example","messageSeqs":[0],"source":{"kind":"fallback"}}),
    ),
    record("permission/preset", 2, json!({"preset":"read-only"})),
    record("sandbox/mode", 3, json!({"mode":"workspace-write"})),
    record("approval/policy", 4, json!({"policy":"on-request"})),
    record(
      "request/context",
      5,
      json!({"provider":"deepseek","model":"test","contextWindow":128000}),
    ),
    record("session/end-seed", 6, json!({})),
    record(
      "todo/write",
      7,
      json!({"todos":[{"content":"test","status":"pending"}]}),
    ),
    record(
      "session/title-llm-request",
      8,
      json!({"titleProvider":"title","messageSeqs":[0],"route":{"provider":"deepseek","model":"test"},"system":"large prompt","messages":[{"role":"user","content":[{"type":"text","text":"example"}]}],"maxTokens":50}),
    ),
    record(
      "web/deepseek-search-llm-request",
      9,
      json!({"endpoint":"https://example.invalid/v1/messages","apiVersion":"2023-06-01","body":{"model":"test","max_tokens":100,"messages":[],"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":5}]}}),
    ),
  ];
  let events = normalize(records.clone());
  assert_eq!(events.len(), records.len());
  for (event, record) in events.iter().zip(records) {
    assert_eq!(event["type"], "metadata");
    assert_eq!(event["native"], record);
    assert_eq!(event["session_id"], "events");
  }
  assert_eq!(events[0]["kind"], "queue");
  assert_eq!(events[8]["kind"], "diagnostic");
}

#[test]
fn unfamiliar_and_malformed_records_remain_unknown_with_exact_native() {
  let mut future = record("plugin/future", 0, json!({"answer":42}));
  future["ignorable"] = json!(true);
  let mut bad_surface = assistant(9, 9, None);
  bad_surface["surfaceOp"] = json!({"op":"new-operation"});
  let mut bad_envelope = record("permission/preset", 10, json!({"preset":"normal"}));
  bad_envelope["time"] = json!("yesterday");
  let records = vec![
    future,
    record("turn/start", 1, json!({"turn":"one"})),
    record("turn/end", 2, json!({"turn":1,"reason":{"kind":"future"}})),
    record(
      "turn/end",
      3,
      json!({"turn":1,"reason":{"kind":"aborted","future":true}}),
    ),
    record("turn/end", 4, json!({"turn":1,"reason":{"kind":"error","error":false}})),
    record("session/title", 5, json!({"title":"missing attribution"})),
    record("permission/preset", 6, json!({"preset":42})),
    record(
      "agent/inbox/spliced",
      7,
      json!({"target":"next-turn","start":0,"inserted":"bad"}),
    ),
    record(
      "assistant/chunk",
      8,
      json!({"turn":1,"step":1,"chunk":{"type":"usage","usage":{"inputTokens":-1,"outputTokens":0}}}),
    ),
    bad_surface,
    bad_envelope,
    usage(
      11,
      2,
      json!({"inputTokens":u64::MAX,"outputTokens":0,"cacheReadTokens":1}),
    ),
  ];
  let events = normalize(records.clone());
  assert_eq!(events.len(), records.len());
  for (event, record) in events.iter().zip(records) {
    assert_eq!(event["type"], "unknown", "{record}");
    assert_eq!(event["native"], record);
  }
}

#[test]
fn unfinished_stream_structure_is_metadata_but_future_chunks_stay_unknown() {
  let records = [
    json!({"type":"block-start","index":0,"blockType":"text"}),
    json!({"type":"text-delta","index":0,"text":"partial"}),
    json!({"type":"tool-call-delta","index":1,"id":"call","name":"read","argumentsDelta":"{"}),
    json!({"type":"finish","reason":{"kind":"max-tokens"}}),
    json!({"type":"future","extra":42}),
    json!({"type":"block-start","index":2,"blockType":"future"}),
  ]
  .into_iter()
  .enumerate()
  .map(|(seq, chunk)| record("assistant/chunk", seq as u64, json!({"turn":1,"step":1,"chunk":chunk})))
  .collect();
  let events = normalize(records);
  let kinds: Vec<_> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
  assert_eq!(
    kinds,
    ["metadata", "message", "metadata", "metadata", "unknown", "unknown"]
  );
  assert_eq!(events[1]["phase"], "delta");
}

#[test]
fn invalid_assembled_surface_does_not_suppress_valid_stream_content_or_usage() {
  let mut invalid = assistant(2, 1, Some(json!({"inputTokens":99,"outputTokens":99})));
  invalid["surfaceOp"] = json!({"op":"replace","start":"bad","end":2});
  let events = normalize(vec![
    record(
      "assistant/chunk",
      0,
      json!({"turn":1,"step":1,"chunk":{"type":"text-delta","index":0,"text":"partial"}}),
    ),
    usage(1, 1, json!({"inputTokens":1,"outputTokens":2})),
    invalid.clone(),
  ]);
  assert_eq!(events.len(), 3);
  assert_eq!(events[0]["type"], "message");
  assert_eq!(events[0]["phase"], "delta");
  assert_eq!(events[1]["type"], "usage");
  assert_eq!(events[1]["input_tokens"], 1);
  assert_eq!(events[2]["type"], "unknown");
  assert_eq!(events[2]["native"], invalid);
}

#[test]
fn future_chunks_remain_visible_even_when_an_assembled_message_exists() {
  let future = record(
    "assistant/chunk",
    0,
    json!({"turn":1,"step":1,"chunk":{"type":"future","value":42}}),
  );
  let events = normalize(vec![future.clone(), assistant(1, 1, None)]);
  assert_eq!(events.len(), 2);
  assert_eq!(events[0]["type"], "unknown");
  assert_eq!(events[0]["native"], future);
  assert_eq!(events[1]["type"], "message");
}
