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

fn event_provider(event: &AgentEvent) -> Provider {
  match event {
    AgentEvent::SessionStarted(event) => event.provider,
    AgentEvent::ProviderChanged(event) => event.provider,
    AgentEvent::SessionSettingsApplied(event) => event.provider,
    AgentEvent::Message(event) => event.provider,
    AgentEvent::QuestionRequest(event) => event.provider,
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
