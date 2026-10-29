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
    for path in self.paths()? {
      // Unlike silently dropping unreadable sessions, an actionable path error
      // makes unsupported versions and damaged compressed files discoverable.
      let mut count = 0;
      let mut summary = DshSessionSummary::default();
      let header = visit_session_records(&path, |line, is_direct| {
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
      references.push(read_relation(&path)?);
    }
    sort_refs(&mut references);
    Ok(references)
  }

  pub fn list_session_relation_catalog(&self) -> Result<SessionRelationCatalog, String> {
    let mut paths = FileSessionCatalog {
      paths: Vec::new(),
      complete: true,
    };
    collect_catalog(&self.session_root()?, &mut paths);
    let mut catalog = SessionRelationCatalog {
      references: Vec::new(),
      complete: paths.complete,
    };
    for path in paths.paths {
      match read_relation(&path) {
        Ok(reference) => catalog.references.push(reference),
        Err(_) => catalog.complete = false,
      }
    }
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
    visit_session_records(&header.path, |line, is_direct| summary.observe(&line, is_direct))?;
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
      [reference] => self.load_session_path(&reference.path),
      [] => Err(format!("no dsh session found for `{id_or_path}`")),
      _ => Err(format!(
        "multiple dsh sessions match `{id_or_path}`; use an exact id or path"
      )),
    }
  }

  pub fn load_session_path(&self, path: &Path) -> Result<LoadedSession, String> {
    let mut lines = Vec::new();
    let mut summary = DshSessionSummary::default();
    let header = visit_session_records(path, |line, is_direct| {
      summary.observe(&line, is_direct);
      if is_direct {
        lines.push(line);
      }
    })?;
    let mut reference = reference(path, &header);
    reference.message_count = message_count(&lines);
    reference.title = summary.title;
    reference.preview = summary.preview;
    let history_status = if header.origin.as_deref() == Some("subagent") && header.seed_length.unwrap_or(0) > 0 {
      SessionHistoryStatus::FilteredSubagent
    } else {
      SessionHistoryStatus::Complete
    };
    Ok(LoadedSession {
      reference,
      events: normalize::normalize(&header, lines),
      history_status,
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
    use std::io::Read;
    let mut bytes = Vec::new();
    storage::reader(path)?
      .take(max_bytes as u64 + 1)
      .read_to_end(&mut bytes)
      .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    if bytes.len() > max_bytes {
      return Err("DSH decoded source exceeds the snapshot size limit".into());
    }
    let complete = bytes
      .iter()
      .rposition(|byte| *byte == b'\n')
      .map_or(0, |index| index + 1);
    let mut rows = bytes[..complete].split(|byte| *byte == b'\n');
    let first: DshSessionLine =
      serde_json::from_slice(rows.next().unwrap_or_default()).map_err(|err| format!("invalid DSH header: {err}"))?;
    let native_header = include_native.then(|| first.native().clone());
    let header = header(first, path)?;
    let seed_length = if header.origin.as_deref() == Some("subagent") {
      header.seed_length.unwrap_or(0)
    } else {
      0
    };
    let mut lines = Vec::new();
    let mut physical = Vec::new();
    let mut summary = DshSessionSummary::default();
    for (index, row) in rows.enumerate() {
      if row.iter().all(u8::is_ascii_whitespace) {
        continue;
      }
      let line: DshSessionLine =
        serde_json::from_slice(row).map_err(|err| format!("invalid DSH row {}: {err}", index + 2))?;
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
    paths.sort();
    Ok(paths)
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
    } else if kind.is_file() && matches!(entry.file_name().to_str(), Some("session.jsonl" | "session.jsonl.zstd")) {
      paths.push(entry.path());
    }
  }
  Ok(())
}

fn collect_catalog(root: &Path, catalog: &mut FileSessionCatalog) {
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
    } else if kind.is_file() && matches!(entry.file_name().to_str(), Some("session.jsonl" | "session.jsonl.zstd")) {
      catalog.paths.push(entry.path());
    }
  }
}

fn read_relation(path: &Path) -> Result<SessionRef, String> {
  let mut reader = storage::reader(path)?;
  let mut first = String::new();
  reader
    .read_line(&mut first)
    .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
  Ok(reference(path, &header(storage::parse(&first, path, 1)?, path)?))
}

pub(crate) fn header(line: DshSessionLine, path: &Path) -> Result<DshSessionHeader, String> {
  let DshSessionItem::Session(header) = line.into_item() else {
    return Err(format!("invalid dsh session header in {}", path.display()));
  };
  if header.version != 0 {
    return Err(format!(
      "unsupported dsh session version {} in {} (expected 0)",
      header.version,
      path.display()
    ));
  }
  Ok(header)
}

fn visit_session_records(path: &Path, mut visit: impl FnMut(DshSessionLine, bool)) -> Result<DshSessionHeader, String> {
  let mut reader = storage::reader(path)?;
  let mut buffer = String::new();
  reader
    .read_line(&mut buffer)
    .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
  let header = header(storage::parse(&buffer, path, 1)?, path)?;
  let seed_length = if header.origin.as_deref() == Some("subagent") {
    header.seed_length.unwrap_or(0)
  } else {
    0
  };
  let mut index = 1;
  loop {
    buffer.clear();
    index += 1;
    if reader
      .read_line(&mut buffer)
      .map_err(|err| format!("failed to read {}:{index}: {err}", path.display()))?
      == 0
    {
      break;
    }
    if buffer.trim().is_empty() {
      continue;
    }
    for line in storage::expand(storage::parse(&buffer, path, index)?)
      .map_err(|err| format!("{}:{index}: {err}", path.display()))?
    {
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
    agent_role: None,
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

#[cfg(test)]
mod tests {
  use super::*;

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
