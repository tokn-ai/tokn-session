use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

mod cache;
pub use cache::{
  CachedSessionRecords, CompactRecord, CompactSessionRecords, OpenCodeCompactCache, OpenCodeSessionCache,
};

use crate::normalize::OpenCodeNormalizer;
use crate::row::{OpenCodeMessageRow, OpenCodePartRow, OpenCodeSessionEntryRow, OpenCodeSessionRow};
use crate::schema::OpenCodeCapabilities;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use serde_json::Value;
use tokn_opencode_protocol::v1::{MessageData, MessageItem, PartData, PartItem, SessionModel};
use tokn_session_core::{
  LoadedSession, LoadedSessionRecords, NormalizedRecord, Provider, SessionHeader, SessionHistoryStatus, SessionRef,
};

#[derive(Clone, Copy)]
enum SessionDatabaseFlavor {
  OpenCode,
  ZCode,
}

impl SessionDatabaseFlavor {
  fn name(self) -> &'static str {
    match self {
      Self::OpenCode => "opencode",
      Self::ZCode => "zcode",
    }
  }

  fn provider(self) -> Provider {
    match self {
      Self::OpenCode => Provider::OpenCode,
      Self::ZCode => Provider::ZCode,
    }
  }
}

pub struct OpenCodeSessionSource {
  session_dir: Option<PathBuf>,
  flavor: SessionDatabaseFlavor,
}

impl OpenCodeSessionSource {
  pub fn new(session_dir: Option<PathBuf>) -> Self {
    Self {
      session_dir,
      flavor: SessionDatabaseFlavor::OpenCode,
    }
  }

  /// Build the compatible SQLite reader for ZCode's extended session store.
  /// Public provider dispatch should use `tokn-session-zcode` instead.
  #[doc(hidden)]
  pub fn for_zcode(session_dir: Option<PathBuf>) -> Self {
    Self {
      session_dir,
      flavor: SessionDatabaseFlavor::ZCode,
    }
  }

  pub fn list_sessions(&self) -> Result<Vec<SessionRef>, String> {
    let (connection, capabilities) = self.connect()?;
    self.list_session_refs(&connection, capabilities, true)
  }

  /// Lists catalog metadata without counting or reading messages. Callers can
  /// hydrate selected untitled headers when they need a user-text preview.
  pub fn list_session_relations(&self) -> Result<Vec<SessionRef>, String> {
    let (connection, capabilities) = self.connect()?;
    self.list_session_refs(&connection, capabilities, false)
  }

  pub fn list_session_headers(&self) -> Result<Vec<SessionHeader>, String> {
    let (connection, capabilities) = self.connect()?;
    let database_path = self.database_path()?;
    list_session_catalog(&connection, capabilities, self.flavor.name())?
      .into_iter()
      .map(|row| {
        let updated_at_ms = row.time_updated.or(row.time_created);
        Ok(SessionHeader {
          id: row.id,
          parent_session_id: row.parent_id,
          agent_path: None,
          agent_nickname: None,
          agent_role: None,
          title: row.title,
          preview: row.preview,
          path: database_path.clone(),
          cwd: row.directory,
          timestamp: timestamp(row.time_created),
          updated_at: timestamp(updated_at_ms),
          updated_at_ms,
        })
      })
      .collect()
  }

  /// Populate presentation metadata that is not available from OpenCode's
  /// session catalog alone. The exact session row is refreshed first in case
  /// OpenCode generated its title after the catalog was listed; only a still
  /// untitled session needs to inspect message parts for a preview.
  pub fn hydrate_session_header(&self, mut header: SessionHeader) -> Result<SessionHeader, String> {
    let connection = connect_database(&header.path)?;
    let capabilities = OpenCodeCapabilities::detect(&connection)?;
    let session = load_session_row(&connection, capabilities, &header.id, self.flavor.name())?
      .ok_or_else(|| format!("no {} session found for `{}`", self.flavor.name(), header.id))?;
    header.title = native_title(session.title);
    header.preview = if header.title.is_none() {
      first_user_preview(&connection, &header.id, self.flavor.name())?
    } else {
      None
    };
    Ok(header)
  }

  fn list_session_refs(
    &self,
    connection: &Connection,
    capabilities: OpenCodeCapabilities,
    include_message_count: bool,
  ) -> Result<Vec<SessionRef>, String> {
    let database_path = self.database_path()?;
    let message_counts = include_message_count
      .then(|| message_counts(connection, self.flavor.name()))
      .transpose()?;
    let mut sessions = Vec::new();
    for row in list_session_catalog(connection, capabilities, self.flavor.name())? {
      let message_count = message_counts
        .as_ref()
        .and_then(|counts| counts.get(&row.id))
        .copied()
        .unwrap_or(0);
      sessions.push(SessionRef {
        id: row.id,
        parent_session_id: row.parent_id,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        title: row.title,
        preview: row.preview,
        path: database_path.clone(),
        cwd: row.directory,
        // Preserve legacy counted-list output. The metadata-only SessionHeader
        // API exposes creation and update time separately.
        timestamp: timestamp(row.time_updated.or(row.time_created)),
        message_count,
      });
    }
    Ok(sessions)
  }

  pub fn load_session(&self, id_or_path: &str) -> Result<LoadedSession, String> {
    let (database_path, session_id) = self.resolve_session(id_or_path)?;
    self.load_session_from_database(database_path, &session_id)
  }

  pub fn load_session_exact(&self, session_id: &str) -> Result<LoadedSession, String> {
    self.load_session_from_database(self.database_path()?, session_id)
  }

  fn load_session_from_database(&self, database_path: PathBuf, session_id: &str) -> Result<LoadedSession, String> {
    self
      .load_records_from_database(database_path, session_id, false)
      .map(Into::into)
  }

  /// Read one consistent session snapshot grouped by session/message/entry ID.
  /// Native messages contain the decoded row plus all of its ordered parts,
  /// not an individual SQLite change or a lossless dump of every SQL column.
  pub fn load_session_records_exact(
    &self,
    session_id: &str,
    include_native: bool,
  ) -> Result<LoadedSessionRecords, String> {
    self.load_records_from_database(self.database_path()?, session_id, include_native)
  }

  /// Reconcile source rows in one read transaction, reusing decoded records
  /// and normalization checkpoints when their inputs have not changed.
  pub fn load_session_records_cached_exact(
    &self,
    session_id: &str,
    include_native: bool,
    cache: &mut OpenCodeSessionCache,
  ) -> Result<CachedSessionRecords, String> {
    cache.load(self, self.database_path()?, session_id, include_native)
  }

  /// Compare raw row fingerprints and normalization checkpoints, returning
  /// previous-image row positions instead of keeping historical bodies in RAM.
  pub fn load_session_records_compact_exact(
    &self,
    session_id: &str,
    include_native: bool,
    cache: &mut OpenCodeCompactCache,
  ) -> Result<CompactSessionRecords, String> {
    cache.load(self, self.database_path()?, session_id, include_native)
  }

  fn load_records_from_database(
    &self,
    database_path: PathBuf,
    session_id: &str,
    include_native: bool,
  ) -> Result<LoadedSessionRecords, String> {
    let mut database = connect_database(&database_path)?;
    let connection = database
      .transaction()
      .map_err(|err| format!("failed to start session snapshot: {err}"))?;
    let capabilities = OpenCodeCapabilities::detect(&connection)?;
    let session = load_session_row(&connection, capabilities, session_id, self.flavor.name())?
      .ok_or_else(|| format!("no {} session found for `{session_id}`", self.flavor.name()))?;
    let title = native_title(session.title.clone());
    let preview = if title.is_none() {
      first_user_preview(&connection, &session.id, self.flavor.name())?
    } else {
      None
    };
    let reference = SessionRef {
      id: session.id.clone(),
      parent_session_id: session.parent_id.clone(),
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      title,
      preview,
      path: database_path,
      cwd: session.directory.clone(),
      timestamp: timestamp(session.time_updated.or(session.time_created)),
      message_count: message_count(&connection, &session.id, self.flavor.name())?,
    };

    let mut normalizer = match self.flavor {
      SessionDatabaseFlavor::OpenCode => OpenCodeNormalizer::new(session.id.clone()),
      SessionDatabaseFlavor::ZCode => OpenCodeNormalizer::with_provider(session.id.clone(), self.flavor.provider()),
    };
    let mut records = vec![NormalizedRecord {
      record_id: format!("session:{}", session.id),
      native: include_native
        .then(|| serde_json::to_value(&session))
        .transpose()
        .map_err(|err| err.to_string())?,
      events: normalizer.normalize_session(&session),
    }];
    let mut timeline: Vec<_> = load_messages(&connection, &session.id, self.flavor.name())?
      .into_iter()
      .map(SessionTimelineRow::Message)
      .collect();
    if matches!(self.flavor, SessionDatabaseFlavor::ZCode) && capabilities.has_session_entry {
      timeline.extend(
        load_session_entries(&connection, &session.id, self.flavor.name())?
          .into_iter()
          .map(SessionTimelineRow::Entry),
      );
    }
    timeline.sort_by(|left, right| {
      left
        .time_created()
        .cmp(&right.time_created())
        .then_with(|| left.id().cmp(right.id()))
    });
    for row in timeline {
      match row {
        SessionTimelineRow::Message(message) => records.push(NormalizedRecord {
          record_id: format!("message:{}", message.id),
          native: include_native
            .then(|| serde_json::to_value(&message))
            .transpose()
            .map_err(|err| err.to_string())?,
          events: normalizer.normalize_message(message),
        }),
        SessionTimelineRow::Entry(entry) => records.push(NormalizedRecord {
          record_id: format!("entry:{}", entry.id),
          native: include_native
            .then(|| serde_json::to_value(&entry))
            .transpose()
            .map_err(|err| err.to_string())?,
          events: vec![normalizer.normalize_session_entry(entry)],
        }),
      }
    }

    Ok(LoadedSessionRecords {
      reference,
      records,
      history_status: SessionHistoryStatus::Complete,
    })
  }

  fn resolve_session(&self, id_or_path: &str) -> Result<(PathBuf, String), String> {
    let candidate = PathBuf::from(id_or_path);
    if candidate.exists() {
      return Err(format!(
        "{} sessions are stored in sqlite; pass a session id and use --session-dir for the database",
        self.flavor.name()
      ));
    }

    let matches: Vec<_> = self
      .list_session_relations()?
      .into_iter()
      .filter(|session| session.id == id_or_path || session.id.starts_with(id_or_path))
      .collect();

    match matches.as_slice() {
      [session] => Ok((session.path.clone(), session.id.clone())),
      [] => Err(format!("no {} session found for `{id_or_path}`", self.flavor.name())),
      _ => Err(format!("multiple {} sessions match `{id_or_path}`", self.flavor.name())),
    }
  }

  fn connect(&self) -> Result<(Connection, OpenCodeCapabilities), String> {
    let connection = connect_database(&self.database_path()?)?;
    let capabilities = OpenCodeCapabilities::detect(&connection)?;
    Ok((connection, capabilities))
  }

  pub fn database_path(&self) -> Result<PathBuf, String> {
    match self.flavor {
      SessionDatabaseFlavor::OpenCode => resolve_database_path(
        self.session_dir.clone(),
        std::env::var_os("OPENCODE_DB"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
      ),
      SessionDatabaseFlavor::ZCode => resolve_zcode_database_path(
        self.session_dir.clone(),
        std::env::var_os("ZCODE_STORAGE_DIR"),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
      ),
    }
  }
}

enum SessionTimelineRow {
  Message(OpenCodeMessageRow),
  Entry(OpenCodeSessionEntryRow),
}

impl SessionTimelineRow {
  fn id(&self) -> &str {
    match self {
      Self::Message(row) => &row.id,
      Self::Entry(row) => &row.id,
    }
  }

  fn time_created(&self) -> Option<i64> {
    match self {
      Self::Message(row) => row.time_created,
      Self::Entry(row) => row.time_created,
    }
  }
}

struct SessionCatalogRow {
  id: String,
  parent_id: Option<String>,
  directory: Option<String>,
  title: Option<String>,
  preview: Option<String>,
  time_created: Option<i64>,
  time_updated: Option<i64>,
}

fn list_session_catalog(
  connection: &Connection,
  capabilities: OpenCodeCapabilities,
  source_name: &str,
) -> Result<Vec<SessionCatalogRow>, String> {
  let mut statement = connection
    .prepare(&format!(
      "select {}
       from session
       order by time_created desc, id desc",
      capabilities.session_catalog_projection()
    ))
    .map_err(|err| format!("failed to prepare {source_name} session query: {err}"))?;
  let rows = statement
    .query_map([], |row| {
      Ok(SessionCatalogRow {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        directory: row.get(2)?,
        title: native_title(row.get(3)?),
        preview: None,
        time_created: row.get(4)?,
        time_updated: row.get(5)?,
      })
    })
    .map_err(|err| format!("failed to query {source_name} sessions: {err}"))?;

  rows
    .map(|row| row.map_err(|err| format!("failed to read {source_name} session row: {err}")))
    .collect()
}

fn resolve_database_path(
  explicit: Option<PathBuf>,
  opencode_db: Option<OsString>,
  xdg_data_home: Option<OsString>,
  home: Option<OsString>,
  user_profile: Option<OsString>,
) -> Result<PathBuf, String> {
  if let Some(path) = explicit {
    return Ok(if path.is_dir() { path.join("opencode.db") } else { path });
  }

  let opencode_db = non_empty(opencode_db);
  if opencode_db.as_deref() == Some(std::ffi::OsStr::new(":memory:")) {
    return Err("OPENCODE_DB=:memory: has no persisted sessions to discover".to_string());
  }
  let opencode_db = opencode_db.map(PathBuf::from);
  if let Some(path) = opencode_db.as_ref().filter(|path| path.is_absolute()) {
    return Ok(path.clone());
  }

  let data_root = non_empty(xdg_data_home)
    .map(PathBuf::from)
    .or_else(|| {
      non_empty(home)
        .or_else(|| non_empty(user_profile))
        .map(|home| PathBuf::from(home).join(".local").join("share"))
    })
    .ok_or_else(|| "set XDG_DATA_HOME, HOME, USERPROFILE, or --session-dir to locate opencode sessions".to_string())?;
  let data_dir = data_root.join("opencode");

  match opencode_db {
    Some(path) => Ok(data_dir.join(path)),
    None => Ok(data_dir.join("opencode.db")),
  }
}

fn resolve_zcode_database_path(
  explicit: Option<PathBuf>,
  storage_dir: Option<OsString>,
  home: Option<OsString>,
  user_profile: Option<OsString>,
) -> Result<PathBuf, String> {
  if let Some(path) = explicit {
    if !path.is_dir() {
      return Ok(path);
    }
    let direct = path.join("db.sqlite");
    let nested = path.join("cli").join("db").join("db.sqlite");
    return Ok(if direct.exists() || !nested.exists() {
      direct
    } else {
      nested
    });
  }

  let storage_dir = non_empty(storage_dir).map(PathBuf::from).or_else(|| {
    non_empty(home)
      .or_else(|| non_empty(user_profile))
      .map(|home| PathBuf::from(home).join(".zcode"))
  });
  storage_dir
    .map(|root| root.join("cli").join("db").join("db.sqlite"))
    .ok_or_else(|| "set ZCODE_STORAGE_DIR, HOME, USERPROFILE, or --session-dir to locate zcode sessions".to_string())
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
  value.filter(|value| !value.is_empty())
}

fn connect_database(path: &Path) -> Result<Connection, String> {
  let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
  let uri = format!("file:{}?mode=ro", sqlite_uri_path(path));
  match Connection::open_with_flags(&uri, flags) {
    Ok(connection) => Ok(connection),
    Err(read_only_error) => {
      let immutable_uri = format!("file:{}?mode=ro&immutable=1", sqlite_uri_path(path));
      Connection::open_with_flags(&immutable_uri, flags).map_err(|immutable_error| {
        format!(
          "failed to open session database {} read-only ({read_only_error}); immutable fallback also failed ({immutable_error})",
          path.display()
        )
      })
    }
  }
}

fn sqlite_uri_path(path: &Path) -> String {
  path
    .to_string_lossy()
    .chars()
    .flat_map(|value| match value {
      ' ' => "%20".chars().collect::<Vec<_>>(),
      '#' => "%23".chars().collect::<Vec<_>>(),
      '?' => "%3f".chars().collect::<Vec<_>>(),
      '%' => "%25".chars().collect::<Vec<_>>(),
      value => vec![value],
    })
    .collect()
}

fn load_session_row(
  connection: &Connection,
  capabilities: OpenCodeCapabilities,
  session_id: &str,
  source_name: &str,
) -> Result<Option<OpenCodeSessionRow>, String> {
  connection
    .query_row(
      &format!(
        "select {} from session where id = ?1",
        capabilities.session_projection()
      ),
      params![session_id],
      read_session_row,
    )
    .optional()
    .map_err(|err| format!("failed to load {source_name} session `{session_id}`: {err}"))
}

fn read_session_row(row: &Row<'_>) -> rusqlite::Result<OpenCodeSessionRow> {
  let model: Option<String> = row.get(4)?;
  Ok(OpenCodeSessionRow {
    id: row.get(0)?,
    parent_id: row.get(1)?,
    directory: row.get(2)?,
    title: row.get(3)?,
    model: parse_optional_model(model),
    time_created: row.get(5)?,
    time_updated: row.get(6)?,
  })
}

fn first_user_preview(connection: &Connection, session_id: &str, source_name: &str) -> Result<Option<String>, String> {
  let mut statement = connection
    .prepare(
      "select message.data, part.data
       from message
       join part on part.message_id = message.id and part.session_id = message.session_id
       where message.session_id = ?1
       order by message.time_created asc, message.id asc, part.time_created asc, part.id asc",
    )
    .map_err(|err| format!("failed to prepare {source_name} preview query for `{session_id}`: {err}"))?;
  let rows = statement
    .query_map(params![session_id], |row| {
      Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .map_err(|err| format!("failed to query {source_name} preview for `{session_id}`: {err}"))?;

  for row in rows {
    let (message, part) =
      row.map_err(|err| format!("failed to read {source_name} preview row for `{session_id}`: {err}"))?;
    let Ok(message) = serde_json::from_str::<MessageData>(&message) else {
      continue;
    };
    if !matches!(message.item(), MessageItem::User(_)) {
      continue;
    }
    let Ok(part) = serde_json::from_str::<PartData>(&part) else {
      continue;
    };
    let preview = match part.item() {
      PartItem::Text(part) if part.synthetic != Some(true) && part.ignored != Some(true) => Some(part.text.as_str()),
      PartItem::Subtask(part) => part.prompt.as_deref(),
      _ => None,
    };
    if let Some(preview) = preview.and_then(non_blank) {
      return Ok(Some(preview.to_string()));
    }
  }

  Ok(None)
}

fn load_messages(
  connection: &Connection,
  session_id: &str,
  source_name: &str,
) -> Result<Vec<OpenCodeMessageRow>, String> {
  let mut statement = connection
    .prepare(
      "select id, time_created, data
       from message
       where session_id = ?1
       order by time_created asc, id asc",
    )
    .map_err(|err| format!("failed to prepare {source_name} message query: {err}"))?;
  let rows = statement
    .query_map(params![session_id], |row| {
      let data: String = row.get(2)?;
      Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?, data))
    })
    .map_err(|err| format!("failed to query {source_name} messages: {err}"))?;

  let mut messages = Vec::new();
  for row in rows {
    let (id, time_created, data) = row.map_err(|err| format!("failed to read {source_name} message row: {err}"))?;
    let data: MessageData =
      serde_json::from_str(&data).map_err(|err| format!("invalid {source_name} message `{id}`: {err}"))?;
    let parts = load_parts(connection, session_id, &id, source_name)?;
    messages.push(OpenCodeMessageRow {
      id,
      time_created,
      data,
      parts,
    });
  }
  Ok(messages)
}

fn load_parts(
  connection: &Connection,
  session_id: &str,
  message_id: &str,
  source_name: &str,
) -> Result<Vec<OpenCodePartRow>, String> {
  let mut statement = connection
    .prepare(
      "select id, time_created, data
       from part
       where session_id = ?1 and message_id = ?2
       order by time_created asc, id asc",
    )
    .map_err(|err| format!("failed to prepare {source_name} part query: {err}"))?;
  let rows = statement
    .query_map(params![session_id, message_id], |row| {
      let data: String = row.get(2)?;
      Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?, data))
    })
    .map_err(|err| format!("failed to query {source_name} parts: {err}"))?;

  let mut parts = Vec::new();
  for row in rows {
    let (id, time_created, data) = row.map_err(|err| format!("failed to read {source_name} part row: {err}"))?;
    let data: PartData =
      serde_json::from_str(&data).map_err(|err| format!("invalid {source_name} part `{id}`: {err}"))?;
    parts.push(OpenCodePartRow { id, time_created, data });
  }
  Ok(parts)
}

fn load_session_entries(
  connection: &Connection,
  session_id: &str,
  source_name: &str,
) -> Result<Vec<OpenCodeSessionEntryRow>, String> {
  let mut statement = connection
    .prepare(
      "select id, type, time_created, data
       from session_entry
       where session_id = ?1
       order by time_created asc, id asc",
    )
    .map_err(|err| format!("failed to prepare {source_name} session-entry query: {err}"))?;
  let rows = statement
    .query_map(params![session_id], |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, Option<i64>>(2)?,
        row.get::<_, String>(3)?,
      ))
    })
    .map_err(|err| format!("failed to query {source_name} session entries: {err}"))?;

  let mut entries = Vec::new();
  for row in rows {
    let (id, native_type, time_created, data) =
      row.map_err(|err| format!("failed to read {source_name} session-entry row: {err}"))?;
    let data = serde_json::from_str(&data).unwrap_or(Value::String(data));
    entries.push(OpenCodeSessionEntryRow {
      id,
      native_type,
      time_created,
      data,
    });
  }
  Ok(entries)
}

fn message_count(connection: &Connection, session_id: &str, source_name: &str) -> Result<usize, String> {
  connection
    .query_row(
      "select count(*) from message where session_id = ?1",
      params![session_id],
      |row| row.get::<_, i64>(0),
    )
    .map(|count| count as usize)
    .map_err(|err| format!("failed to count {source_name} messages for `{session_id}`: {err}"))
}

fn message_counts(connection: &Connection, source_name: &str) -> Result<HashMap<String, usize>, String> {
  let mut statement = connection
    .prepare("select session_id, count(*) from message group by session_id")
    .map_err(|err| format!("failed to prepare {source_name} message counts: {err}"))?;
  let rows = statement
    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
    .map_err(|err| format!("failed to query {source_name} message counts: {err}"))?;
  rows
    .map(|row| {
      let (session_id, count) = row.map_err(|err| format!("failed to read {source_name} message count: {err}"))?;
      let count = usize::try_from(count)
        .map_err(|err| format!("invalid {source_name} message count for `{session_id}`: {err}"))?;
      Ok((session_id, count))
    })
    .collect()
}

fn parse_optional_model(value: Option<String>) -> Option<SessionModel> {
  value.and_then(|value| serde_json::from_str(&value).ok())
}

fn native_title(title: Option<String>) -> Option<String> {
  title
    .as_deref()
    .and_then(non_blank)
    .filter(|title| !is_default_title(title))
    .map(str::to_string)
}

fn non_blank(value: &str) -> Option<&str> {
  let value = value.trim();
  (!value.is_empty()).then_some(value)
}

fn is_default_title(title: &str) -> bool {
  let Some(timestamp) = title
    .strip_prefix("New session - ")
    .or_else(|| title.strip_prefix("Child session - "))
  else {
    return false;
  };
  let bytes = timestamp.as_bytes();
  bytes.len() == 24
    && bytes.iter().enumerate().all(|(index, byte)| match index {
      4 | 7 => *byte == b'-',
      10 => *byte == b'T',
      13 | 16 => *byte == b':',
      19 => *byte == b'.',
      23 => *byte == b'Z',
      _ => byte.is_ascii_digit(),
    })
}

fn timestamp(value: Option<i64>) -> Option<String> {
  value.map(|value| value.to_string())
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use rusqlite::{Connection, params};
  use tempfile::tempdir;
  use tokn_opencode_protocol::v1::{MessageItem, PartItem};
  use tokn_session_core::AgentEvent;

  use crate::schema::OpenCodeCapabilities;

  use super::{OpenCodeSessionSource, load_messages, load_session_row, native_title, resolve_database_path};

  #[test]
  fn resolves_persisted_database_paths_with_opencode_precedence() {
    let directory = tempdir().unwrap();
    let explicit_directory = directory.path().join("explicit");
    std::fs::create_dir(&explicit_directory).unwrap();
    let explicit = resolve_database_path(
      Some(explicit_directory.clone()),
      Some(":memory:".into()),
      None,
      None,
      None,
    )
    .unwrap();
    assert_eq!(explicit, explicit_directory.join("opencode.db"));

    let absolute = directory.path().join("custom.db");
    assert_eq!(
      resolve_database_path(None, Some(absolute.clone().into_os_string()), None, None, None).unwrap(),
      absolute,
    );

    let xdg_root = PathBuf::from("xdg-data");
    assert_eq!(
      resolve_database_path(
        None,
        Some("custom.db".into()),
        Some(xdg_root.clone().into_os_string()),
        Some("ignored-home".into()),
        None,
      )
      .unwrap(),
      xdg_root.join("opencode/custom.db"),
    );
    assert_eq!(
      resolve_database_path(
        None,
        None,
        Some(xdg_root.clone().into_os_string()),
        Some("ignored-home".into()),
        None,
      )
      .unwrap(),
      xdg_root.join("opencode/opencode.db"),
    );

    assert_eq!(
      resolve_database_path(None, None, None, Some("home".into()), Some("profile".into())).unwrap(),
      PathBuf::from("home/.local/share/opencode/opencode.db"),
    );
    assert_eq!(
      resolve_database_path(None, None, None, None, Some("profile".into())).unwrap(),
      PathBuf::from("profile/.local/share/opencode/opencode.db"),
    );
  }

  #[test]
  fn rejects_in_memory_database_for_persisted_discovery() {
    let error = resolve_database_path(None, Some(":memory:".into()), Some("xdg-data".into()), None, None).unwrap_err();

    assert!(error.contains(":memory:"));
    assert!(error.contains("no persisted sessions"));
  }

  #[test]
  fn lists_message_counts_for_multiple_sessions_including_empty_ones() {
    let directory = tempdir().unwrap();
    let database_path = directory.path().join("opencode.db");
    let connection = Connection::open(&database_path).unwrap();
    connection
      .execute_batch(
        r#"create table session (
             id text primary key, parent_id text, directory text not null,
             time_created integer not null, time_updated integer not null
           );
           create table message (
             id text primary key, session_id text not null,
             time_created integer, data text not null
           );
           create table part (
             id text primary key, message_id text not null,
             session_id text not null, time_created integer, data text not null
           );
           insert into session values
             ('empty', null, '/tmp', 1, 1),
             ('one', null, '/tmp', 2, 2),
             ('two', null, '/tmp', 3, 3);
           insert into message values
             ('m1', 'one', 1, '{}'),
             ('m2', 'two', 2, '{}'),
             ('m3', 'two', 3, '{}');"#,
      )
      .unwrap();

    let source = OpenCodeSessionSource::new(Some(database_path));
    let counts: std::collections::HashMap<_, _> = source
      .list_sessions()
      .unwrap()
      .into_iter()
      .map(|session| (session.id, session.message_count))
      .collect();
    assert_eq!(counts.get("empty"), Some(&0));
    assert_eq!(counts.get("one"), Some(&1));
    assert_eq!(counts.get("two"), Some(&2));
  }

  #[test]
  fn lists_and_loads_database_without_model_column() {
    let directory = tempdir().expect("temporary directory should be created");
    let database_path = directory.path().join("opencode.db");
    let connection = Connection::open(&database_path).expect("database should open");
    connection
      .execute_batch(
        r#"pragma journal_mode = wal;
         create table session (
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
         );
         insert into session (
           id, parent_id, directory, time_created, time_updated
         ) values (
           'ses_without_model', null, '/tmp/without-model', 1, 2
         );
         insert into message (
           id, session_id, time_created, data
         ) values (
           'msg_user',
           'ses_without_model',
           1,
           '{"role":"user","model":{"providerID":"openai","modelID":"gpt-5"}}'
         );
         insert into part (
           id, message_id, session_id, time_created, data
         ) values (
           'prt_text',
           'msg_user',
           'ses_without_model',
           1,
           '{"type":"text","text":"hello"}'
         );"#,
      )
      .expect("fixture schema without model should be created");

    let source = OpenCodeSessionSource::new(Some(database_path));
    let sessions = source
      .list_sessions()
      .expect("schema without model should remain listable");
    let relations = source
      .list_session_relations()
      .expect("catalog metadata should remain listable without counts");
    let headers = source
      .list_session_headers()
      .expect("catalog headers should keep creation and update time separate");
    let session = source
      .load_session_exact("ses_without_model")
      .expect("schema without model should remain loadable");

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "ses_without_model");
    assert_eq!(sessions[0].message_count, 1);
    assert_eq!(relations.len(), 1);
    assert_eq!(relations[0].id, "ses_without_model");
    assert_eq!(relations[0].message_count, 0);
    assert_eq!(relations[0].title, None);
    assert_eq!(relations[0].preview, None);
    assert_eq!(headers[0].timestamp.as_deref(), Some("1"));
    assert_eq!(headers[0].updated_at.as_deref(), Some("2"));
    assert_eq!(headers[0].updated_at_ms, Some(2));
    assert_eq!(headers[0].title, None);
    assert_eq!(headers[0].preview, None);
    let hydrated = source
      .hydrate_session_header(headers[0].clone())
      .expect("legacy header should hydrate its preview");
    assert_eq!(hydrated.preview.as_deref(), Some("hello"));
    assert_eq!(session.reference.id, "ses_without_model");
    assert_eq!(session.reference.cwd.as_deref(), Some("/tmp/without-model"));
    assert_eq!(session.reference.title, None);
    assert_eq!(session.reference.preview.as_deref(), Some("hello"));
    assert!(session.events.iter().any(|event| matches!(
      event,
      AgentEvent::ProviderChanged(event)
        if event.model_provider.as_deref() == Some("openai")
          && event.model_id.as_deref() == Some("gpt-5")
    )));
  }

  #[test]
  fn preserves_session_model_when_column_exists() {
    let connection = Connection::open_in_memory().expect("database should open");
    connection
      .execute_batch(
        r#"create table session (
           id text primary key,
           parent_id text,
           directory text not null,
           model text,
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
         );
         insert into session (
           id, parent_id, directory, model, time_created, time_updated
         ) values (
           'ses_with_model',
           null,
           '/tmp/with-model',
           '{"id":"gpt-5","providerID":"openai"}',
           1,
           2
         );"#,
      )
      .expect("fixture schema with model should be created");

    let capabilities = OpenCodeCapabilities::detect(&connection).expect("schema with model should be detected");
    let session = load_session_row(&connection, capabilities, "ses_with_model", "opencode")
      .expect("schema with model should remain loadable")
      .expect("fixture session should exist");
    let model = session.model.expect("session model should be preserved");

    assert_eq!(model.id.as_deref(), Some("gpt-5"));
    assert_eq!(model.provider_id.as_deref(), Some("openai"));
  }

  #[test]
  fn keeps_native_titles_but_filters_exact_opencode_placeholders() {
    assert_eq!(
      native_title(Some("  Useful session title  ".to_string())).as_deref(),
      Some("Useful session title")
    );
    assert_eq!(native_title(Some("   ".to_string())), None);
    assert_eq!(
      native_title(Some("New session - 2026-06-06T12:34:56.789Z".to_string())),
      None
    );
    assert_eq!(
      native_title(Some("Child session - 2026-06-06T12:34:56.789Z".to_string())),
      None
    );
    assert_eq!(
      native_title(Some("New session - custom".to_string())).as_deref(),
      Some("New session - custom")
    );
  }

  #[test]
  fn uses_first_real_user_text_when_native_title_is_a_placeholder() {
    let directory = tempdir().expect("temporary directory should be created");
    let database_path = directory.path().join("opencode.db");
    let connection = Connection::open(&database_path).expect("database should open");
    connection
      .execute_batch(
        r#"create table session (
           id text primary key,
           parent_id text,
           directory text not null,
           title text not null,
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
         );
         insert into session (
           id, parent_id, directory, title, time_created, time_updated
         ) values
           ('ses_placeholder', null, '/tmp/placeholder', 'New session - 2026-06-06T12:34:56.789Z', 1, 3),
           ('ses_named', null, '/tmp/named', 'Generated title', 2, 4);
         insert into message (id, session_id, time_created, data) values
           ('msg_assistant', 'ses_placeholder', 1, '{"role":"assistant"}'),
           ('msg_user', 'ses_placeholder', 2, '{"role":"user"}');
         insert into part (id, message_id, session_id, time_created, data) values
           ('prt_assistant', 'msg_assistant', 'ses_placeholder', 1, '{"type":"text","text":"not the user"}'),
           ('prt_synthetic', 'msg_user', 'ses_placeholder', 2, '{"type":"text","text":"generated context","synthetic":true}'),
           ('prt_user', 'msg_user', 'ses_placeholder', 3, '{"type":"text","text":"  actual request  "}');"#,
      )
      .expect("fixture schema with title should be created");
    drop(connection);

    let source = OpenCodeSessionSource::new(Some(database_path));
    let headers = source.list_session_headers().expect("title catalog should be listable");
    let named = headers
      .iter()
      .find(|header| header.id == "ses_named")
      .expect("named session should exist");
    let placeholder = headers
      .iter()
      .find(|header| header.id == "ses_placeholder")
      .expect("placeholder session should exist");

    assert_eq!(named.title.as_deref(), Some("Generated title"));
    assert_eq!(named.preview, None);
    assert_eq!(placeholder.title, None);
    assert_eq!(placeholder.preview, None);

    let hydrated = source
      .hydrate_session_header(placeholder.clone())
      .expect("placeholder header should hydrate");
    assert_eq!(hydrated.title, None);
    assert_eq!(hydrated.preview.as_deref(), Some("actual request"));
  }

  #[test]
  fn loads_unknown_payloads_without_aborting_the_session() {
    let connection = Connection::open_in_memory().expect("database should open");
    connection
      .execute_batch(
        "create table message (
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
      .expect("fixture schema should be created");
    connection
      .execute(
        "insert into message (id, session_id, time_created, data)
         values (?1, ?2, ?3, ?4)",
        params![
          "msg_1",
          "ses_1",
          1_i64,
          r#"{"role":"future-role","payload":{"answer":42}}"#
        ],
      )
      .expect("message fixture should insert");
    connection
      .execute(
        "insert into part (id, message_id, session_id, time_created, data)
         values (?1, ?2, ?3, ?4, ?5)",
        params![
          "prt_1",
          "msg_1",
          "ses_1",
          2_i64,
          r#"{"type":"future-part","answer":42}"#
        ],
      )
      .expect("part fixture should insert");

    let messages = load_messages(&connection, "ses_1", "opencode").expect("unknown payloads should remain loadable");
    assert_eq!(messages.len(), 1);
    assert!(matches!(
      messages[0].data.item(),
      MessageItem::Unknown(item) if item.native_type.as_deref() == Some("future-role")
    ));
    assert_eq!(messages[0].parts[0].id, "prt_1");
    assert!(matches!(
      messages[0].parts[0].data.item(),
      PartItem::Unknown(item) if item.native_type.as_deref() == Some("future-part")
    ));
  }
}
