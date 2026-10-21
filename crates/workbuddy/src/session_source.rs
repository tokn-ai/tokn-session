use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OpenFlags, Row};
use tokn_session_core::{
  LoadedSession, LoadedSessionRecords, NormalizedRecord, SessionHeader, SessionHistoryStatus, SessionRef,
};
use tokn_workbuddy_protocol::{WorkBuddySessionItem, WorkBuddySessionLine};

use crate::normalize::{WorkBuddyNormalizer, timestamp};

pub struct WorkBuddySessionSource {
  config_dir: Option<PathBuf>,
}

impl WorkBuddySessionSource {
  pub fn new(config_dir: Option<PathBuf>) -> Self {
    Self { config_dir }
  }

  pub fn list_sessions(&self) -> Result<Vec<SessionRef>, String> {
    let mut sessions = Vec::new();
    for header in self.list_session_headers()? {
      let summary = if header.path.is_file() {
        Some(read_history_summary(&header.path)?)
      } else {
        None
      };
      sessions.push(SessionRef {
        model_baseline: None,
        id: header.id,
        parent_session_id: None,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        title: header
          .title
          .or_else(|| summary.as_ref().and_then(|summary| summary.title.clone())),
        preview: header
          .preview
          .or_else(|| summary.as_ref().and_then(|summary| summary.preview.clone())),
        path: header.path,
        cwd: header
          .cwd
          .or_else(|| summary.as_ref().and_then(|summary| summary.cwd.clone())),
        timestamp: header.updated_at.or(header.timestamp),
        message_count: summary.map_or(0, |summary| summary.message_count),
      });
    }
    Ok(sessions)
  }

  pub fn list_session_relations(&self) -> Result<Vec<SessionRef>, String> {
    self
      .list_session_headers()?
      .into_iter()
      .map(|header| {
        Ok(SessionRef {
          model_baseline: None,
          id: header.id,
          parent_session_id: None,
          agent_path: None,
          agent_nickname: None,
          agent_role: None,
          title: header.title,
          preview: header.preview,
          path: header.path,
          cwd: header.cwd,
          timestamp: header.updated_at.or(header.timestamp),
          message_count: 0,
        })
      })
      .collect()
  }

  pub fn list_session_headers(&self) -> Result<Vec<SessionHeader>, String> {
    let config_dir = self.config_dir()?;
    let history_paths = collect_history_paths(&config_dir.join("projects"))?;
    let catalog = self.catalog(&config_dir)?;
    let mut histories_by_id: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in history_paths {
      let Some(id) = history_id(&path) else {
        continue;
      };
      histories_by_id.entry(id).or_default().push(path);
    }
    for paths in histories_by_id.values_mut() {
      paths.sort();
    }

    for deleted_id in catalog.deleted_ids {
      histories_by_id.remove(&deleted_id);
    }

    let mut headers = Vec::new();
    for row in catalog.rows {
      let expected = history_path(&config_dir, &row.cwd, &row.id);
      let path = if expected.is_file() {
        expected
      } else {
        histories_by_id
          .get_mut(&row.id)
          .and_then(|paths| (!paths.is_empty()).then(|| paths.remove(0)))
          .unwrap_or(expected)
      };
      remove_history_path(&mut histories_by_id, &row.id, &path);
      headers.push(header_from_catalog(row, path));
    }

    for (id, paths) in histories_by_id {
      for path in paths {
        let summary = read_history_head(&path, &id)?;
        let updated_at_ms = file_modified_ms(&path);
        headers.push(SessionHeader {
          id: summary.id,
          parent_session_id: None,
          agent_path: None,
          agent_nickname: None,
          agent_role: None,
          title: None,
          preview: None,
          path,
          cwd: summary.cwd,
          timestamp: timestamp(summary.timestamp),
          updated_at: updated_at_ms.map(|value| value.to_string()),
          updated_at_ms,
        });
      }
    }

    sort_headers(&mut headers);
    Ok(headers)
  }

  pub fn hydrate_session_header(&self, mut header: SessionHeader) -> Result<SessionHeader, String> {
    let summary = read_history_summary(&header.path)?;
    if header.title.is_none() {
      header.title = summary.title;
    }
    if header.preview.is_none() {
      header.preview = summary.preview;
    }
    if header.cwd.is_none() {
      header.cwd = summary.cwd;
    }
    Ok(header)
  }

  pub fn load_session(&self, id_or_path: &str) -> Result<LoadedSession, String> {
    let candidate = Path::new(id_or_path);
    if candidate.is_file() {
      return self.load_session_path(candidate);
    }

    let references = self.list_session_relations()?;
    let exact: Vec<_> = references
      .iter()
      .filter(|reference| reference.id == id_or_path)
      .collect();
    let matches = if exact.is_empty() {
      references
        .iter()
        .filter(|reference| reference.id.starts_with(id_or_path))
        .collect()
    } else {
      exact
    };
    match matches.as_slice() {
      [reference] => self.load_session_path(&reference.path),
      [] => Err(format!("no workbuddy session found for `{id_or_path}`")),
      _ => Err(format!(
        "multiple workbuddy sessions match `{id_or_path}`; use an exact id or path"
      )),
    }
  }

  pub fn load_session_exact(&self, session_id: &str) -> Result<LoadedSession, String> {
    let matches: Vec<_> = self
      .list_session_relations()?
      .into_iter()
      .filter(|reference| reference.id == session_id)
      .collect();
    match matches.as_slice() {
      [reference] => self.load_session_path(&reference.path),
      [] => Err(format!("no workbuddy session found for `{session_id}`")),
      _ => Err(format!(
        "multiple workbuddy sessions have id `{session_id}`; use an explicit path"
      )),
    }
  }

  pub fn load_session_path(&self, path: &Path) -> Result<LoadedSession, String> {
    Ok(self.normalize_records(path, read_history(path)?, false)?.into())
  }

  /// Complete-line snapshot for Relay. Record ordinals preserve duplicate native IDs.
  pub fn load_session_records_path(
    &self,
    path: &Path,
    include_native: bool,
    max_bytes: usize,
  ) -> Result<LoadedSessionRecords, String> {
    use std::io::Read;
    let file = File::open(path).map_err(|err| err.to_string())?;
    let len = file.metadata().map_err(|err| err.to_string())?.len();
    if len > max_bytes as u64 {
      return Err("WorkBuddy source exceeds the snapshot size limit".into());
    }
    let mut bytes = Vec::new();
    file.take(len).read_to_end(&mut bytes).map_err(|err| err.to_string())?;
    let complete = bytes
      .iter()
      .rposition(|byte| *byte == b'\n')
      .map_or(0, |index| index + 1);
    let lines = bytes[..complete]
      .split(|byte| *byte == b'\n')
      .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
      .map(serde_json::from_slice)
      .collect::<Result<Vec<_>, _>>()
      .map_err(|err| format!("invalid WorkBuddy JSONL: {err}"))?;
    self.normalize_records(path, lines, include_native)
  }

  fn normalize_records(
    &self,
    path: &Path,
    lines: Vec<WorkBuddySessionLine>,
    include_native: bool,
  ) -> Result<LoadedSessionRecords, String> {
    let fallback_id = history_id(path).ok_or_else(|| format!("invalid workbuddy session path {}", path.display()))?;
    let summary = summarize_history(&lines, fallback_id);
    // An explicit JSONL can live outside the configured WorkBuddy root. Only
    // consult a catalog when the source was configured or the standard
    // `projects/<encoded-cwd>/<id>.jsonl` ancestry identifies its owning root.
    let catalog = self
      .config_dir_for_history(path)?
      .map(|config_dir| self.catalog_rows(&config_dir))
      .transpose()?
      .unwrap_or_default()
      .into_iter()
      .find(|row| row.id == summary.id);
    let title = catalog.as_ref().and_then(|row| row.title.clone()).or(summary.title);
    let cwd = catalog.as_ref().map(|row| row.cwd.clone()).or(summary.cwd);
    let created_at = catalog
      .as_ref()
      .and_then(|row| row.created_at)
      .and_then(to_u64)
      .or(summary.timestamp);
    let updated_at = catalog
      .as_ref()
      .and_then(CatalogRow::updated_at)
      .and_then(to_u64)
      .or(summary.updated_at);
    let model = catalog.as_ref().and_then(|row| row.model.as_deref());
    let reference = SessionRef {
      model_baseline: None,
      id: summary.id.clone(),
      parent_session_id: None,
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      title,
      preview: summary.preview,
      path: path.to_path_buf(),
      cwd: cwd.clone(),
      timestamp: timestamp(updated_at.or(created_at)),
      message_count: summary.message_count,
    };

    let mut normalizer = WorkBuddyNormalizer::new(summary.id);
    let mut records = vec![NormalizedRecord {
      record_id: format!("session:{}", reference.id),
      native: None,
      events: normalizer.start(cwd, timestamp(created_at), model),
    }];
    for (index, line) in lines.into_iter().enumerate() {
      records.push(NormalizedRecord {
        record_id: format!("row:{index}"),
        native: include_native.then(|| line.native().clone()),
        events: normalizer.normalize_line(line),
      });
    }
    Ok(LoadedSessionRecords {
      reference,
      records,
      history_status: SessionHistoryStatus::Complete,
    })
  }

  pub fn database_path(&self) -> Result<PathBuf, String> {
    Ok(self.config_dir()?.join("workbuddy.db"))
  }

  pub fn config_dir(&self) -> Result<PathBuf, String> {
    resolve_config_dir(
      self.config_dir.clone(),
      std::env::var_os("WORKBUDDY_CONFIG_DIR"),
      std::env::var_os("CODEBUDDY_CONFIG_DIR"),
      std::env::var_os("HOME"),
      std::env::var_os("USERPROFILE"),
    )
  }

  fn config_dir_for_history(&self, path: &Path) -> Result<Option<PathBuf>, String> {
    if self.config_dir.is_some() {
      return self.config_dir().map(Some);
    }
    Ok(
      path
        .ancestors()
        .find(|ancestor| ancestor.file_name() == Some(OsStr::new("projects")))
        .and_then(Path::parent)
        .map(Path::to_path_buf),
    )
  }

  fn catalog_rows(&self, config_dir: &Path) -> Result<Vec<CatalogRow>, String> {
    Ok(self.catalog(config_dir)?.rows)
  }

  fn catalog(&self, config_dir: &Path) -> Result<Catalog, String> {
    let database_path = config_dir.join("workbuddy.db");
    if !database_path.is_file() {
      return Ok(Catalog::default());
    }
    let connection = connect_database(&database_path)?;
    load_catalog(&connection)
  }
}

#[derive(Clone, Debug)]
struct CatalogRow {
  id: String,
  cwd: String,
  title: Option<String>,
  created_at: Option<i64>,
  updated_at: Option<i64>,
  last_activity_at: Option<i64>,
  model: Option<String>,
}

#[derive(Default)]
struct Catalog {
  rows: Vec<CatalogRow>,
  deleted_ids: HashSet<String>,
}

impl CatalogRow {
  fn updated_at(&self) -> Option<i64> {
    self.last_activity_at.or(self.updated_at).or(self.created_at)
  }
}

#[derive(Default)]
struct HistorySummary {
  id: String,
  cwd: Option<String>,
  timestamp: Option<u64>,
  updated_at: Option<u64>,
  title: Option<String>,
  preview: Option<String>,
  message_count: usize,
}

fn resolve_config_dir(
  explicit: Option<PathBuf>,
  workbuddy_config_dir: Option<OsString>,
  codebuddy_config_dir: Option<OsString>,
  home: Option<OsString>,
  user_profile: Option<OsString>,
) -> Result<PathBuf, String> {
  if let Some(path) = explicit {
    return Ok(if path.is_file() {
      path
        .ancestors()
        .find(|ancestor| ancestor.file_name() == Some(OsStr::new("projects")))
        .and_then(Path::parent)
        .or_else(|| path.parent())
        .unwrap_or(&path)
        .to_path_buf()
    } else {
      path
    });
  }
  non_empty(workbuddy_config_dir)
    .or_else(|| non_empty(codebuddy_config_dir))
    .map(PathBuf::from)
    .or_else(|| {
      non_empty(home)
        .or_else(|| non_empty(user_profile))
        .map(|home| PathBuf::from(home).join(".workbuddy-ai"))
    })
    .ok_or_else(|| {
      "set WORKBUDDY_CONFIG_DIR, CODEBUDDY_CONFIG_DIR, HOME, USERPROFILE, or --session-dir to locate workbuddy sessions"
        .to_string()
    })
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
  value.filter(|value| !value.is_empty())
}

fn load_catalog(connection: &Connection) -> Result<Catalog, String> {
  let columns = table_columns(connection, "sessions")?;
  for required in ["id", "cwd"] {
    if !columns.contains(required) {
      return Err(format!(
        "workbuddy session catalog is missing required `sessions.{required}` column"
      ));
    }
  }
  let projection = [
    column_or_null(&columns, "id"),
    column_or_null(&columns, "cwd"),
    column_or_null(&columns, "title"),
    column_or_null(&columns, "custom_title"),
    column_or_null(&columns, "created_at"),
    column_or_null(&columns, "updated_at"),
    column_or_null(&columns, "last_activity_at"),
    column_or_null(&columns, "model"),
    column_or_null(&columns, "deleted_at"),
  ]
  .join(", ");
  let order = ["last_activity_at", "updated_at", "created_at"]
    .into_iter()
    .filter(|column| columns.contains(*column))
    .collect::<Vec<_>>();
  let order = if order.is_empty() {
    "id desc".to_string()
  } else {
    format!("coalesce({}) desc, id desc", order.join(", "))
  };
  let sql = format!("select {projection} from sessions order by {order}");
  let mut statement = connection
    .prepare(&sql)
    .map_err(|err| format!("failed to prepare workbuddy session query: {err}"))?;
  let rows = statement
    .query_map([], read_catalog_row)
    .map_err(|err| format!("failed to query workbuddy sessions: {err}"))?;
  let mut catalog = Catalog::default();
  for row in rows {
    let (row, deleted) = row.map_err(|err| format!("failed to read workbuddy session row: {err}"))?;
    if deleted {
      catalog.deleted_ids.insert(row.id);
    } else {
      catalog.rows.push(row);
    }
  }
  Ok(catalog)
}

fn read_catalog_row(row: &Row<'_>) -> rusqlite::Result<(CatalogRow, bool)> {
  let custom_title: Option<String> = row.get(3)?;
  let native_title: Option<String> = row.get(2)?;
  let title = non_blank_string(custom_title).or_else(|| non_blank_string(native_title));
  let catalog_row = CatalogRow {
    id: row.get(0)?,
    cwd: row.get(1)?,
    title,
    created_at: row.get(4)?,
    updated_at: row.get(5)?,
    last_activity_at: row.get(6)?,
    model: non_blank_string(row.get(7)?),
  };
  let deleted_at: Option<i64> = row.get(8)?;
  Ok((catalog_row, deleted_at.is_some()))
}

fn table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>, String> {
  let mut statement = connection
    .prepare(&format!("pragma table_info({table})"))
    .map_err(|err| format!("failed to inspect workbuddy `{table}` schema: {err}"))?;
  let columns = statement
    .query_map([], |row| row.get::<_, String>(1))
    .map_err(|err| format!("failed to query workbuddy `{table}` schema: {err}"))?
    .collect::<Result<HashSet<_>, _>>()
    .map_err(|err| format!("failed to read workbuddy `{table}` schema: {err}"))?;
  if columns.is_empty() {
    return Err(format!("workbuddy database is missing required `{table}` table"));
  }
  Ok(columns)
}

fn column_or_null(columns: &HashSet<String>, column: &str) -> String {
  if columns.contains(column) {
    column.to_string()
  } else {
    "null".to_string()
  }
}

fn connect_database(path: &Path) -> Result<Connection, String> {
  let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
  let immutable_uri = format!("file:{}?mode=ro&immutable=1", sqlite_uri_path(path));
  let mut wal_path = path.as_os_str().to_os_string();
  wal_path.push("-wal");
  let wal_path = PathBuf::from(wal_path);
  let has_wal = wal_path.metadata().is_ok_and(|metadata| metadata.len() > 0);
  if !has_wal {
    return Connection::open_with_flags(&immutable_uri, flags).map_err(|err| {
      format!(
        "failed to open workbuddy database {} immutable and read-only: {err}",
        path.display()
      )
    });
  }

  let uri = format!("file:{}?mode=ro", sqlite_uri_path(path));
  match Connection::open_with_flags(&uri, flags) {
    Ok(connection) => Ok(connection),
    Err(read_only_error) => Connection::open_with_flags(&immutable_uri, flags).map_err(|immutable_error| {
      format!(
        "failed to open workbuddy database {} with its WAL ({read_only_error}); immutable fallback also failed ({immutable_error})",
        path.display()
      )
    }),
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

fn collect_history_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
  let mut paths = Vec::new();
  collect_history_paths_into(root, &mut paths)?;
  paths.sort();
  Ok(paths)
}

fn collect_history_paths_into(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
  let entries = match std::fs::read_dir(root) {
    Ok(entries) => entries,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
    Err(err) => {
      return Err(format!(
        "failed to scan workbuddy histories at {}: {err}",
        root.display()
      ));
    }
  };
  for entry in entries {
    let entry = entry.map_err(|err| format!("failed to scan workbuddy histories at {}: {err}", root.display()))?;
    let kind = entry
      .file_type()
      .map_err(|err| format!("failed to inspect {}: {err}", entry.path().display()))?;
    if kind.is_dir() {
      collect_history_paths_into(&entry.path(), paths)?;
    } else if kind.is_file() && entry.path().extension() == Some(OsStr::new("jsonl")) {
      paths.push(entry.path());
    }
  }
  Ok(())
}

fn history_path(config_dir: &Path, cwd: &str, session_id: &str) -> PathBuf {
  config_dir
    .join("projects")
    .join(encode_cwd(cwd))
    .join(format!("{session_id}.jsonl"))
}

fn encode_cwd(cwd: &str) -> String {
  cwd
    .trim_start_matches(['/', '\\'])
    .chars()
    .map(|character| match character {
      '/' | '\\' | ':' => '-',
      character => character,
    })
    .collect()
}

fn history_id(path: &Path) -> Option<String> {
  path.file_stem()?.to_str().map(str::to_string)
}

fn remove_history_path(histories: &mut BTreeMap<String, Vec<PathBuf>>, id: &str, path: &Path) {
  let Some(paths) = histories.get_mut(id) else {
    return;
  };
  paths.retain(|candidate| candidate != path);
  if paths.is_empty() {
    histories.remove(id);
  }
}

fn header_from_catalog(row: CatalogRow, path: PathBuf) -> SessionHeader {
  let updated_at_ms = row.updated_at();
  SessionHeader {
    id: row.id,
    parent_session_id: None,
    agent_path: None,
    agent_nickname: None,
    agent_role: None,
    title: row.title,
    preview: None,
    path,
    cwd: Some(row.cwd),
    timestamp: row.created_at.map(|value| value.to_string()),
    updated_at: updated_at_ms.map(|value| value.to_string()),
    updated_at_ms,
  }
}

fn read_history_head(path: &Path, fallback_id: &str) -> Result<HistorySummary, String> {
  let file = File::open(path).map_err(|err| format!("failed to open workbuddy session {}: {err}", path.display()))?;
  let mut reader = BufReader::new(file);
  let mut buffer = String::new();
  let mut index = 0;
  loop {
    buffer.clear();
    if reader
      .read_line(&mut buffer)
      .map_err(|err| format!("failed to read workbuddy session {}: {err}", path.display()))?
      == 0
    {
      return Ok(HistorySummary {
        id: fallback_id.to_string(),
        ..HistorySummary::default()
      });
    }
    index += 1;
    if buffer.trim().is_empty() {
      continue;
    }
    let line: WorkBuddySessionLine = serde_json::from_str(&buffer)
      .map_err(|err| format!("invalid workbuddy session {}:{index}: {err}", path.display()))?;
    return Ok(HistorySummary {
      id: line.session_id().unwrap_or(fallback_id).to_string(),
      cwd: line.cwd().map(str::to_string),
      timestamp: line.timestamp(),
      updated_at: line.timestamp(),
      ..HistorySummary::default()
    });
  }
}

fn read_history_summary(path: &Path) -> Result<HistorySummary, String> {
  let fallback_id = history_id(path).ok_or_else(|| format!("invalid workbuddy session path {}", path.display()))?;
  Ok(summarize_history(&read_history(path)?, fallback_id))
}

fn read_history(path: &Path) -> Result<Vec<WorkBuddySessionLine>, String> {
  let file = File::open(path).map_err(|err| format!("failed to open workbuddy session {}: {err}", path.display()))?;
  let mut reader = BufReader::new(file);
  let mut lines = Vec::new();
  let mut buffer = String::new();
  let mut index = 0;
  loop {
    buffer.clear();
    let read = reader
      .read_line(&mut buffer)
      .map_err(|err| format!("failed to read workbuddy session {}: {err}", path.display()))?;
    if read == 0 {
      break;
    }
    index += 1;
    if buffer.trim().is_empty() {
      continue;
    }
    match serde_json::from_str(&buffer) {
      Ok(line) => lines.push(line),
      Err(err) if err.is_eof() && !buffer.ends_with('\n') => break,
      Err(err) => {
        return Err(format!("invalid workbuddy session {}:{index}: {err}", path.display()));
      }
    }
  }
  Ok(lines)
}

fn summarize_history(lines: &[WorkBuddySessionLine], fallback_id: String) -> HistorySummary {
  let mut summary = HistorySummary {
    id: fallback_id,
    ..HistorySummary::default()
  };
  for line in lines {
    if let Some(id) = line.session_id().filter(|id| !id.is_empty()) {
      summary.id = id.to_string();
    }
    if summary.cwd.is_none() {
      summary.cwd = line.cwd().map(str::to_string);
    }
    if summary.timestamp.is_none() {
      summary.timestamp = line.timestamp();
    }
    if line.timestamp().is_some() {
      summary.updated_at = line.timestamp();
    }
    match line.item() {
      WorkBuddySessionItem::Message(message) => {
        summary.message_count += 1;
        if summary.preview.is_none() && message.role.as_deref() == Some("user") {
          summary.preview = message
            .content
            .iter()
            .filter_map(|block| block.text())
            .find_map(non_blank_string_ref);
        }
      }
      WorkBuddySessionItem::AiTitle(title) => {
        if let Some(title) = non_blank_string(title.ai_title.clone()) {
          summary.title = Some(title);
        }
      }
      _ => {}
    }
  }
  summary
}

fn non_blank_string(value: Option<String>) -> Option<String> {
  value.and_then(|value| non_blank_string_ref(&value))
}

fn non_blank_string_ref(value: &str) -> Option<String> {
  let value = value.trim();
  (!value.is_empty()).then(|| value.to_string())
}

fn file_modified_ms(path: &Path) -> Option<i64> {
  let duration = path.metadata().ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
  i64::try_from(duration.as_millis()).ok()
}

fn to_u64(value: i64) -> Option<u64> {
  u64::try_from(value).ok()
}

fn sort_headers(headers: &mut [SessionHeader]) {
  headers.sort_by(|left, right| {
    right
      .updated_at_ms
      .cmp(&left.updated_at_ms)
      .then_with(|| right.timestamp.cmp(&left.timestamp))
      .then_with(|| right.id.cmp(&left.id))
      .then_with(|| right.path.cmp(&left.path))
  });
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use rusqlite::Connection;
  use tempfile::tempdir;
  use tokn_session_core::{AgentEvent, MetadataKind, Provider, Role, ToolKind, ToolRecordKind};

  use super::{WorkBuddySessionSource, encode_cwd, resolve_config_dir};

  fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
  }

  #[test]
  fn lists_catalog_and_uncataloged_histories() {
    let source = WorkBuddySessionSource::new(Some(fixture_root()));
    let sessions = source.list_sessions().expect("fixtures should list");

    assert_eq!(sessions.len(), 5);
    let chat = sessions
      .iter()
      .find(|session| session.id == "wb-chat-basic")
      .expect("chat fixture should be listed");
    assert_eq!(
      chat.title.as_deref(),
      Some("Explain provider-agnostic agent session layer")
    );
    assert_eq!(chat.cwd.as_deref(), Some("/fixture/workspace"));
    assert_eq!(chat.timestamp.as_deref(), Some("1788265740971"));
    assert_eq!(chat.message_count, 2);
    assert!(chat.path.ends_with("fixture-workspace/wb-chat-basic.jsonl"));

    let orphan = sessions
      .iter()
      .find(|session| session.id == "wb-file-read")
      .expect("uncataloged headless history should be listed");
    assert_eq!(orphan.message_count, 2);
    assert_eq!(orphan.cwd.as_deref(), Some("/fixture/workspace"));
    assert!(orphan.title.is_none());
    assert!(
      orphan
        .preview
        .as_deref()
        .is_some_and(|preview| preview.starts_with("Use the Read tool"))
    );
  }

  #[test]
  fn headers_are_catalog_first_and_hydrate_body_metadata() {
    let source = WorkBuddySessionSource::new(Some(fixture_root()));
    let headers = source.list_session_headers().expect("fixtures should list");
    let orphan = headers
      .into_iter()
      .find(|header| header.id == "wb-file-read")
      .expect("orphan history should have a header");
    assert!(orphan.preview.is_none());
    let hydrated = source
      .hydrate_session_header(orphan)
      .expect("orphan metadata should hydrate");
    assert!(
      hydrated
        .preview
        .as_deref()
        .is_some_and(|preview| preview.starts_with("Use the Read tool"))
    );
  }

  #[test]
  fn resolves_exact_prefix_and_explicit_path() {
    let source = WorkBuddySessionSource::new(Some(fixture_root()));
    let exact = source.load_session("wb-file-read").expect("exact orphan id should win");
    assert_eq!(exact.reference.id, "wb-file-read");
    assert!(exact.events.iter().any(|event| matches!(event, AgentEvent::Error(_))));

    let prefix = source.load_session("wb-chat-b").expect("unique prefix should resolve");
    assert_eq!(prefix.reference.id, "wb-chat-basic");

    let error = source
      .load_session("wb-file")
      .expect_err("shared prefix should be ambiguous");
    assert!(error.contains("multiple workbuddy sessions"));

    let path = fixture_root().join("projects/fixture-workspace/wb-shell-command.jsonl");
    let explicit = source
      .load_session(path.to_str().expect("fixture path should be UTF-8"))
      .expect("explicit path should load");
    assert_eq!(explicit.reference.id, "wb-shell-command");
  }

  #[test]
  fn loads_normalized_catalog_session() {
    let source = WorkBuddySessionSource::new(Some(fixture_root()));
    let session = source
      .load_session_exact("wb-shell-command")
      .expect("shell fixture should load");

    assert_eq!(session.reference.message_count, 2);
    assert_eq!(session.reference.title.as_deref(), Some("Run pwd and uname in Bash"));
    assert!(matches!(&session.events[0], AgentEvent::SessionStarted(event)
      if matches!(event.provider, Provider::WorkBuddy)));
    let calls = session
      .events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::ToolCall(call) => Some(call),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|call| matches!(call.tool_kind, ToolKind::Shell)));
    assert_eq!(
      calls
        .iter()
        .filter(|call| matches!(call.record_kind, ToolRecordKind::Invocation))
        .count(),
      2
    );
    assert_eq!(
      calls
        .iter()
        .filter(|call| matches!(call.record_kind, ToolRecordKind::Result))
        .count(),
      2
    );
    assert!(
      session
        .events
        .iter()
        .any(|event| matches!(event, AgentEvent::Metadata(metadata)
      if matches!(metadata.kind, MetadataKind::Context)
        && metadata.native_type == "file-history-snapshot"))
    );
    assert!(
      session
        .events
        .iter()
        .any(|event| matches!(event, AgentEvent::Message(message)
      if matches!(message.role, Role::Assistant)
        && message.text.contains("operating-system name")))
    );
  }

  #[test]
  fn resolves_configuration_root_and_encodes_workspace() {
    assert_eq!(encode_cwd("/fixture/workspace"), "fixture-workspace");
    let explicit = PathBuf::from("fixtures");
    assert_eq!(
      resolve_config_dir(Some(explicit.clone()), None, None, None, None).expect("explicit path should resolve"),
      explicit
    );
    assert_eq!(
      resolve_config_dir(
        None,
        Some("workbuddy-home".into()),
        Some("codebuddy-home".into()),
        Some("platform-home".into()),
        None,
      )
      .expect("environment path should resolve"),
      Path::new("workbuddy-home")
    );
    assert_eq!(
      resolve_config_dir(None, None, None, Some("platform-home".into()), None).expect("home should resolve"),
      Path::new("platform-home/.workbuddy-ai")
    );
  }

  #[test]
  fn missing_storage_lists_empty() {
    let root = tempdir().expect("tempdir");
    let source = WorkBuddySessionSource::new(Some(root.path().to_path_buf()));
    assert!(
      source
        .list_sessions()
        .expect("missing storage should be empty")
        .is_empty()
    );
  }

  #[test]
  fn quiescent_database_reads_do_not_create_sqlite_sidecars() {
    let root = tempdir().expect("tempdir");
    let database = root.path().join("workbuddy.db");
    std::fs::copy(fixture_root().join("workbuddy.db"), &database).expect("fixture database should copy");
    let source = WorkBuddySessionSource::new(Some(root.path().to_path_buf()));

    let headers = source.list_session_headers().expect("copied catalog should list");

    assert_eq!(headers.len(), 4);
    assert!(!root.path().join("workbuddy.db-wal").exists());
    assert!(!root.path().join("workbuddy.db-shm").exists());
  }

  #[test]
  fn reads_active_wal_and_keeps_deleted_histories_out_of_the_catalog() {
    let root = tempdir().expect("tempdir");
    let database = root.path().join("workbuddy.db");
    let connection = Connection::open(&database).expect("test database should open");
    connection
      .execute_batch(
        "pragma journal_mode=wal;
         pragma wal_autocheckpoint=0;
         create table sessions (
           id text primary key,
           cwd text not null,
           title text,
           custom_title text,
           created_at integer,
           updated_at integer,
           last_activity_at integer,
           model text,
           deleted_at integer
         );
         pragma wal_checkpoint(truncate);
         insert into sessions values
           ('live-session', '/fixture/live', 'Live title', null, 10, 20, 30, 'local:test', null),
           ('deleted-session', '/fixture/deleted', 'Deleted title', null, 11, 21, 31, 'local:test', 32);",
      )
      .expect("test catalog should initialize");
    assert!(
      root
        .path()
        .join("workbuddy.db-wal")
        .metadata()
        .is_ok_and(|metadata| metadata.len() > 0)
    );
    let source = WorkBuddySessionSource::new(Some(root.path().to_path_buf()));

    let headers = source.list_session_headers().expect("active WAL catalog should list");

    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].id, "live-session");
    assert_eq!(headers[0].title.as_deref(), Some("Live title"));
  }
}
