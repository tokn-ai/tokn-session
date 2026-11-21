use crate::{normalize, storage};
use serde_json::Value;
use std::{
  collections::HashSet,
  fs::File,
  io::Read,
  path::{Path, PathBuf},
  time::UNIX_EPOCH,
};
use tokn_session_core::{
  FileSessionCatalog, LoadedSession, LoadedSessionRecords, NormalizedRecord, SessionHeader, SessionHistoryStatus,
  SessionRef, SessionRelationCatalog,
};

pub struct QoderSessionSource {
  session_dir: Option<PathBuf>,
}

impl QoderSessionSource {
  pub fn new(session_dir: Option<PathBuf>) -> Self {
    Self { session_dir }
  }
  pub fn session_root(&self) -> Result<PathBuf, String> {
    self
      .session_dir
      .clone()
      .map(Ok)
      .unwrap_or_else(storage::default_session_root)
  }
  pub fn session_roots(&self) -> Result<Vec<PathBuf>, String> {
    Ok(vec![self.session_root()?])
  }
  pub fn session_path_catalog(&self) -> Result<FileSessionCatalog, String> {
    Ok(storage::catalog(&self.session_root()?))
  }
  pub fn session_paths(&self) -> Result<Vec<PathBuf>, String> {
    Ok(self.session_path_catalog()?.paths)
  }
  pub fn session_header_exact(&self, path: &Path, id: &str) -> Result<SessionHeader, String> {
    read_session_header_exact(path, id)
  }
  pub fn session_relation_at_path(&self, path: &Path) -> Result<SessionRef, String> {
    relation(path, &storage::first_identity(path)?)
  }

  pub fn list_session_relation_catalog(&self) -> Result<SessionRelationCatalog, String> {
    let paths = self.session_path_catalog()?;
    let mut catalog = SessionRelationCatalog {
      references: Vec::new(),
      complete: paths.complete,
    };
    for path in paths.paths {
      match self.session_relation_at_path(&path) {
        Ok(reference) => catalog.references.push(reference),
        Err(_) => catalog.complete = false,
      }
    }
    sort(&mut catalog.references);
    Ok(catalog)
  }
  pub fn list_session_relations(&self) -> Result<Vec<SessionRef>, String> {
    Ok(self.list_session_relation_catalog()?.references)
  }
  pub fn list_sessions(&self) -> Result<Vec<SessionRef>, String> {
    let mut sessions = Vec::new();
    for reference in self.list_session_relations()? {
      sessions.push(self.load_session_path(&reference.path)?.reference);
    }
    sort(&mut sessions);
    Ok(sessions)
  }
  pub fn hydrate_session_header(&self, mut header: SessionHeader) -> Result<SessionHeader, String> {
    let loaded = self.load_session_path(&header.path)?;
    if loaded.reference.id != header.id {
      return Err("Qoder session identity changed".into());
    }
    header.title = loaded.reference.title;
    header.preview = loaded.reference.preview;
    header.cwd = loaded.reference.cwd;
    Ok(header)
  }
  pub fn load_session(&self, id_or_path: &str) -> Result<LoadedSession, String> {
    if Path::new(id_or_path).is_file() {
      return self.load_session_path(Path::new(id_or_path));
    }
    let references = self.list_session_relations()?;
    let exact: Vec<_> = references
      .iter()
      .filter(|reference| reference.id == id_or_path)
      .collect();
    let matching = if exact.is_empty() {
      references
        .iter()
        .filter(|reference| reference.id.starts_with(id_or_path))
        .collect()
    } else {
      exact
    };
    match matching.as_slice() {
      [reference] => self.load_session_path(&reference.path),
      [] => Err(format!("no qoder session found for `{id_or_path}`")),
      _ => Err(format!(
        "multiple qoder sessions match `{id_or_path}`; use an exact id or path"
      )),
    }
  }
  pub fn load_session_exact(&self, id: &str) -> Result<LoadedSession, String> {
    let matching: Vec<_> = self
      .list_session_relations()?
      .into_iter()
      .filter(|reference| reference.id == id)
      .collect();
    match matching.as_slice() {
      [reference] => self.load_session_path(&reference.path),
      [] => Err(format!("no qoder session found for `{id}`")),
      _ => Err(format!("multiple qoder sessions have exact id `{id}`")),
    }
  }
  pub fn load_session_path(&self, path: &Path) -> Result<LoadedSession, String> {
    Ok(
      self
        .load_session_records_path(path, false, storage::MAX_SOURCE_BYTES)?
        .into(),
    )
  }

  /// Snapshot only complete physical lines. An unfinished append is observed
  /// on the next read; malformed completed records are never silently skipped.
  pub fn load_session_records_path(
    &self,
    path: &Path,
    include_native: bool,
    max_bytes: usize,
  ) -> Result<LoadedSessionRecords, String> {
    let mut bytes = Vec::new();
    File::open(path)
      .map_err(|err| format!("failed to open {}: {err}", path.display()))?
      .take(max_bytes as u64 + 1)
      .read_to_end(&mut bytes)
      .map_err(|err| format!("failed to read Qoder transcript: {err}"))?;
    if bytes.len() > max_bytes {
      return Err("Qoder source exceeds the snapshot size limit".into());
    }
    let complete = bytes
      .iter()
      .rposition(|byte| *byte == b'\n')
      .map_or(0, |index| index + 1);
    let mut rows = Vec::new();
    let mut positions = Vec::new();
    for (position, row) in bytes[..complete].split(|byte| *byte == b'\n').enumerate() {
      if row.iter().all(u8::is_ascii_whitespace) {
        continue;
      }
      rows.push(
        serde_json::from_slice::<Value>(row).map_err(|err| format!("invalid Qoder row {}: {err}", position + 1))?,
      );
      positions.push(position + 1);
    }
    let identity = rows
      .iter()
      .find(|row| row["sessionId"].as_str().is_some() && row["cwd"].as_str().is_some())
      .ok_or("Qoder transcript has no completed identity record")?;
    let mut reference = relation(path, identity)?;
    let native_id = identity["sessionId"].as_str().expect("verified identity");
    let agent_id = identity["agentId"]
      .as_str()
      .filter(|_| reference.parent_session_id.is_some());
    // Every conversation record belongs to this exact transcript. Metadata
    // without a sessionId (e.g. file snapshots) is scoped by the native file.
    if rows
      .iter()
      .any(|row| row["sessionId"].as_str().is_some_and(|id| id != native_id))
    {
      return Err("Qoder transcript contains conflicting session identities".into());
    }
    let active = active_rows(&rows, agent_id)?;
    presentation(&mut reference, &rows, &active);
    let batches = normalize::normalize(&reference, &rows, &active);
    let mut records = vec![NormalizedRecord {
      record_id: "session".into(),
      native: None,
      events: vec![normalize::session_started(&reference)],
    }];
    for ((position, row), events) in positions.into_iter().zip(rows).zip(batches) {
      records.push(NormalizedRecord {
        record_id: format!("row:{position}"),
        native: include_native.then_some(row),
        events,
      });
    }
    let history_status = if reference.parent_session_id.is_some() {
      SessionHistoryStatus::FilteredSubagent
    } else {
      SessionHistoryStatus::Complete
    };
    Ok(LoadedSessionRecords {
      reference,
      records,
      history_status,
    })
  }
}

pub fn read_session_header_exact(path: &Path, expected_id: &str) -> Result<SessionHeader, String> {
  let identity = storage::first_identity(path)?;
  let mut reference = relation(path, &identity)?;
  if reference.id != expected_id {
    return Err("Qoder transcript does not match the expected session ID".into());
  }
  reference.cwd = Some(storage::effective_cwd(
    path,
    identity["sessionId"].as_str().expect("verified identity"),
    reference.cwd.take().expect("verified directory"),
  )?);
  let modified = std::fs::metadata(path)
    .ok()
    .and_then(|metadata| metadata.modified().ok())
    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
    .and_then(|duration| i64::try_from(duration.as_millis()).ok());
  Ok(SessionHeader {
    id: reference.id,
    parent_session_id: reference.parent_session_id,
    agent_path: reference.agent_path,
    agent_nickname: reference.agent_nickname,
    agent_role: reference.agent_role,
    title: reference.title,
    preview: reference.preview,
    path: reference.path,
    cwd: reference.cwd,
    timestamp: reference.timestamp,
    updated_at: modified.map(|time| time.to_string()),
    updated_at_ms: modified,
  })
}

fn relation(path: &Path, row: &Value) -> Result<SessionRef, String> {
  let native_id = row["sessionId"]
    .as_str()
    .filter(|id| storage::valid_id(id))
    .ok_or("invalid Qoder session ID")?;
  let cwd = row["cwd"]
    .as_str()
    .filter(|cwd| storage::native_absolute_path(cwd))
    .ok_or("Qoder transcript requires an absolute working directory")?;
  let stem = path
    .file_stem()
    .and_then(|stem| stem.to_str())
    .ok_or("Qoder transcript filename is not UTF-8")?;
  let is_child = path
    .parent()
    .and_then(Path::file_name)
    .is_some_and(|name| name == "subagents");
  let (id, parent, agent_path) = if is_child {
    let agent = stem
      .strip_prefix("agent-")
      .filter(|id| storage::valid_id(id))
      .ok_or("invalid Qoder subagent filename")?;
    if row["isSidechain"] != true
      || row["agentId"] != agent
      || path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_none_or(|name| name != native_id)
    {
      return Err("Qoder subagent identity does not match its transcript path".into());
    }
    (
      format!("{native_id}/agent/{agent}"),
      Some(native_id.to_owned()),
      Some(format!("/root/{agent}")),
    )
  } else {
    if stem != native_id || row["isSidechain"] == true {
      return Err("Qoder session identity does not match its transcript filename".into());
    }
    (native_id.to_owned(), None, None)
  };
  Ok(SessionRef {
    model_baseline: None,
    id,
    parent_session_id: parent,
    agent_path,
    agent_nickname: None,
    agent_role: None,
    title: None,
    preview: None,
    path: path.to_path_buf(),
    cwd: Some(cwd.to_owned()),
    timestamp: row["timestamp"].as_str().map(str::to_owned),
    message_count: 0,
  })
}

fn active_rows(rows: &[Value], agent: Option<&str>) -> Result<HashSet<usize>, String> {
  let in_scope = |row: &Value| match agent {
    Some(agent) => row["agentId"] == agent && row["isSidechain"] == true,
    None => row["isSidechain"] != true,
  };
  let mut by_id = std::collections::HashMap::new();
  let mut latest = None;
  let mut explicit = None;
  for (index, row) in rows.iter().enumerate() {
    if !in_scope(row) {
      continue;
    }
    if let Some(id) = row["uuid"].as_str() {
      by_id.insert(id, index);
      if matches!(row["type"].as_str(), Some("user" | "assistant"))
        && row["isMeta"] != true
        && row.get("teamName").is_none()
        && matches!(row["message"]["role"].as_str(), Some("user" | "assistant"))
        && (row["message"]["content"].is_string() || row["message"]["content"].is_array())
      {
        latest = Some(id);
      }
    }
    if agent.is_none()
      && row["type"] == "active-leaf"
      && row.get("leafUuid").is_some_and(Value::is_null)
      && row["explicit"].is_boolean()
    {
      explicit = Some(None);
    } else if agent.is_none()
      && row["type"] == "active-leaf"
      && row["explicit"].is_boolean()
      && let Some(id) = row["leafUuid"].as_str()
    {
      explicit = Some(Some(id));
    }
  }
  let mut active = HashSet::new();
  let mut next = explicit.unwrap_or(latest);
  while let Some(id) = next {
    let Some(index) = by_id.get(id).copied() else {
      return Err("Qoder active transcript chain references a missing record".into());
    };
    if !active.insert(index) {
      return Err("Qoder transcript contains a parent cycle".into());
    }
    let row = &rows[index];
    next = if row["type"] == "system" && row["subtype"] == "compact_boundary" {
      row["logicalParentUuid"].as_str().or_else(|| row["parentUuid"].as_str())
    } else {
      row["parentUuid"].as_str()
    };
  }
  Ok(active)
}

fn presentation(reference: &mut SessionRef, rows: &[Value], active: &HashSet<usize>) {
  let mut ai_title = None;
  let mut custom_title = None;
  for (index, row) in rows.iter().enumerate() {
    match row["type"].as_str() {
      Some("custom-title") => {
        custom_title = row["customTitle"]
          .as_str()
          .filter(|title| !title.is_empty())
          .map(str::to_owned)
      }
      Some("ai-title") => {
        ai_title = row["aiTitle"]
          .as_str()
          .filter(|title| !title.is_empty())
          .map(str::to_owned)
      }
      Some("relocated") if row["relocatedCwd"].as_str().is_some_and(storage::native_absolute_path) => {
        reference.cwd = row["relocatedCwd"].as_str().map(str::to_owned)
      }
      Some("user" | "assistant") if active.contains(&index) => {
        reference.message_count += 1;
        if reference.preview.is_none() && row["type"] == "user" && row["isMeta"] != true {
          reference.preview = normalize::text_content(&row["message"]["content"]).filter(|text| !text.is_empty());
        }
      }
      _ => {}
    }
  }
  reference.title = custom_title.or(ai_title);
}

fn sort(references: &mut [SessionRef]) {
  references.sort_by(|left, right| {
    right
      .timestamp
      .cmp(&left.timestamp)
      .then_with(|| right.path.cmp(&left.path))
  });
}
