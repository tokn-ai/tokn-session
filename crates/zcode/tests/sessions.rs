use rusqlite::Connection;
use tokn_session_core::{AgentEvent, Provider, ToolKind};
use tokn_session_zcode::ZCodeSessionSource;

fn fixture_database() -> (tempfile::TempDir, std::path::PathBuf) {
  let directory = tempfile::tempdir().expect("fixture directory should be created");
  let database = directory.path().join("db.sqlite");
  let connection = Connection::open(&database).expect("fixture database should open");
  connection
    .execute_batch(include_str!("../fixtures/session.sql"))
    .expect("fixture schema should load");
  drop(connection);
  (directory, database)
}

#[test]
fn observed_zcode_catalog_distinguishes_empty_from_missing_database() {
  let (_directory, database) = fixture_database();
  let source = ZCodeSessionSource::new(Some(database.clone()));
  let observed = source.list_session_header_catalog().unwrap();
  assert!(observed.complete);
  assert_eq!(observed.headers, source.list_session_headers().unwrap());
  let connection = Connection::open(&database).unwrap();
  connection.execute("delete from session", []).unwrap();
  assert!(source.list_session_header_catalog().unwrap().complete);
  assert!(source.list_session_header_catalog().unwrap().headers.is_empty());
  drop(connection);
  std::fs::remove_file(database).unwrap();
  assert!(!source.list_session_header_catalog().unwrap().complete);
}

#[test]
fn observed_zcode_catalog_retains_existing_database_errors() {
  let (_directory, database) = fixture_database();
  let source = ZCodeSessionSource::new(Some(database.clone()));
  let connection = Connection::open(&database).unwrap();
  connection.execute("drop table session", []).unwrap();
  assert_eq!(
    source.list_session_header_catalog().unwrap_err(),
    source.list_session_headers().unwrap_err()
  );
}

#[test]
fn lists_relationships_and_normalizes_zcode_events() {
  let (_directory, database) = fixture_database();
  let source = ZCodeSessionSource::new(Some(database.clone()));

  let sessions = source.list_sessions().expect("fixture sessions should list");
  assert_eq!(sessions.len(), 2);
  let child = sessions
    .iter()
    .find(|session| session.id == "sess_child")
    .expect("child session should exist");
  assert_eq!(child.parent_session_id.as_deref(), Some("sess_zcode"));
  assert_eq!(child.path, database);

  let loaded = source
    .load_session("sess_z")
    .expect("unique session prefix should load");
  assert_eq!(loaded.reference.id, "sess_zcode");
  assert_eq!(loaded.reference.title.as_deref(), Some("ZCode fixture"));
  assert_eq!(loaded.reference.message_count, 3);
  assert!(
    loaded
      .events
      .iter()
      .all(|event| event_provider(event) == Provider::ZCode)
  );

  assert!(loaded.events.iter().any(|event| {
    matches!(event, AgentEvent::Message(message) if message.text == "inspect this project" && !event.is_hidden())
  }));
  assert!(loaded.events.iter().any(|event| {
    matches!(event, AgentEvent::Message(message) if message.text == "model-only reminder" && event.is_hidden())
  }));
  assert!(loaded.events.iter().any(|event| {
    matches!(event, AgentEvent::Reasoning(reasoning) if reasoning.signature.as_deref() == Some("sig_fixture"))
  }));
  assert!(loaded.events.iter().any(|event| {
    matches!(event, AgentEvent::ToolCall(tool)
      if tool.tool_name.as_deref() == Some("Bash") && tool.tool_kind == ToolKind::Shell)
  }));
  assert!(loaded.events.iter().any(|event| {
    matches!(event, AgentEvent::Usage(usage)
      if usage.input_tokens == 15 && usage.output_tokens == 6 && usage.total_tokens == Some(23))
  }));
  assert!(loaded.events.iter().any(|event| {
    matches!(event, AgentEvent::ProviderChanged(change)
      if change.thinking_level.as_deref() == Some("high") && change.native_id.as_deref() == Some("entry_model"))
  }));
  assert!(loaded.events.iter().any(|event| {
    matches!(event, AgentEvent::Unknown(unknown)
      if unknown.native_type.as_deref() == Some("runtime/future_entry")
        && unknown.native.as_ref().and_then(|native| native.pointer("/data/answer")) == Some(&serde_json::json!(42)))
  }));
}

#[test]
fn resolves_an_explicit_zcode_storage_root() {
  let directory = tempfile::tempdir().expect("fixture directory should be created");
  let database_dir = directory.path().join("cli/db");
  std::fs::create_dir_all(&database_dir).expect("database directory should be created");
  let database = database_dir.join("db.sqlite");
  Connection::open(&database).expect("fixture database should open");

  let source = ZCodeSessionSource::new(Some(directory.path().to_path_buf()));
  assert_eq!(source.database_path().unwrap(), database);
}

#[test]
fn serializes_the_provider_as_zcode() {
  assert_eq!(serde_json::to_value(Provider::ZCode).unwrap(), "zcode");
}

#[test]
fn shared_error_normalization_preserves_zcode_identity_and_native_records() {
  let (_directory, database) = fixture_database();
  let connection = Connection::open(&database).unwrap();
  let records = [
    (
      "failed",
      serde_json::json!({"name":"AiSdkModelAdapterError", "data":{"message":"Connection failed", "code":"first"}}),
    ),
    (
      "failed-again",
      serde_json::json!({"name":"AiSdkModelAdapterError", "data":{"message":"Connection failed", "code":"second"}}),
    ),
    (
      "cancelled",
      serde_json::json!({"name":"MessageAbortedError", "data":{"message":"Stopped"}}),
    ),
  ];
  let mut expected = Vec::new();
  for (offset, (id, error)) in records.into_iter().enumerate() {
    let data = serde_json::json!({"role":"assistant", "parentID":format!("parent-{id}"), "error":error,
      "semantics":{"kind":"assistant_response", "transcriptVisibility":"visible"}});
    connection
      .execute(
        "INSERT INTO message VALUES(?1, 'sess_zcode', ?2, ?3, ?4)",
        rusqlite::params![id, 2000 + offset as i64, data.to_string(), 4 + offset as i64],
      )
      .unwrap();
    expected.push(serde_json::json!({"id":id, "data":data}));
  }
  drop(connection);
  let loaded = ZCodeSessionSource::new(Some(database))
    .load_session_exact("sess_zcode")
    .unwrap();
  let errors: Vec<_> = loaded
    .events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::Error(error) => Some(error),
      _ => None,
    })
    .collect();
  assert_eq!(errors.len(), 2);
  for (error, native) in errors.iter().zip(&expected) {
    assert_eq!(error.provider, Provider::ZCode);
    assert_eq!(error.message, "Connection failed");
    assert_eq!(error.native.as_ref(), Some(native));
  }
  let diagnostics: Vec<_> = loaded
    .events
    .iter()
    .filter_map(|event| match event {
      AgentEvent::Metadata(diagnostic) if diagnostic.native_type == "message.error" => Some(diagnostic),
      _ => None,
    })
    .collect();
  assert_eq!(diagnostics.len(), 3);
  for (diagnostic, native) in diagnostics.iter().zip(expected) {
    assert_eq!(diagnostic.provider, Provider::ZCode);
    assert_eq!(diagnostic.session_id.as_deref(), Some("sess_zcode"));
    assert_eq!(diagnostic.native, native);
  }
  assert_eq!(diagnostics[2].summary, "Provider response interrupted");
}

fn event_provider(event: &AgentEvent) -> Provider {
  match event {
    AgentEvent::SessionStarted(event) => event.provider,
    AgentEvent::ProviderChanged(event) => event.provider,
    AgentEvent::SessionSettingsApplied(event) => event.provider,
    AgentEvent::Message(event) => event.provider,
    AgentEvent::QuestionRequest(event) => event.provider,
    AgentEvent::QuestionReply(event) => event.provider,
    AgentEvent::Reasoning(event) => event.provider,
    AgentEvent::GoalUpdated(event) => event.provider,
    AgentEvent::AgentActivity(event) => event.provider,
    AgentEvent::ToolCall(event) => event.provider,
    AgentEvent::Lifecycle(event) => event.provider,
    AgentEvent::Compaction(event) => event.provider,
    AgentEvent::Usage(event) => event.provider,
    AgentEvent::Metadata(event) => event.provider,
    AgentEvent::Error(event) => event.provider,
    AgentEvent::Unknown(event) => event.provider,
  }
}
