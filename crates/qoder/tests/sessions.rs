use serde_json::{Value, json};
use std::{
  fs,
  path::{Path, PathBuf},
};
use tokn_session_core::{AgentEvent, Phase, RecordedImageSource, SessionHistoryStatus, ToolRecordKind};
use tokn_session_qoder::{QoderSessionSource, project_identifier, read_session_header_exact};

fn fixture() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/session-main.jsonl")
}
fn write_rows(path: &Path, rows: &[Value]) {
  fs::write(path, rows.iter().map(|row| format!("{row}\n")).collect::<String>()).unwrap();
}
fn message(id: &str, parent: Option<&str>, text: &str) -> Value {
  json!({"type":"user","uuid":id,"parentUuid":parent,"isSidechain":false,"cwd":"/workspace/demo","sessionId":"session-main","timestamp":"2026-11-20T10:00:00Z","message":{"role":"user","content":text}})
}
fn texts(source: &QoderSessionSource, path: &Path) -> Vec<String> {
  source
    .load_session_path(path)
    .unwrap()
    .events
    .into_iter()
    .filter_map(|event| {
      if let AgentEvent::Message(message) = event {
        Some(message.text)
      } else {
        None
      }
    })
    .collect()
}

#[test]
fn response_models_and_explicit_terminal_delivery_are_local_to_each_visible_assistant_row() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("session-main.jsonl");
  let mut rows = vec![message("user", None, "Begin")];
  let mut parent = "user".to_owned();
  for index in 1..=8 {
    let id = format!("assistant-{index}");
    let mut row = message(&id, Some(&parent), "");
    row["type"] = json!("assistant");
    row["message"] = json!({"role":"assistant","model":format!("backend-{index}"),
      "stop_reason":"end_turn","content":[{"type":"text","text":format!("answer {index}")}],
      "usage":{"input_tokens":1,"output_tokens":1,"service_tier":"priority","speed":"fast"}});
    match index {
      2 => {
        row["message"].as_object_mut().unwrap().remove("stop_reason");
      }
      3 => {
        row["message"]["content"].as_array_mut().unwrap().push(json!({
          "type":"tool_use","id":"tool","name":"Read","input":{}
        }));
      }
      4 => {
        row["isMeta"] = json!(true);
      }
      5 => {
        row["isApiErrorMessage"] = json!(true);
      }
      6 => {
        row["message"]["model"] = json!("invalid\nmodel");
      }
      7 => {
        row["message"].as_object_mut().unwrap().remove("model");
      }
      8 => {
        row["aborted"] = json!(true);
      }
      _ => {}
    }
    rows.push(row);
    parent = id;
  }
  write_rows(&path, &rows);
  let events = QoderSessionSource::new(None).load_session_path(&path).unwrap().events;
  let messages: Vec<_> = events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::Message(message) => Some(message),
      _ => None,
    })
    .collect();
  assert_eq!(messages.len(), 9);
  assert!(messages[0].response_metadata.is_none());
  for (message, model) in [
    (messages[1], "backend-1"),
    (messages[2], "backend-2"),
    (messages[3], "backend-3"),
  ] {
    let metadata = message.response_metadata.as_ref().unwrap();
    assert_eq!(metadata.fast_mode, None);
    let recorded = metadata.model.as_ref().unwrap();
    assert_eq!(recorded.origin, tokn_session_core::ModelOrigin::Response);
    assert_eq!(recorded.model.model_id, model);
    assert_eq!(recorded.model.model_provider, None);
  }
  assert!(messages[4..].iter().all(|message| message.response_metadata.is_none()));
  for index in [1, 6, 7] {
    assert_eq!(messages[index].delivery, tokn_session_core::MessageDelivery::Final);
  }
  for index in [0, 2, 3, 4, 5, 8] {
    assert_eq!(
      messages[index].delivery,
      tokn_session_core::MessageDelivery::Unspecified
    );
  }
  // Preserve the existing native response-model labels, including malformed
  // labels kept for inspection; the new metadata applies stricter validation.
  assert_eq!(
    events
      .iter()
      .filter(|event| matches!(event, AgentEvent::ProviderChanged(_)))
      .count(),
    7
  );
}

#[test]
fn discovers_and_normalizes_official_transcript_shapes_without_losing_unknown_records() {
  let source = QoderSessionSource::new(Some(fixture().parent().unwrap().to_path_buf()));
  let header = source.list_session_relations().unwrap().remove(0);
  assert_eq!(header.id, "session-main");
  assert_eq!(header.cwd.as_deref(), Some("/workspace/demo"));
  assert_eq!(header.message_count, 0);
  let loaded = source.load_session_exact("session-main").unwrap();
  assert_eq!(loaded.reference.title.as_deref(), Some("Fixture conversation"));
  assert_eq!(loaded.reference.message_count, 4);
  assert_eq!(loaded.reference.preview.as_deref(), Some("Read the fixture file."));
  assert!(loaded.events.iter().any(
    |event| matches!(event,AgentEvent::Reasoning(reasoning) if reasoning.text.as_deref()==Some("I will read the file."))
  ));
  let tools: Vec<_> = loaded
    .events
    .iter()
    .filter_map(|event| {
      if let AgentEvent::ToolCall(tool) = event {
        Some(tool)
      } else {
        None
      }
    })
    .collect();
  assert_eq!(tools.len(), 2);
  assert_eq!(tools[0].record_kind, ToolRecordKind::Invocation);
  assert_eq!(tools[1].record_kind, ToolRecordKind::Result);
  assert!(matches!(tools[1].phase, Phase::Finished));
  assert_eq!(tools[1].tool_name.as_deref(), Some("Read"));
  assert_eq!(tools[1].output, Some(json!("Fixture contents")));
  assert!(loaded.events.iter().any(
    |event| matches!(event,AgentEvent::Usage(usage) if usage.input_tokens==7 && usage.cache_read_tokens==Some(3))
  ));
  assert!(loaded.events.iter().any(
    |event| matches!(event,AgentEvent::Unknown(unknown) if unknown.native.as_ref().unwrap()["detail"]["kept"]==true)
  ));
  let records = source.load_session_records_path(&fixture(), true, 1024 * 1024).unwrap();
  assert_eq!(records.records[0].record_id, "session");
  assert_eq!(records.records[1].record_id, "row:1");
  assert_eq!(records.records[1].native.as_ref().unwrap()["uuid"], "user-1");
}

#[test]
fn follows_rewind_commits_and_later_implicit_branch_advancement() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session-main.jsonl");
  let source = QoderSessionSource::new(None);
  let mut rows = vec![
    message("one", None, "Original"),
    message("two", Some("one"), "Discarded"),
    json!({"type":"active-leaf","sessionId":"session-main","leafUuid":"one","explicit":true,"timestamp":1}),
  ];
  write_rows(&path, &rows);
  assert_eq!(texts(&source, &path), vec!["Original"]);
  rows.push(message("three", Some("one"), "New branch"));
  // A message appended before its durable branch marker does not override the rewind.
  write_rows(&path, &rows);
  assert_eq!(texts(&source, &path), vec!["Original"]);
  rows.push(json!({"type":"active-leaf","sessionId":"session-main","leafUuid":"three","explicit":false,"timestamp":2}));
  write_rows(&path, &rows);
  assert_eq!(texts(&source, &path), vec!["Original", "New branch"]);
  rows.push(json!({"type":"active-leaf","sessionId":"session-main","leafUuid":null,"explicit":true,"timestamp":3}));
  write_rows(&path, &rows);
  assert!(texts(&source, &path).is_empty());
}

#[test]
fn keeps_latest_message_revision_and_ignores_only_incomplete_final_lines() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session-main.jsonl");
  let source = QoderSessionSource::new(None);
  write_rows(
    &path,
    &[
      message("one", None, "Earlier fragment"),
      message("one", None, "Final message"),
    ],
  );
  assert_eq!(texts(&source, &path), vec!["Final message"]);
  use std::io::Write;
  let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
  file.write_all(b"{\"type\":\"user\"").unwrap();
  assert_eq!(texts(&source, &path), vec!["Final message"]);
  file.write_all(b"\n").unwrap();
  assert!(source.load_session_path(&path).is_err());
}

#[test]
fn separates_subagent_identity_from_native_root_and_filters_main_sidechains() {
  let dir = tempfile::tempdir().unwrap();
  let child = dir.path().join("session-main/subagents");
  fs::create_dir_all(&child).unwrap();
  let path = child.join("agent-worker.jsonl");
  let mut row = message("child-one", None, "Child work");
  row["isSidechain"] = json!(true);
  row["agentId"] = json!("worker");
  write_rows(&path, &[row.clone()]);
  let source = QoderSessionSource::new(Some(dir.path().to_path_buf()));
  let loaded = source.load_session_path(&path).unwrap();
  assert_eq!(loaded.reference.id, "session-main/agent/worker");
  assert_eq!(loaded.reference.parent_session_id.as_deref(), Some("session-main"));
  assert!(matches!(loaded.history_status, SessionHistoryStatus::FilteredSubagent));
  assert!(read_session_header_exact(&path, "session-main").is_err());
  let root = dir.path().join("session-main.jsonl");
  write_rows(&root, &[message("one", None, "Root work"), row]);
  assert_eq!(texts(&source, &root), vec!["Root work"]);
}

#[test]
fn exact_header_applies_relocation_without_decoding_large_message_rows() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session-main.jsonl");
  write_rows(
    &path,
    &[
      message("one", None, &"x".repeat(100_000)),
      json!({"type":"relocated","sessionId":"session-main","relocatedCwd":"/workspace/moved"}),
    ],
  );
  let header = read_session_header_exact(&path, "session-main").unwrap();
  assert_eq!(header.cwd.as_deref(), Some("/workspace/moved"));
  assert!(read_session_header_exact(&path, "another").is_err());
}

#[test]
fn preserves_embedded_images_and_unknown_content_blocks() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session-main.jsonl");
  let mut row = message("one", None, "");
  row["message"]["content"] = json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"fixture-image"}},{"type":"future-content","answer":42}]);
  write_rows(&path, &[row]);
  let loaded = QoderSessionSource::new(None).load_session_path(&path).unwrap();
  assert!(loaded.events.iter().any(|event|matches!(event,AgentEvent::Message(message) if matches!(&message.images[0].source,RecordedImageSource::Base64{data} if data=="fixture-image"))));
  assert!(
    loaded
      .events
      .iter()
      .any(|event| matches!(event,AgentEvent::Unknown(unknown) if unknown.native.as_ref().unwrap()["answer"]==42))
  );
}

#[test]
fn reports_partial_catalogs_and_snapshot_limits() {
  let dir = tempfile::tempdir().unwrap();
  let missing = QoderSessionSource::new(Some(dir.path().join("missing")));
  assert!(!missing.list_session_relation_catalog().unwrap().complete);
  fs::copy(fixture(), dir.path().join("session-main.jsonl")).unwrap();
  fs::write(dir.path().join("broken.jsonl"), "broken\n").unwrap();
  let source = QoderSessionSource::new(Some(dir.path().to_path_buf()));
  let catalog = source.list_session_relation_catalog().unwrap();
  assert!(!catalog.complete);
  assert_eq!(catalog.references.len(), 1);
  assert!(
    source
      .load_session_records_path(&fixture(), false, 10)
      .unwrap_err()
      .contains("size limit")
  );
}

#[test]
fn project_paths_follow_native_utf16_and_hash_rules() {
  assert_eq!(project_identifier("/workspace/demo"), "-workspace-demo");
  assert_eq!(project_identifier("/workspace/😀"), "-workspace---");
  let long = format!("/{}", "x".repeat(220));
  let encoded = project_identifier(&long);
  assert!(encoded.starts_with(&format!("-{}-", "x".repeat(199))));
  assert!(encoded.len() > 201);
}

#[test]
fn large_first_message_identity_and_unknown_rows_remain_readable() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session-main.jsonl");
  let rows = vec![
    message("one", None, &"x".repeat(3 * 1024 * 1024)),
    json!({"type":"system","sessionId":"session-main","futureDiagnostic":true}),
    json!({"type":"user","sessionId":"session-main","message":{"futureShape":true}}),
  ];
  write_rows(&path, &rows);
  assert_eq!(
    read_session_header_exact(&path, "session-main").unwrap().id,
    "session-main"
  );
  let loaded = QoderSessionSource::new(None).load_session_path(&path).unwrap();
  assert_eq!(
    loaded
      .events
      .iter()
      .filter(|event| matches!(event, AgentEvent::Unknown(_)))
      .count(),
    2
  );
}

#[test]
fn fallback_leaf_excludes_meta_queries_and_implicit_null_clears_history() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session-main.jsonl");
  let source = QoderSessionSource::new(None);
  let mut meta = message("meta", None, "Hidden side query");
  meta["isMeta"] = json!(true);
  let mut rows = vec![message("one", None, "Visible"), meta];
  write_rows(&path, &rows);
  assert_eq!(texts(&source, &path), vec!["Visible"]);
  rows.push(json!({"type":"active-leaf","sessionId":"session-main","leafUuid":null,"explicit":false,"timestamp":1}));
  write_rows(&path, &rows);
  assert!(texts(&source, &path).is_empty());
}

#[test]
fn discovery_ignores_non_transcript_project_journals_and_logs() {
  let dir = tempfile::tempdir().unwrap();
  let project = dir.path().join("project-key");
  fs::create_dir_all(project.join("logs")).unwrap();
  fs::create_dir_all(project.join("session-main/workflows")).unwrap();
  fs::copy(fixture(), project.join("session-main.jsonl")).unwrap();
  fs::write(project.join("logs/session-main.jsonl"), "{\"type\":\"diagnostic\"}\n").unwrap();
  fs::write(
    project.join("session-main/workflows/journal.jsonl"),
    "{\"type\":\"workflow\"}\n",
  )
  .unwrap();
  let catalog = QoderSessionSource::new(Some(dir.path().to_path_buf()))
    .list_session_relation_catalog()
    .unwrap();
  assert!(catalog.complete);
  assert_eq!(catalog.references.len(), 1);
}

#[test]
fn historical_native_working_directories_are_not_limited_to_the_host_os() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("session-main.jsonl");
  for cwd in [
    "/workspace/demo",
    r"C:\workspace\demo",
    r"D:/workspace/demo",
    r"\\fixture-server\share\demo",
  ] {
    let mut row = message("one", None, "Portable history");
    row["cwd"] = json!(cwd);
    write_rows(&path, &[row]);
    assert_eq!(
      read_session_header_exact(&path, "session-main").unwrap().cwd.as_deref(),
      Some(cwd)
    );
    assert_eq!(
      QoderSessionSource::new(None)
        .load_session_path(&path)
        .unwrap()
        .reference
        .cwd
        .as_deref(),
      Some(cwd)
    );
  }
  for cwd in ["relative/project", r"C:relative", r"\\missing-share"] {
    let mut row = message("one", None, "Invalid relative history");
    row["cwd"] = json!(cwd);
    write_rows(&path, &[row]);
    assert!(read_session_header_exact(&path, "session-main").is_err());
  }
}
