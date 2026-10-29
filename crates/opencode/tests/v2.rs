use rusqlite::Connection;
use std::sync::Arc;
use tokn_session_core::{AgentEvent, Phase, Role};
use tokn_session_opencode::{
  CompactRecord, OpenCodeCompactCache, OpenCodeHistoryFormat, OpenCodeSessionCache, OpenCodeSessionSource,
};

fn fixture() -> (tempfile::TempDir, Connection, OpenCodeSessionSource) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("opencode.db");
  let connection = Connection::open(&path).unwrap();
  connection.execute_batch(include_str!("fixtures/v2.sql")).unwrap();
  let source = OpenCodeSessionSource::new(Some(path));
  (directory, connection, source)
}

#[test]
fn observed_catalog_preserves_valid_rows_and_distinguishes_empty_from_unreadable() {
  let (_directory, connection, source) = fixture();
  let complete = source.list_session_header_catalog().unwrap();
  assert!(complete.complete);
  assert_eq!(complete.headers, source.list_session_headers().unwrap());
  connection
    .execute("update session set time_created = 'invalid' where id = 'ses_v1'", [])
    .unwrap();
  let partial = source.list_session_header_catalog().unwrap();
  assert!(!partial.complete);
  assert_eq!(partial.headers.len(), complete.headers.len() - 1);
  assert!(!partial.headers.iter().any(|header| header.id == "ses_v1"));
  assert!(source.list_session_headers().is_err());
  connection.execute("delete from session", []).unwrap();
  let empty = source.list_session_header_catalog().unwrap();
  assert!(empty.complete);
  assert!(empty.headers.is_empty());
  connection.execute("drop table session", []).unwrap();
  assert_eq!(
    source.list_session_header_catalog().unwrap_err(),
    source.list_session_headers().unwrap_err()
  );
}

#[test]
fn observed_catalog_preserves_diagnostics_for_corrupt_existing_database() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("opencode.db");
  std::fs::write(&database, "not a SQLite database").unwrap();
  let source = OpenCodeSessionSource::new(Some(database));
  assert_eq!(
    source.list_session_header_catalog().unwrap_err(),
    source.list_session_headers().unwrap_err()
  );
}

#[test]
fn observed_catalog_retains_errors_while_executing_a_readable_schema() {
  let (_directory, connection, source) = fixture();
  connection
    .execute_batch(
      "drop table session;
     create view session as select 'broken' as id, null as parent_id, '/work' as directory,
       abs(-9223372036854775808) as time_created, 0 as time_updated;",
    )
    .unwrap();
  let error = source.list_session_header_catalog().unwrap_err();
  assert!(error.contains("integer overflow"));
  assert_eq!(error, source.list_session_headers().unwrap_err());
}

#[test]
fn per_session_format_distinguishes_empty_v1_v2_and_mixed_without_table_heuristics() {
  let (_directory, connection, source) = fixture();
  assert_eq!(
    source.history_format_exact("ses_empty").unwrap(),
    OpenCodeHistoryFormat::Empty
  );
  assert_eq!(
    source.history_format_exact("ses_v1").unwrap(),
    OpenCodeHistoryFormat::V1
  );
  assert_eq!(
    source.history_format_exact("ses_v2").unwrap(),
    OpenCodeHistoryFormat::V2
  );
  connection
    .execute(
      "insert into message values ('conflict', 'ses_v2', 99, '{\"role\":\"user\"}')",
      [],
    )
    .unwrap();
  assert_eq!(
    source.history_format_exact("ses_v2").unwrap(),
    OpenCodeHistoryFormat::Mixed
  );
  assert!(
    source
      .load_session_exact("ses_v2")
      .unwrap_err()
      .contains("both V1 and V2")
  );
  assert!(source.history_format_exact("ses_missing").is_err());
}

#[test]
fn empty_session_has_exact_local_identity_before_first_prompt_projection() {
  let (_directory, connection, source) = fixture();
  let header = source.session_header_exact("ses_empty").unwrap();
  assert_eq!(header.cwd.as_deref(), Some("/work/project"));
  assert_eq!(header.path, source.database_path().unwrap());
  assert!(header.title.is_none());
  let empty = source.load_session_exact("ses_empty").unwrap();
  assert!(matches!(&empty.events[..], [AgentEvent::SessionStarted(_)]));
  connection.execute("insert into session_message values ('msg_new','ses_empty','user',0,101,101,'{\"text\":\"First prompt\",\"time\":{\"created\":101}}')", []).unwrap();
  assert_eq!(
    source.history_format_exact("ses_empty").unwrap(),
    OpenCodeHistoryFormat::V2
  );
  let loaded = source.load_session_exact("ses_empty").unwrap();
  assert_eq!(loaded.reference.message_count, 1);
  assert!(
    loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "First prompt"))
  );
}

#[test]
fn v2_normalizes_native_sequence_content_images_models_and_accounting() {
  let (_directory, _connection, source) = fixture();
  let records = source.load_session_records_exact("ses_v2", true).unwrap();
  assert_eq!(records.reference.message_count, 3);
  assert_eq!(records.reference.preview.as_deref(), Some("Inspect this image"));
  assert_eq!(
    records
      .records
      .iter()
      .map(|record| record.record_id.as_str())
      .collect::<Vec<_>>(),
    vec!["session:ses_v2", "v2:msg_user", "v2:msg_answer", "v2:msg_model"]
  );
  let events = records
    .records
    .iter()
    .flat_map(|record| &record.events)
    .collect::<Vec<_>>();
  assert!(events.iter().any(|event| matches!(event, AgentEvent::Message(message) if matches!(message.role, Role::User) && message.images.len() == 1)));
  assert!(
    events
      .iter()
      .any(|event| matches!(event, AgentEvent::Reasoning(reasoning) if reasoning.text.as_deref() == Some("Thinking")))
  );
  assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolCall(tool) if tool.tool_name.as_deref() == Some("question") && tool.phase == Phase::Finished && tool.images.len() == 1 && tool.native.as_ref().unwrap()["state"]["content"][0]["text"] == "A")));
  assert!(events.iter().any(|event| matches!(event, AgentEvent::Message(message) if message.text == "Answer" && message.phase == Phase::Finished)));
  assert!(events.iter().any(|event| matches!(event, AgentEvent::Usage(usage) if usage.input_tokens == 14 && usage.output_tokens == 4 && usage.reasoning_tokens == Some(2))));
  assert!(events.iter().any(|event| matches!(event, AgentEvent::ProviderChanged(model) if model.native_id.as_deref() == Some("msg_model") && model.model_id.as_deref() == Some("latest"))));
  assert_eq!(
    records.records[2].native.as_ref().unwrap()["content"][1]["state"]["input"]["questions"][0]["question"],
    "Which?"
  );
  let counts = source.list_sessions().unwrap();
  assert_eq!(
    counts
      .iter()
      .find(|session| session.id == "ses_v2")
      .unwrap()
      .message_count,
    3
  );
  let header = source
    .list_session_headers()
    .unwrap()
    .into_iter()
    .find(|header| header.id == "ses_v2")
    .unwrap();
  assert_eq!(
    source.hydrate_session_header(header).unwrap().preview.as_deref(),
    Some("Inspect this image")
  );
  assert!(
    source
      .load_session_exact("ses_v1")
      .unwrap()
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "legacy prompt"))
  );
}

#[test]
fn cached_and_compact_reads_reconcile_timestamp_free_v2_edits_and_keep_sequence_order() {
  let (_directory, connection, source) = fixture();
  let mut cache = OpenCodeSessionCache::default();
  let first = source
    .load_session_records_cached_exact("ses_v2", true, &mut cache)
    .unwrap();
  let mut compact = OpenCodeCompactCache::default();
  source
    .load_session_records_compact_exact("ses_v2", true, &mut compact)
    .unwrap();
  let unchanged = source
    .load_session_records_compact_exact("ses_v2", true, &mut compact)
    .unwrap();
  assert!(
    unchanged
      .records
      .iter()
      .all(|record| matches!(record, CompactRecord::Reused(_)))
  );
  connection
    .execute(
      "update session_message set data = replace(data,'Answer','Changed') where id = 'msg_answer'",
      [],
    )
    .unwrap();
  let next = source
    .load_session_records_cached_exact("ses_v2", true, &mut cache)
    .unwrap();
  assert!(Arc::ptr_eq(&first.records[1], &next.records[1]));
  assert!(!Arc::ptr_eq(&first.records[2], &next.records[2]));
  assert!(
    next.records[2]
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "Changed"))
  );
  let changed = source
    .load_session_records_compact_exact("ses_v2", true, &mut compact)
    .unwrap();
  assert!(matches!(changed.records[2], CompactRecord::Changed(_)));
  assert_eq!(next.records[1].record_id, "v2:msg_user");
}

#[test]
fn v2_only_schema_is_readable_and_unknown_content_never_erases_valid_text() {
  let (_directory, connection, source) = fixture();
  connection.execute_batch("drop table message; drop table part; update session_message set data=json_set(data,'$.content[#]',json('{\"type\":\"future\",\"opaque\":true}')) where id='msg_answer';").unwrap();
  let loaded = source.load_session_exact("ses_v2").unwrap();
  assert!(loaded.events.iter().any(|event| matches!(event, AgentEvent::Unknown(unknown) if unknown.native.as_ref().is_some_and(|native| native["opaque"] == true))));
  assert!(
    loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "Answer"))
  );
}

#[test]
fn mutable_assistant_completion_errors_and_compaction_keep_their_native_meaning() {
  let (_directory, connection, source) = fixture();
  connection
    .execute(
      "update session_message set data = json_remove(data,'$.finish','$.time.completed') where id='msg_answer'",
      [],
    )
    .unwrap();
  let partial = source.load_session_exact("ses_v2").unwrap();
  assert!(partial.events.iter().any(
    |event| matches!(event, AgentEvent::Message(message) if message.text == "Answer" && message.phase == Phase::Delta)
  ));
  connection.execute("update session_message set data=json_set(data,'$.error',json('{\"type\":\"unknown\",\"message\":\"upstream failed\"}')) where id='msg_answer'", []).unwrap();
  connection.execute("insert into session_message values('msg_compact','ses_v2','compaction',3,122,122,'{\"reason\":\"manual\",\"summary\":\"Summarized prior context\",\"recent\":\"Kept context\",\"time\":{\"created\":122}}')", []).unwrap();
  let loaded = source.load_session_exact("ses_v2").unwrap();
  let text_position = loaded.events.iter().position(|event| matches!(event, AgentEvent::Message(message) if message.text == "Answer" && message.phase == Phase::Finished)).unwrap();
  let error_position = loaded.events.iter().position(|event| matches!(event, AgentEvent::Error(error) if error.message == "upstream failed" && error.native.as_ref().unwrap()["data"]["error"]["type"] == "unknown")).unwrap();
  assert!(text_position < error_position);
  assert!(loaded.events.iter().any(|event| matches!(event, AgentEvent::Compaction(compact) if compact.summary.as_deref() == Some("Summarized prior context"))));
  assert!(
    !loaded
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "Summarized prior context"))
  );
  connection
    .execute(
      "update session_message set data=json_set(data,'$.tokens',json('{\"input\":\"invalid\"}')) where id='msg_answer'",
      [],
    )
    .unwrap();
  let malformed_usage = source.load_session_exact("ses_v2").unwrap();
  assert!(
    malformed_usage
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "Answer"))
  );
  assert!(
    malformed_usage
      .events
      .iter()
      .any(|event| matches!(event, AgentEvent::Unknown(unknown) if unknown.native_type.as_deref() == Some("v2.usage")))
  );
}
