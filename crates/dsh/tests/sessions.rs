use std::fs;
use std::path::PathBuf;

use serde_json::json;
use tokn_session_core::{AgentEvent, MessageDelivery, Phase, SessionHeader, SessionHistoryStatus};
use tokn_session_dsh::DshSessionSource;

const FIXTURE: &str = include_str!("../fixtures/basic/session.jsonl");

fn fixture_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

#[test]
fn discovers_and_normalizes_historical_session_without_duplicate_streams() {
  let source = DshSessionSource::new(Some(fixture_dir()));
  let references = source.list_sessions().unwrap();
  assert_eq!(references.len(), 1);
  assert_eq!(references[0].id, "dsh-fixture");
  assert_eq!(references[0].message_count, 4);
  assert_eq!(references[0].cwd.as_deref(), Some("/project/demo"));
  assert_eq!(references[0].title.as_deref(), Some("Reading a guide"));
  assert_eq!(references[0].preview.as_deref(), Some("Read the guide."));
  let loaded = source.load_session("dsh-fixt").unwrap();
  assert_eq!(loaded.history_status, SessionHistoryStatus::Complete);
  assert_eq!(loaded.reference.title.as_deref(), Some("Reading a guide"));
  assert_eq!(loaded.reference.preview.as_deref(), Some("Read the guide."));
  let messages: Vec<_> = loaded
    .events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::Message(message) => Some(message),
      _ => None,
    })
    .collect();
  assert_eq!(
    messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(),
    ["Read the guide.", "All done.", "Continue.", "Still", " working", "..."]
  );
  assert!(matches!(messages[1].delivery, MessageDelivery::Final));
  assert!(matches!(messages[3].phase, Phase::Delta));
  assert_eq!(messages[5].timestamp.as_deref(), Some("1025"));
  let tools: Vec<_> = loaded
    .events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::ToolCall(tool) => Some(tool),
      _ => None,
    })
    .collect();
  assert_eq!(tools.len(), 2);
  assert!(matches!(tools[0].phase, Phase::Started));
  assert!(matches!(tools[1].phase, Phase::Finished));
  assert_eq!(tools[1].tool_name.as_deref(), Some("read_file"));
  assert_eq!(tools[1].is_error, Some(false));
  assert_eq!(tools[1].input.as_ref().unwrap()["path"], "guide.md");
  assert!(
    loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::ProviderChanged(value)
    if value.model_id.as_deref() == Some("deepseek-v4-flash") && value.thinking_level.as_deref() == Some("high")
      && value.model_origin == Some(tokn_session_core::ModelOrigin::Request)))
  );
  assert!(
    loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Unknown(value)
    if value.native_type.as_deref() == Some("plugin/future")))
  );
  assert!(
    loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Unknown(value)
    if value.native_type.as_deref() == Some("user/message")))
  );
  assert_eq!(
    loaded
      .events
      .iter()
      .filter(|event| matches!(event, AgentEvent::Reasoning(_)))
      .count(),
    1
  );
}

#[test]
fn reads_concatenated_zstd_frames_and_never_modifies_input() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.jsonl.zstd");
  // A frame boundary need not align with a JSONL line.
  let split = 19;
  let mut compressed = zstd::stream::encode_all(&FIXTURE.as_bytes()[..split], 1).unwrap();
  compressed.extend(zstd::stream::encode_all(&FIXTURE.as_bytes()[split..], 1).unwrap());
  fs::write(&path, &compressed).unwrap();
  let source = DshSessionSource::new(Some(dir.path().into()));
  let relation = source.list_session_relations().unwrap().pop().unwrap();
  let hydrated = source
    .hydrate_session_header(SessionHeader {
      id: relation.id,
      parent_session_id: relation.parent_session_id,
      agent_path: relation.agent_path,
      agent_nickname: relation.agent_nickname,
      agent_role: relation.agent_role,
      title: relation.title,
      preview: relation.preview,
      path: relation.path,
      cwd: relation.cwd,
      timestamp: relation.timestamp,
      updated_at: None,
      updated_at_ms: None,
    })
    .unwrap();
  assert_eq!(hydrated.title.as_deref(), Some("Reading a guide"));
  assert_eq!(hydrated.preview.as_deref(), Some("Read the guide."));
  let compressed_session = source.load_session("dsh-fixture").unwrap();
  let plain_session = DshSessionSource::new(Some(fixture_dir()))
    .load_session("dsh-fixture")
    .unwrap();
  assert_eq!(
    serde_json::to_value(compressed_session.events).unwrap(),
    serde_json::to_value(plain_session.events).unwrap()
  );
  assert_eq!(fs::read(path).unwrap(), compressed);
}

#[test]
fn rejects_corruption_and_supports_explicit_files() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.jsonl");
  fs::write(&path, FIXTURE).unwrap();
  let source = DshSessionSource::new(Some(dir.path().into()));
  assert_eq!(source.load_session_path(&path).unwrap().reference.id, "dsh-fixture");
  fs::write(&path, FIXTURE.replace("\"version\":0", "\"version\":42")).unwrap();
  assert!(
    source
      .load_session_path(&path)
      .unwrap_err()
      .contains("unsupported dsh session version 42")
  );
  fs::write(&path, FIXTURE.replace("\"dt\":[1,-2]", "\"dt\":[]")).unwrap();
  assert!(
    source
      .load_session_path(&path)
      .unwrap_err()
      .contains("malformed text-chunks")
  );
  fs::write(&path, format!("{FIXTURE}{{broken\n")).unwrap();
  assert!(
    source
      .load_session_path(&path)
      .unwrap_err()
      .contains("invalid dsh JSON")
  );
  let zipped = dir.path().join("session.jsonl.zstd");
  let mut compressed = zstd::stream::encode_all(FIXTURE.as_bytes(), 1).unwrap();
  compressed.truncate(compressed.len() - 5);
  fs::write(&zipped, compressed).unwrap();
  assert!(
    source
      .load_session_path(&zipped)
      .unwrap_err()
      .contains("failed to read")
  );
  let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
  encoder.include_checksum(true).unwrap();
  std::io::Write::write_all(&mut encoder, FIXTURE.as_bytes()).unwrap();
  let mut checksummed = encoder.finish().unwrap();
  *checksummed.last_mut().unwrap() ^= 1;
  fs::write(&zipped, checksummed).unwrap();
  assert!(
    source
      .load_session_path(&zipped)
      .unwrap_err()
      .contains("failed to read")
  );
}

#[test]
fn distinguishes_subagent_seed_from_fork_and_resume_history() {
  let dir = tempfile::tempdir().unwrap();
  for (name, origin) in [("child", Some("subagent")), ("fork", None)] {
    let folder = dir.path().join(name);
    fs::create_dir_all(&folder).unwrap();
    let mut header = json!({"type":"session","version":0,"id":name,"createdAt":100,"delegationDepth":1,"parentSession":"root","seedLength":2});
    if let Some(origin) = origin {
      header["origin"] = json!(origin);
    }
    let body = [
      json!({"type":"session/title","seq":0,"time":0,"data":{"title":"Inherited title","messageSeqs":[1],"source":{"kind":"fallback"}}}),
      json!({"type":"user/message","seq":1,"time":1,"data":{"id":"inherited","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"parent history"}]}}),
      json!({"type":"user/message","seq":2,"time":2,"data":{"id":"own","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"own history"}]}}),
      json!({"type":"session/end-seed","seq":3,"time":3,"data":{}}),
    ];
    fs::write(
      folder.join("session.jsonl"),
      format!("{header}\n{}\n", body.map(|value| value.to_string()).join("\n")),
    )
    .unwrap();
  }
  let source = DshSessionSource::new(Some(dir.path().into()));
  let child = source.load_session("child").unwrap();
  assert_eq!(child.reference.parent_session_id.as_deref(), Some("root"));
  assert_eq!(child.history_status, SessionHistoryStatus::FilteredSubagent);
  assert_eq!(child.reference.message_count, 1);
  assert_eq!(child.reference.title.as_deref(), Some("Inherited title"));
  assert_eq!(child.reference.preview.as_deref(), Some("own history"));
  assert!(
    child
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "own history"))
  );
  let fork = source.load_session("fork").unwrap();
  assert_eq!(fork.reference.parent_session_id, None);
  assert_eq!(fork.reference.message_count, 2);
  let relation = &source.list_session_relations().unwrap()[0];
  assert_eq!(relation.message_count, 0);
  assert_eq!(relation.title, None);
  assert_eq!(relation.preview, None);
}

#[test]
fn latest_valid_title_wins_and_non_human_messages_do_not_become_preview() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.jsonl");
  let records = [
    json!({"type":"session","version":0,"id":"titles","createdAt":0,"delegationDepth":0}),
    json!({"type":"session/title","seq":0,"time":0,"data":{"title":"Earlier","messageSeqs":[],"source":{"kind":"user"}}}),
    json!({"type":"user/message","seq":1,"time":1,"data":{"id":"tool","role":"user","source":{"kind":"tool"},"content":[{"type":"text","text":"tool output"}]}}),
    json!({"type":"user/message","seq":2,"time":2,"data":{"id":"empty","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"  "}]}}),
    json!({"type":"user/message","seq":3,"time":3,"data":{"id":"human","role":"user","source":{"kind":"user"},"content":[{"type":"image","attachment":{}},{"type":"text","text":"first"},{"type":"text","text":"prompt"}]}}),
    json!({"type":"session/title","seq":4,"time":4,"data":{"title":"Latest","messageSeqs":[3],"source":{"kind":"fallback"}}}),
    // A malformed later record is not allowed to erase the latest valid fold.
    json!({"type":"session/title","seq":5,"time":5,"data":{"title":"Malformed"}}),
  ];
  fs::write(
    &path,
    records
      .into_iter()
      .map(|record| record.to_string())
      .collect::<Vec<_>>()
      .join("\n"),
  )
  .unwrap();

  let reference = DshSessionSource::new(Some(dir.path().into()))
    .list_sessions()
    .unwrap()
    .remove(0);

  assert_eq!(reference.title.as_deref(), Some("Latest"));
  assert_eq!(reference.preview.as_deref(), Some("first\nprompt"));
}

#[test]
fn preserves_unknown_content_and_failure_without_treating_cancellation_as_error() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.jsonl");
  let header = FIXTURE.lines().next().unwrap();
  let records = [
    json!({"type":"user/message","seq":0,"time":1,"data":{"id":"image","role":"user","source":{"kind":"user"},"content":[{"type":"future-block","answer":42}]}}),
    json!({"type":"turn/end","seq":1,"time":2,"data":{"turn":1,"reason":{"kind":"aborted","reason":{"kind":"user"}}}}),
    json!({"type":"turn/end","seq":2,"time":3,"data":{"turn":2,"reason":{"kind":"error","error":{"message":"provider failed","code":"HTTP"}}}}),
  ];
  fs::write(
    &path,
    format!("{header}\n{}\n", records.map(|record| record.to_string()).join("\n")),
  )
  .unwrap();
  let events = DshSessionSource::new(None).load_session_path(&path).unwrap().events;
  assert_eq!(
    events
      .iter()
      .filter(|event| matches!(event, AgentEvent::Error(_)))
      .count(),
    1
  );
  assert!(events.iter().any(|event| matches!(event, AgentEvent::Unknown(event)
    if event.native.as_ref().is_some_and(|native| native["data"]["content"][0]["answer"] == 42))));
}

#[test]
fn missing_root_is_empty_and_prefixes_must_be_unambiguous() {
  let dir = tempfile::tempdir().unwrap();
  assert!(
    DshSessionSource::new(Some(dir.path().join("missing")))
      .list_sessions()
      .unwrap()
      .is_empty()
  );
  for id in ["same", "same-long"] {
    let folder = dir.path().join(id);
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("session.jsonl"), FIXTURE.replace("dsh-fixture", id)).unwrap();
  }
  let source = DshSessionSource::new(Some(dir.path().into()));
  assert_eq!(source.load_session("same").unwrap().reference.id, "same");
  assert!(
    source
      .load_session("sam")
      .unwrap_err()
      .contains("multiple dsh sessions")
  );
  assert!(source.load_session("missing").unwrap_err().contains("no dsh session"));
}

#[test]
fn relay_batches_match_history_and_retain_packed_native_in_compressed_logs() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session.jsonl.zstd");
  let midpoint = FIXTURE.find("{\"type\":\"tool/result\"").unwrap();
  let mut compressed = zstd::stream::encode_all(&FIXTURE.as_bytes()[..midpoint], 1).unwrap();
  compressed.extend(zstd::stream::encode_all(&FIXTURE.as_bytes()[midpoint..], 1).unwrap());
  fs::write(&path, compressed).unwrap();
  let source = DshSessionSource::new(Some(dir.path().into()));
  for native in [false, true] {
    let records = source.load_session_records_path(&path, native, 1_000_000).unwrap();
    assert!(records.records.iter().all(|r| r.native.is_some() == native));
    if native {
      assert!(
        records
          .records
          .iter()
          .any(|r| r.native.as_ref().unwrap()["type"] == "reasoning-chunks")
      );
    }
    let flattened: tokn_session_core::LoadedSession = records.into();
    let history = source.load_session_path(&path).unwrap();
    assert_eq!(
      serde_json::to_value(flattened).unwrap(),
      serde_json::to_value(history).unwrap()
    );
  }
  assert!(
    source
      .load_session_records_path(&path, false, 100)
      .unwrap_err()
      .contains("size limit")
  );
}
