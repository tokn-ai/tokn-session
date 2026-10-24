use std::collections::BTreeSet;

use rusqlite::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenCodeCapabilities {
  pub(crate) has_v1: bool,
  pub(crate) has_v2: bool,
  pub(crate) has_session_entry: bool,
  pub(crate) has_session_agent: bool,
  pub(crate) has_session_model: bool,
  pub(crate) has_session_title: bool,
  pub(crate) has_session_workspace_id: bool,
}

impl OpenCodeCapabilities {
  pub(crate) fn detect(connection: &Connection) -> Result<Self, String> {
    let session = table_columns(connection, "session")?;
    let message = table_columns(connection, "message")?;
    let part = table_columns(connection, "part")?;
    let session_entry = table_columns(connection, "session_entry")?;
    let session_message = table_columns(connection, "session_message")?;

    let mut missing = Vec::new();
    require_columns(
      &mut missing,
      "session",
      &session,
      &["id", "parent_id", "directory", "time_created", "time_updated"],
    );
    if !session_entry.is_empty() {
      require_columns(
        &mut missing,
        "session_entry",
        &session_entry,
        &["id", "session_id", "type", "time_created", "data"],
      );
    }
    if !message.is_empty() || !part.is_empty() || session_message.is_empty() {
      require_columns(
        &mut missing,
        "message",
        &message,
        &["id", "session_id", "time_created", "data"],
      );
      require_columns(
        &mut missing,
        "part",
        &part,
        &["id", "message_id", "session_id", "time_created", "data"],
      );
    }
    if !session_message.is_empty() {
      require_columns(
        &mut missing,
        "session_message",
        &session_message,
        &["id", "session_id", "type", "seq", "time_created", "data"],
      );
    }
    if !missing.is_empty() {
      return Err(format!(
        "unsupported session database schema: missing {}",
        missing.join(", ")
      ));
    }

    Ok(Self {
      has_v1: !message.is_empty() && !part.is_empty(),
      has_v2: !session_message.is_empty(),
      has_session_entry: !session_entry.is_empty(),
      has_session_agent: session.contains("agent"),
      has_session_model: session.contains("model"),
      has_session_title: session.contains("title"),
      has_session_workspace_id: session.contains("workspace_id"),
    })
  }

  pub(crate) fn session_projection(self) -> &'static str {
    match (self.has_session_title, self.has_session_model) {
      (true, true) => "id, parent_id, directory, title, model, time_created, time_updated",
      (true, false) => "id, parent_id, directory, title, null as model, time_created, time_updated",
      (false, true) => "id, parent_id, directory, null as title, model, time_created, time_updated",
      (false, false) => "id, parent_id, directory, null as title, null as model, time_created, time_updated",
    }
  }

  pub(crate) fn session_catalog_projection(self) -> &'static str {
    if self.has_session_title {
      "id, parent_id, directory, title, time_created, time_updated"
    } else {
      "id, parent_id, directory, null as title, time_created, time_updated"
    }
  }
}

fn table_columns(connection: &Connection, table: &str) -> Result<BTreeSet<String>, String> {
  let pragma = match table {
    "session" => "pragma table_info(session)",
    "message" => "pragma table_info(message)",
    "part" => "pragma table_info(part)",
    "session_entry" => "pragma table_info(session_entry)",
    "session_message" => "pragma table_info(session_message)",
    _ => return Err(format!("unsupported session schema table `{table}`")),
  };
  let mut statement = connection
    .prepare(pragma)
    .map_err(|err| format!("failed to inspect session `{table}` schema: {err}"))?;
  let rows = statement
    .query_map([], |row| row.get::<_, String>(1))
    .map_err(|err| format!("failed to read session `{table}` schema: {err}"))?;

  rows
    .map(|row| row.map_err(|err| format!("failed to read session `{table}` schema: {err}")))
    .collect()
}

fn require_columns(missing: &mut Vec<String>, table: &str, columns: &BTreeSet<String>, required: &[&str]) {
  if columns.is_empty() {
    missing.push(format!("table `{table}`"));
    return;
  }
  for column in required {
    if !columns.contains(*column) {
      missing.push(format!("`{table}.{column}`"));
    }
  }
}

#[cfg(test)]
mod tests {
  use rusqlite::Connection;

  use super::OpenCodeCapabilities;

  fn base_schema(connection: &Connection) {
    connection
      .execute_batch(
        "create table session (
           id text primary key,
           parent_id text,
           directory text not null,
           time_created integer not null,
           time_updated integer not null
         );
         create table message (
           id text primary key,
           session_id text not null,
           time_created integer,
           data text not null
         );
         create table part (
           id text primary key,
           message_id text not null,
           session_id text not null,
           time_created integer,
           data text not null
         );",
      )
      .expect("base schema should be created");
  }

  #[test]
  fn detects_optional_session_capabilities() {
    let connection = Connection::open_in_memory().expect("database should open");
    base_schema(&connection);

    let capabilities = OpenCodeCapabilities::detect(&connection).expect("base schema should be supported");
    assert!(!capabilities.has_session_entry);
    assert!(!capabilities.has_session_agent);
    assert!(!capabilities.has_session_model);
    assert!(!capabilities.has_session_title);
    assert!(!capabilities.has_session_workspace_id);
    assert!(capabilities.session_projection().contains("null as model"));
    assert!(capabilities.session_projection().contains("null as title"));
    assert!(capabilities.session_catalog_projection().contains("null as title"));

    connection
      .execute_batch(
        "alter table session add column agent text;
         alter table session add column model text;
         alter table session add column title text;
         alter table session add column workspace_id text;",
      )
      .expect("optional columns should be added");
    let capabilities = OpenCodeCapabilities::detect(&connection).expect("extended schema should be supported");
    assert!(capabilities.has_session_agent);
    assert!(capabilities.has_session_model);
    assert!(capabilities.has_session_title);
    assert!(capabilities.has_session_workspace_id);
    assert!(capabilities.session_projection().contains(", model,"));
    assert!(capabilities.session_projection().contains(", title,"));
    assert!(capabilities.session_catalog_projection().contains(", title,"));

    connection
      .execute_batch(
        "create table session_entry (
           id text primary key,
           session_id text not null,
           type text not null,
           time_created integer not null,
           data text not null
         );",
      )
      .expect("session entry table should be created");
    let capabilities = OpenCodeCapabilities::detect(&connection).expect("runtime entries should be supported");
    assert!(capabilities.has_session_entry);
  }

  #[test]
  fn rejects_missing_required_tables_and_columns() {
    let connection = Connection::open_in_memory().expect("database should open");
    connection
      .execute_batch(
        "create table session (
           id text primary key
         );",
      )
      .expect("partial schema should be created");

    let error = OpenCodeCapabilities::detect(&connection).expect_err("partial schema should be rejected");
    assert!(error.contains("`session.parent_id`"));
    assert!(error.contains("table `message`"));
    assert!(error.contains("table `part`"));
  }
}
