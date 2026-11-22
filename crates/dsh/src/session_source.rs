use std::path::{Path, PathBuf};

use tokn_dsh_protocol::{DshSessionItem, DshSessionLine, SessionHeader as DshSessionHeader};
use tokn_session_core::{
  FileSessionCatalog, LoadedSession, LoadedSessionRecords, NormalizedRecord, SessionHeader, SessionHistoryStatus,
  SessionRef, SessionRelationCatalog,
};

use crate::{normalize, storage};

pub struct DshSessionSource {
  session_dir: Option<PathBuf>,
}

impl DshSessionSource {
  pub fn new(session_dir: Option<PathBuf>) -> Self {
    Self { session_dir }
  }

  /// Verify one exact native file without discovery or conversation-body reads.
  pub fn session_header_exact(&self, path: &Path, session_id: &str) -> Result<SessionHeader, String> {
    crate::read_session_header_exact(path, session_id)
  }

  pub fn list_sessions(&self) -> Result<Vec<SessionRef>, String> {
    let mut references = Vec::new();
    for relation in self.list_session_relations()? {
      let path = relation.path;
      // Unlike silently dropping unreadable sessions, an actionable path error
      // makes unsupported versions and damaged compressed files discoverable.
      let mut count = 0;
      let mut summary = DshSessionSummary::default();
      let header = visit_session_records(&path, Some(&relation.id), |line, is_direct| {
        summary.observe(&line, is_direct);
        if is_direct && is_message(&line) {
          count += 1;
        }
      })?;
      let mut reference = reference(&path, &header);
      reference.message_count = count;
      reference.title = summary.title;
      reference.preview = summary.preview;
      references.push(reference);
    }
    sort_refs(&mut references);
    Ok(references)
  }

  pub fn list_session_relations(&self) -> Result<Vec<SessionRef>, String> {
    let mut references = Vec::new();
    for path in self.paths()? {
      references.extend(read_relations(&path)?);
    }
    deduplicate(&mut references);
    sort_refs(&mut references);
    Ok(references)
  }

  pub fn list_session_relation_catalog(&self) -> Result<SessionRelationCatalog, String> {
    let mut paths = FileSessionCatalog {
      paths: Vec::new(),
      complete: true,
    };
    collect_catalog(&self.session_root()?, &mut paths);
    match select_generations(std::mem::take(&mut paths.paths)) {
      Ok(selected) => paths.paths = selected,
      Err(_) => paths.complete = false,
    }
    let mut catalog = SessionRelationCatalog {
      references: Vec::new(),
      complete: paths.complete,
    };
    for path in paths.paths {
      match read_relations(&path) {
        Ok(references) => catalog.references.extend(references),
        Err(_) => catalog.complete = false,
      }
    }
    deduplicate(&mut catalog.references);
    sort_refs(&mut catalog.references);
    Ok(catalog)
  }

  /// Populate presentation metadata stored after DSH's first session row.
  ///
  /// Relation discovery deliberately remains a header-only operation. Callers
  /// can hydrate only the rows they intend to display, and this streaming scan
  /// works for both JSONL and concatenated Zstandard logs.
  pub fn hydrate_session_header(&self, mut header: SessionHeader) -> Result<SessionHeader, String> {
    let mut summary = DshSessionSummary::default();
    visit_session_records(&header.path, Some(&header.id), |line, is_direct| {
      summary.observe(&line, is_direct)
    })?;
    header.title = summary.title;
    header.preview = summary.preview;
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
      [reference] => self.load_session_path_with_id(&reference.path, &reference.id),
      [] => Err(format!("no dsh session found for `{id_or_path}`")),
      _ => Err(format!(
        "multiple dsh sessions match `{id_or_path}`; use an exact id or path"
      )),
    }
  }

  pub fn load_session_path(&self, path: &Path) -> Result<LoadedSession, String> {
    let loaded = self.records(path, None, false, 128 * 1024 * 1024)?;
    Ok(LoadedSession {
      reference: loaded.reference,
      events: loaded.records.into_iter().flat_map(|record| record.events).collect(),
      history_status: loaded.history_status,
    })
  }

  /// Exact identity is required when several native sessions share a database.
  pub fn load_session_path_with_id(&self, path: &Path, session_id: &str) -> Result<LoadedSession, String> {
    let loaded = self.records(path, Some(session_id), false, 128 * 1024 * 1024)?;
    Ok(LoadedSession {
      reference: loaded.reference,
      events: loaded.records.into_iter().flat_map(|record| record.events).collect(),
      history_status: loaded.history_status,
    })
  }

  /// Bounded complete-line snapshot, including concatenated Zstandard frames.
  /// Native batches retain physical packed rows, while normalization operates
  /// on their logical members and can revise earlier unfinished stream output.
  pub fn load_session_records_path(
    &self,
    path: &Path,
    include_native: bool,
    max_bytes: usize,
  ) -> Result<LoadedSessionRecords, String> {
    self.records(path, None, include_native, max_bytes)
  }

  pub fn load_session_records_path_with_id(
    &self,
    path: &Path,
    session_id: &str,
    include_native: bool,
    max_bytes: usize,
  ) -> Result<LoadedSessionRecords, String> {
    self.records(path, Some(session_id), include_native, max_bytes)
  }

  fn records(
    &self,
    path: &Path,
    session_id: Option<&str>,
    include_native: bool,
    max_bytes: usize,
  ) -> Result<LoadedSessionRecords, String> {
    let snapshot = storage::snapshot(path, session_id, max_bytes)?;
    let header = snapshot.header;
    let native_header = include_native.then_some(snapshot.native_header);
    let seed_length = if header.origin.as_deref() == Some("subagent") {
      header.seed_length.unwrap_or(0)
    } else {
      0
    };
    let mut lines = Vec::new();
    let mut physical = Vec::new();
    let mut summary = DshSessionSummary::default();
    for (index, line) in snapshot.rows.into_iter().enumerate() {
      let native = include_native.then(|| line.native().clone());
      let start = lines.len();
      for line in storage::expand(line)? {
        let direct = !line
          .native()
          .get("seq")
          .and_then(serde_json::Value::as_u64)
          .is_some_and(|seq| seq < seed_length);
        summary.observe(&line, direct);
        if direct {
          lines.push(line);
        }
      }
      physical.push((index, native, lines.len() - start));
    }
    let mut reference = reference(path, &header);
    reference.message_count = message_count(&lines);
    reference.title = summary.title;
    reference.preview = summary.preview;
    let mut batches = normalize::normalize_batches(&header, lines).into_iter();
    let mut records = vec![NormalizedRecord {
      record_id: format!("session:{}", header.id),
      native: native_header,
      events: batches.next().unwrap_or_default(),
    }];
    for (index, native, count) in physical {
      records.push(NormalizedRecord {
        record_id: format!("row:{index}"),
        native,
        events: batches.by_ref().take(count).flatten().collect(),
      });
    }
    Ok(LoadedSessionRecords {
      reference,
      records,
      history_status: if seed_length > 0 {
        SessionHistoryStatus::FilteredSubagent
      } else {
        SessionHistoryStatus::Complete
      },
    })
  }

  pub fn session_root(&self) -> Result<PathBuf, String> {
    session_root(
      self.session_dir.clone(),
      std::env::var_os("DSH_HOME"),
      std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )
  }

  fn paths(&self) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    collect(&self.session_root()?, &mut paths)?;
    let mut selected = select_generations(paths)?;
    selected.sort();
    Ok(selected)
  }
}

fn session_root(
  explicit: Option<PathBuf>,
  dsh_home: Option<std::ffi::OsString>,
  home: Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
  if let Some(root) = explicit {
    return Ok(root);
  }
  if let Some(home) = dsh_home.filter(|value| !value.is_empty()) {
    return Ok(PathBuf::from(home).join("sessions"));
  }
  home
    .map(|home| PathBuf::from(home).join(".dsh/sessions"))
    .ok_or_else(|| "set DSH_HOME, HOME, USERPROFILE, or --session-dir to locate dsh sessions".into())
}

fn collect(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
  if root.is_file() {
    paths.push(root.to_owned());
    return Ok(());
  }
  let entries = match std::fs::read_dir(root) {
    Ok(entries) => entries,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
    Err(err) => return Err(format!("failed to scan {}: {err}", root.display())),
  };
  for entry in entries {
    let entry = entry.map_err(|err| format!("failed to scan {}: {err}", root.display()))?;
    let kind = entry.file_type().map_err(|err| err.to_string())?;
    if kind.is_dir() {
      collect(&entry.path(), paths)?;
    } else if kind.is_file() && is_source(&entry.path()) {
      paths.push(entry.path());
    }
  }
  Ok(())
}

fn collect_catalog(root: &Path, catalog: &mut FileSessionCatalog) {
  if root.is_file() {
    catalog.paths.push(root.to_owned());
    return;
  }
  let Ok(entries) = std::fs::read_dir(root) else {
    catalog.complete = false;
    return;
  };
  for entry in entries {
    let Ok(entry) = entry else {
      catalog.complete = false;
      continue;
    };
    let Ok(kind) = entry.file_type() else {
      catalog.complete = false;
      continue;
    };
    if kind.is_dir() {
      collect_catalog(&entry.path(), catalog);
    } else if kind.is_file() && is_source(&entry.path()) {
      catalog.paths.push(entry.path());
    }
  }
}

fn read_relation(path: &Path) -> Result<SessionRef, String> {
  if crate::sqlite::is_marker(path) {
    let value = crate::sqlite::header(path, None)?;
    return Ok(reference(
      path,
      &header(serde_json::from_value(value).map_err(|error| error.to_string())?, path)?,
    ));
  }
  Ok(reference(path, &crate::identity::read_file_header(path)?))
}

pub(crate) fn header(line: DshSessionLine, path: &Path) -> Result<DshSessionHeader, String> {
  let DshSessionItem::Session(header) = line.into_item() else {
    return Err(format!("invalid dsh session header in {}", path.display()));
  };
  if header.version > 4 {
    return Err(format!(
      "unsupported dsh session version {} in {} (supported 0 through 4)",
      header.version,
      path.display()
    ));
  }
  if header.version >= 2 && header.is_seeded.is_none() {
    return Err("DSH version 2 through 4 header requires isSeeded".into());
  }
  if jsonl_generation(path).is_some_and(|(version, _)| version != header.version) {
    return Err("DSH session filename generation does not match its header".into());
  }
  if header.id.is_empty()
    || header.id.len() > 512
    || header.id.trim() != header.id
    || header.id.chars().any(char::is_control)
    || header.created_at > 9_007_199_254_740_991
    || header.delegation_depth > 9_007_199_254_740_991
    || header.cwd.as_deref().is_some_and(|cwd| !native_absolute_path(cwd))
  {
    return Err("Invalid DSH session identity or metadata".into());
  }
  Ok(header)
}

fn visit_session_records(
  path: &Path,
  session_id: Option<&str>,
  mut visit: impl FnMut(DshSessionLine, bool),
) -> Result<DshSessionHeader, String> {
  let snapshot = storage::snapshot(path, session_id, 128 * 1024 * 1024)?;
  let header = snapshot.header;
  let seed_length = if header.origin.as_deref() == Some("subagent") {
    header.seed_length.unwrap_or(0)
  } else {
    0
  };
  for row in snapshot.rows {
    for line in storage::expand(row)? {
      // seedLength is immutable fork ancestry, unlike end-seed, which also
      // appears on every resume and must not hide the session's own past turns.
      let is_direct = !line
        .native()
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|seq| seq < seed_length);
      visit(line, is_direct);
    }
  }
  Ok(header)
}

fn is_source(path: &Path) -> bool {
  jsonl_generation(path).is_some()
    || path.file_name().is_some_and(|name| name == "sessions.sqlite")
      && path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == ".segments")
}

fn jsonl_generation(path: &Path) -> Option<(u64, bool)> {
  let name = path.file_name()?.to_str()?;
  let (raw, compressed) = name
    .strip_suffix(".zstd")
    .or_else(|| name.strip_suffix(".zst"))
    .map_or((name, false), |raw| (raw, true));
  if raw == "session.jsonl" {
    return Some((0, compressed));
  }
  let digits = raw.strip_prefix("session.v")?.strip_suffix(".jsonl")?;
  if digits.starts_with('0') || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  let version = digits.parse::<u64>().ok()?;
  (version > 0 && version <= 9_007_199_254_740_991).then_some((version, compressed))
}

fn select_generations(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
  let mut directories: std::collections::BTreeMap<PathBuf, Vec<(u64, bool, PathBuf)>> =
    std::collections::BTreeMap::new();
  let mut selected = Vec::new();
  for path in paths {
    if let Some((version, compressed)) = jsonl_generation(&path) {
      directories
        .entry(path.parent().unwrap_or(Path::new("")).to_owned())
        .or_default()
        .push((version, compressed, path));
    } else {
      selected.push(path);
    }
  }
  for (_, mut generations) in directories {
    let compressed = generations[0].1;
    if generations.iter().any(|(_, encoding, _)| *encoding != compressed) {
      return Err("DSH session directory contains conflicting JSONL encodings".into());
    }
    generations.sort_by_key(|(version, _, _)| std::cmp::Reverse(*version));
    if generations.len() > 1 && generations[0].0 == generations[1].0 {
      return Err("DSH session directory contains ambiguous duplicate generations".into());
    }
    selected.push(generations.remove(0).2);
  }
  Ok(selected)
}

fn read_relations(path: &Path) -> Result<Vec<SessionRef>, String> {
  if crate::sqlite::is_database(path) {
    crate::sqlite::headers(path)?
      .into_iter()
      .map(|native| {
        let header = header(serde_json::from_value(native).map_err(|error| error.to_string())?, path)?;
        Ok(reference(path, &header))
      })
      .collect()
  } else {
    Ok(vec![read_relation(path)?])
  }
}

fn deduplicate(references: &mut Vec<SessionRef>) {
  let routed: std::collections::HashSet<_> = references
    .iter()
    .filter(|reference| crate::sqlite::is_database(&reference.path))
    .map(|reference| reference.id.clone())
    .collect();
  references.retain(|reference| crate::sqlite::is_database(&reference.path) || !routed.contains(&reference.id));
}

pub(crate) fn reference(path: &Path, header: &DshSessionHeader) -> SessionRef {
  SessionRef {
    model_baseline: None,
    id: header.id.clone(),
    parent_session_id: if header.origin.as_deref() == Some("subagent") {
      header.parent_session.clone()
    } else {
      None
    },
    agent_path: None,
    agent_nickname: None,
    agent_role: (header.origin.as_deref() == Some("subagent")).then(|| "subagent".into()),
    title: None,
    preview: None,
    path: path.to_path_buf(),
    cwd: header.cwd.clone(),
    timestamp: Some(header.created_at.to_string()),
    message_count: 0,
  }
}

#[derive(Default)]
struct DshSessionSummary {
  title: Option<String>,
  preview: Option<String>,
}

impl DshSessionSummary {
  fn observe(&mut self, line: &DshSessionLine, is_direct: bool) {
    // DSH's title projection is a pure last-valid-event-wins fold. Title
    // events inherited in a subagent seed still name that fork, while the
    // fallback preview intentionally starts with the subagent's direct input.
    if let Some(title) = crate::metadata::session_title(line.native()).and_then(non_blank) {
      self.title = Some(title);
    }
    if is_direct && self.preview.is_none() {
      self.preview = dsh_user_preview(line);
    }
  }
}

fn dsh_user_preview(line: &DshSessionLine) -> Option<String> {
  let DshSessionItem::Event(tokn_dsh_protocol::SessionEvent::UserMessage(event)) = line.item() else {
    return None;
  };
  if event.data.role != "user" || event.data.source.kind != "user" {
    return None;
  }
  non_blank(
    event
      .data
      .content
      .iter()
      .filter_map(|block| match block {
        tokn_dsh_protocol::ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
      })
      .collect::<Vec<_>>()
      .join("\n"),
  )
}

fn non_blank(value: String) -> Option<String> {
  let value = value.trim();
  (!value.is_empty()).then(|| value.to_string())
}

fn message_count(lines: &[DshSessionLine]) -> usize {
  lines.iter().filter(|line| is_message(line)).count()
}

fn is_message(line: &DshSessionLine) -> bool {
  matches!(
    line.item(),
    DshSessionItem::Event(
      tokn_dsh_protocol::SessionEvent::UserMessage(_) | tokn_dsh_protocol::SessionEvent::AssistantMessage(_)
    )
  )
}

fn sort_refs(references: &mut [SessionRef]) {
  references.sort_by_key(|reference| {
    (
      std::cmp::Reverse(
        reference
          .timestamp
          .as_deref()
          .and_then(|value| value.parse::<u64>().ok()),
      ),
      reference.path.clone(),
    )
  });
}

fn native_absolute_path(path: &str) -> bool {
  if path.contains('\0') {
    return false;
  }
  if path.starts_with('/') {
    return true;
  }
  let bytes = path.as_bytes();
  if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\') {
    return true;
  }
  if let Some(unc) = path.strip_prefix("\\\\") {
    let mut parts = unc.split('\\');
    return parts.next().is_some_and(|part| !part.is_empty()) && parts.next().is_some_and(|part| !part.is_empty());
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_canonical_generation_names_are_discovered() {
    for (name, expected) in [
      ("session.jsonl", Some((0, false))),
      ("session.v4.jsonl.zstd", Some((4, true))),
      ("session.v0.jsonl", None),
      ("session.v04.jsonl", None),
      ("session.V4.jsonl", None),
      ("session.v4.jsonl.tmp", None),
    ] {
      assert_eq!(jsonl_generation(Path::new(name)), expected, "{name}");
    }
  }

  #[test]
  fn resolves_root_precedence_without_mutating_environment() {
    assert_eq!(
      session_root(Some("explicit".into()), Some("custom".into()), Some("home".into())).unwrap(),
      PathBuf::from("explicit")
    );
    assert_eq!(
      session_root(None, Some("custom".into()), Some("home".into())).unwrap(),
      PathBuf::from("custom/sessions")
    );
    assert_eq!(
      session_root(None, Some("".into()), Some("home".into())).unwrap(),
      PathBuf::from("home/.dsh/sessions")
    );
  }
}
