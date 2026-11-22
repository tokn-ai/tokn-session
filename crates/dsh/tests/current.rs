//! Synthetic native-format examples derived from the pinned upstream codecs.
use serde_json::{Value, json};
use tokn_session_core::{AgentEvent, MessageDelivery, Role, SessionHistoryStatus, UsageKind};
use tokn_session_dsh::DshSessionSource;

fn event(kind: &str, seq: u64, data: Value) -> Value {
  json!({"type":kind,"seq":seq,"time":1000+seq,"data":data})
}

fn message(id: &str, role: &str, text: &str) -> Value {
  json!({"id":id,"role":role,"source":{"kind":if role == "user" {"user"} else {"model"}},"content":[{"type":"text","text":text}]})
}

fn write(version: u64, seeded: bool, rows: Vec<Value>) -> (tempfile::TempDir, std::path::PathBuf) {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join(if version == 0 {
    "session.jsonl".into()
  } else {
    format!("session.v{version}.jsonl")
  });
  let mut header = json!({"type":"session","version":version,"id":"fixture","cwd":"/project/fixture","createdAt":1,"delegationDepth":0});
  if version >= 2 {
    header["isSeeded"] = json!(seeded);
  }
  if seeded {
    header["origin"] = json!("subagent");
    header["parentSession"] = json!("parent");
  }
  let mut lines = vec![header.to_string()];
  lines.extend(rows.into_iter().map(|row| row.to_string()));
  std::fs::write(&path, lines.join("\n") + "\n").unwrap();
  (dir, path)
}

#[test]
fn every_released_generation_keeps_conversation_and_exact_identity() {
  for version in 0..=4 {
    let (_dir, path) = write(
      version,
      false,
      vec![
        event("user/message", 0, message("user", "user", "question")),
        event(
          "assistant/message",
          1,
          json!({"turn":1,"step":1,"message":message("assistant","assistant","answer"),"stream":[]}),
        ),
      ],
    );
    let source = DshSessionSource::new(None);
    let loaded = source.load_session_path_with_id(&path, "fixture").unwrap();
    assert_eq!(
      loaded
        .events
        .iter()
        .filter_map(|event| if let AgentEvent::Message(event) = event {
          Some(event.text.as_str())
        } else {
          None
        })
        .collect::<Vec<_>>(),
      ["question", "answer"]
    );
    assert_eq!(source.session_header_exact(&path, "fixture").unwrap().id, "fixture");
    assert!(source.load_session_path_with_id(&path, "other").is_err());
  }
}

#[test]
fn current_seed_ownership_uses_last_tagged_cut_not_resume_boundary() {
  for version in 2..=4 {
    let (_dir, path) = write(
      version,
      true,
      vec![
        event("user/message", 0, message("ancestor", "user", "ancestor")),
        event("session/end-seed", 1, json!({"inherited":true})),
        event(
          "assistant/message",
          2,
          json!({"turn":1,"step":1,"message":message("parent","assistant","parent"),"stream":[]}),
        ),
        event("session/end-seed", 3, json!({"inherited":true})),
        event("user/message", 4, message("child", "user", "child task")),
        event("session/end-seed", 5, json!({})),
        event(
          "assistant/message",
          6,
          json!({"turn":2,"step":1,"message":message("child-answer","assistant","child answer"),"stream":[]}),
        ),
      ],
    );
    let loaded = DshSessionSource::new(None).load_session_path(&path).unwrap();
    assert_eq!(loaded.history_status, SessionHistoryStatus::FilteredSubagent);
    assert_eq!(loaded.reference.parent_session_id.as_deref(), Some("parent"));
    assert_eq!(loaded.reference.preview.as_deref(), Some("child task"));
    assert_eq!(
      loaded
        .events
        .iter()
        .filter_map(|event| if let AgentEvent::Message(event) = event {
          Some(event.text.as_str())
        } else {
          None
        })
        .collect::<Vec<_>>(),
      ["child task", "child answer"]
    );
  }
}

#[test]
fn current_role_stream_accounting_and_surface_metadata_remain_truthful() {
  let usage = json!({"inputTokens":2,"outputTokens":1,"cacheReadTokens":3});
  let mut system = event(
    "system/message",
    0,
    json!({"turn":1,"step":1,"message":message("system","system","instructions")}),
  );
  system["surfaceOp"] = json!("append");
  let mut developer = event(
    "developer/message",
    1,
    json!({"turn":1,"step":1,"headerSeq":0,"message":{"id":"developer","role":"developer","source":{"kind":"fixture"},"content":[{"type":"tool-addition","toolName":"inspect"},{"type":"text","text":"more instructions"}]}}),
  );
  developer["surfaceOp"] = json!("append");
  let mut replacement = event("user/message", 2, message("user", "user", "new surface"));
  replacement["sourceEventSeqs"] = json!([[0, 1]]);
  replacement["surfaceOp"] = json!({"op":"replace","startSeq":0,"endSeq":1});
  let (_dir, path) = write(
    4,
    false,
    vec![
      system,
      developer,
      replacement,
      event(
        "assistant/attempt",
        3,
        json!({"turn":1,"step":1,"stream":[{"type":"text-chunks","time0":10,"index":0,"dt":[2],"texts":["failed"," attempt"]},{"type":"chunk","time":13,"chunk":{"type":"usage","usage":usage}}]}),
      ),
      event(
        "assistant/message",
        4,
        json!({"turn":1,"step":1,"message":message("assistant","assistant","delivered prefix"),"stream":[],"interrupted":true,"usage":usage}),
      ),
      event(
        "tool/call",
        5,
        json!({"turn":1,"step":1,"callId":"call","name":"inspect","arguments":"{}"}),
      ),
      event(
        "tool/result",
        6,
        json!({"turn":1,"step":1,"message":{"id":"result","role":"tool","source":{"kind":"tool","callId":"call"},"toolCallId":"call","isError":true,"content":[{"type":"text","text":"denied"}]},"error":{"name":"Denied","code":"DENIED","reason":"fixture denial"}}),
      ),
      event("turn/end", 7, json!({"turn":1,"reason":{"kind":"forked"}})),
    ],
  );
  let loaded = DshSessionSource::new(None).load_session_path(&path).unwrap();
  let messages = loaded
    .events
    .iter()
    .filter_map(|event| {
      if let AgentEvent::Message(event) = event {
        Some(event)
      } else {
        None
      }
    })
    .collect::<Vec<_>>();
  assert_eq!(messages.len(), 4);
  assert_eq!(messages[0].role, Role::System);
  assert_eq!(messages[1].role, Role::System);
  assert_eq!(
    messages[2].provenance.as_ref().unwrap().source_event_seqs,
    Some(vec![0, 1])
  );
  assert_eq!(messages[3].delivery, MessageDelivery::Unspecified);
  assert_eq!(loaded.events.iter().filter(|event| matches!(event,AgentEvent::Usage(event) if event.kind == UsageKind::ModelCall && event.input_tokens == 5)).count(),2);
  let result = loaded
    .events
    .iter()
    .find_map(|event| {
      if let AgentEvent::ToolCall(event) = event {
        event.output.as_ref().map(|_| event)
      } else {
        None
      }
    })
    .unwrap();
  assert_eq!(result.tool_call_id.as_deref(), Some("call"));
  assert_eq!(result.is_error, Some(true));
  assert_eq!(
    result.native.as_ref().unwrap()["data"]["error"]["reason"],
    "fixture denial"
  );
  assert!(
    !loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Unknown(_)))
  );
}

#[test]
fn malformed_current_seed_sequence_and_ranges_fail_without_tail_repair() {
  for (seeded, rows) in [
    (true, vec![event("session/end-seed", 0, json!({}))]),
    (false, vec![event("session/end-seed", 0, json!({"inherited":true}))]),
    (false, vec![event("user/message", 1, message("user", "user", "gap"))]),
    (
      false,
      vec![json!({"type":"future/event","seq":0,"time":1,"data":{},"sourceEventSeqs":[[0,9_007_199_254_740_991_u64]]})],
    ),
  ] {
    let (_dir, path) = write(4, seeded, rows);
    let before = std::fs::read(&path).unwrap();
    assert!(DshSessionSource::new(None).load_session_path(&path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
  }
}

#[test]
fn canonical_generation_selection_ignores_retained_predecessors_and_never_falls_back() {
  let (dir, path) = write(
    4,
    false,
    vec![event("user/message", 0, message("current", "user", "current input"))],
  );
  let old = dir.path().join("session.jsonl");
  std::fs::write(&old, "not-json\n").unwrap();
  for ignored in ["session.v0.jsonl", "session.v04.jsonl", "session.v4.jsonl.tmp"] {
    std::fs::write(dir.path().join(ignored), "not-json\n").unwrap();
  }
  let source = DshSessionSource::new(Some(dir.path().to_owned()));
  let references = source.list_session_relations().unwrap();
  assert_eq!(references.len(), 1);
  assert_eq!(references[0].path, path);
  assert_eq!(
    source.load_session("fixture").unwrap().reference.preview.as_deref(),
    Some("current input")
  );
  let future = dir.path().join("session.v5.jsonl");
  std::fs::write(
    &future,
    "{\"type\":\"session\",\"version\":5,\"id\":\"fixture\",\"createdAt\":1,\"isSeeded\":false}\n",
  )
  .unwrap();
  assert!(
    source
      .list_session_relations()
      .unwrap_err()
      .contains("unsupported dsh session version 5")
  );
  assert!(!source.list_session_relation_catalog().unwrap().complete);
  std::fs::remove_file(future).unwrap();
  let contents = std::fs::read_to_string(&path).unwrap();
  std::fs::write(&path, contents.replace("\"version\":4", "\"version\":3")).unwrap();
  assert!(
    source
      .list_session_relations()
      .unwrap_err()
      .contains("filename generation")
  );
  std::fs::write(&path, contents).unwrap();
  std::fs::write(
    dir.path().join("session.v3.jsonl.zstd"),
    zstd::stream::encode_all(&b"unused"[..], 1).unwrap(),
  )
  .unwrap();
  assert!(
    source
      .list_session_relations()
      .unwrap_err()
      .contains("conflicting JSONL encodings")
  );
}

#[test]
fn malformed_known_role_and_interruption_values_remain_native_unknown_records() {
  for row in [
    event("user/message", 0, message("wrong", "assistant", "not a user message")),
    event(
      "system/message",
      0,
      json!({"turn":1,"step":1,"message":message("wrong","user","not system instructions")}),
    ),
    event(
      "developer/message",
      0,
      json!({"turn":1,"step":1,"message":message("wrong","assistant","not developer instructions")}),
    ),
    event(
      "assistant/message",
      0,
      json!({"turn":1,"step":1,"message":message("wrong","assistant","unsupported interruption flag"),"interrupted":false,"stream":[]}),
    ),
  ] {
    let (_directory, path) = write(4, false, vec![row.clone()]);
    let loaded = DshSessionSource::new(None).load_session_path(&path).unwrap();
    assert!(matches!(&loaded.events[1],AgentEvent::Unknown(event) if event.native.as_ref()==Some(&row)));
    assert_eq!(loaded.events.len(), 2);
  }
}
