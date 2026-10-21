use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::event::{PiContentBlock, PiMessage, PiSessionItem, PiSessionLine, PiUserContent};
use crate::normalize::PiNormalizer;
use tokn_session_core::{LoadedSession, SessionHeader, SessionHistoryStatus, SessionRef};

pub struct PiSessionSource {
  session_dir: Option<PathBuf>,
}

impl PiSessionSource {
  pub fn new(session_dir: Option<PathBuf>) -> Self {
    Self { session_dir }
  }

  pub fn list_sessions(&self) -> Result<Vec<SessionRef>, String> {
    self.list_session_refs(inspect_session)
  }

  pub fn list_session_relations(&self) -> Result<Vec<SessionRef>, String> {
    self.list_session_refs(inspect_session_header)
  }

  /// Returns the effective session root used for discovery.
  ///
  /// Consumers that maintain a file watcher should use this resolved root so
  /// environment overrides and platform-specific home-directory handling stay
  /// consistent with ordinary Pi discovery.
  pub fn session_roots(&self) -> Result<Vec<PathBuf>, String> {
    Ok(vec![self.root()?])
  }

  /// Enumerates persisted session paths without opening their contents.
  pub fn session_paths(&self) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for root in self.session_roots()? {
      collect_jsonl_files(&root, &mut paths)?;
    }
    paths.sort();
    Ok(paths)
  }

  /// Reads the header relation for one known session path without scanning its
  /// conversation body or sibling session files.
  pub fn session_relation_at_path(&self, path: &Path) -> Result<SessionRef, String> {
    inspect_session_header(path)
  }

  /// Populate presentation metadata stored outside Pi's first session row.
  ///
  /// Relation discovery deliberately remains a header-only operation. Callers
  /// can hydrate only the rows they intend to display, avoiding a full scan of
  /// every Pi transcript on each list refresh.
  pub fn hydrate_session_header(&self, mut header: SessionHeader) -> Result<SessionHeader, String> {
    let summary = inspect_session_summary(&header.path)?;
    header.title = summary.title;
    header.preview = summary.preview;
    Ok(header)
  }

  fn list_session_refs(&self, inspect: fn(&Path) -> Result<SessionRef, String>) -> Result<Vec<SessionRef>, String> {
    let mut refs = Vec::new();
    for path in self.session_paths()? {
      if let Ok(reference) = inspect(&path) {
        refs.push(reference);
      }
    }
    refs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.path.cmp(&a.path)));
    Ok(refs)
  }

  pub fn load_session(&self, id_or_path: &str) -> Result<LoadedSession, String> {
    let path = self.resolve_session(id_or_path)?;
    self.load_session_path(&path)
  }

  pub fn load_session_path(&self, path: &Path) -> Result<LoadedSession, String> {
    let reference = inspect_session(&path)?;
    let file = File::open(&path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let reader = BufReader::new(file);
    let mut normalizer = PiNormalizer::new();
    let mut events = Vec::new();

    for (index, line) in reader.lines().enumerate() {
      let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
      if line.trim().is_empty() {
        continue;
      }
      let event: PiSessionLine = serde_json::from_str(&line)
        .map_err(|err| format!("invalid pi jsonl at {}:{}: {err}", path.display(), index + 1))?;
      events.extend(normalizer.normalize(event));
    }

    Ok(LoadedSession {
      reference,
      events,
      history_status: SessionHistoryStatus::Complete,
    })
  }

  fn resolve_session(&self, id_or_path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(id_or_path);
    if candidate.exists() {
      return Ok(candidate);
    }

    let matches: Vec<_> = self
      .list_sessions()?
      .into_iter()
      .filter(|session| session.id == id_or_path || session.id.starts_with(id_or_path))
      .collect();

    match matches.as_slice() {
      [session] => Ok(session.path.clone()),
      [] => Err(format!("no pi session found for `{id_or_path}`")),
      _ => Err(format!("multiple pi sessions match `{id_or_path}`")),
    }
  }

  fn root(&self) -> Result<PathBuf, String> {
    if let Some(root) = &self.session_dir {
      return Ok(root.clone());
    }

    let configured_session_dir = std::env::var_os("PI_CODING_AGENT_SESSION_DIR");
    let configured_agent_dir = std::env::var_os("PI_CODING_AGENT_DIR");
    let platform_home = dirs::home_dir();
    resolve_pi_session_root(
      self.session_dir.as_deref(),
      configured_session_dir.as_deref(),
      configured_agent_dir.as_deref(),
      platform_home.as_deref(),
      cfg!(windows),
    )
  }
}

fn resolve_pi_session_root(
  explicit_session_dir: Option<&Path>,
  configured_session_dir: Option<&OsStr>,
  configured_agent_dir: Option<&OsStr>,
  platform_home: Option<&Path>,
  is_windows: bool,
) -> Result<PathBuf, String> {
  if let Some(explicit_session_dir) = explicit_session_dir {
    return Ok(explicit_session_dir.to_path_buf());
  }

  if let Some(configured_session_dir) = configured_session_dir.filter(|value| !value.is_empty()) {
    return expand_pi_tilde(
      configured_session_dir,
      platform_home,
      "PI_CODING_AGENT_SESSION_DIR",
      is_windows,
    );
  }

  if let Some(configured_agent_dir) = configured_agent_dir.filter(|value| !value.is_empty()) {
    return expand_pi_tilde(configured_agent_dir, platform_home, "PI_CODING_AGENT_DIR", is_windows)
      .map(|agent_dir| agent_dir.join("sessions"));
  }

  let platform_home = platform_home
    .filter(|path| !path.as_os_str().is_empty())
    .ok_or_else(|| {
      "could not determine the user home directory; set PI_CODING_AGENT_SESSION_DIR, PI_CODING_AGENT_DIR, or pass --session-dir"
        .to_string()
    })?;
  Ok(platform_home.join(".pi").join("agent").join("sessions"))
}

fn expand_pi_tilde(
  value: &OsStr,
  platform_home: Option<&Path>,
  variable: &str,
  is_windows: bool,
) -> Result<PathBuf, String> {
  let Some(value) = value.to_str() else {
    return Ok(PathBuf::from(value));
  };
  let suffix = if value == "~" {
    Some("")
  } else if let Some(suffix) = value.strip_prefix("~/") {
    Some(suffix)
  } else if is_windows {
    value.strip_prefix("~\\")
  } else {
    None
  };

  let Some(suffix) = suffix else {
    return Ok(PathBuf::from(value));
  };
  let platform_home = platform_home.ok_or_else(|| {
    format!(
      "{variable} uses `~`, but the user home directory could not be determined; set {variable} to a path without `~` or pass --session-dir"
    )
  })?;

  if suffix.is_empty() {
    Ok(platform_home.to_path_buf())
  } else {
    Ok(platform_home.join(suffix))
  }
}

fn collect_jsonl_files(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
  if !dir.exists() {
    return Ok(());
  }

  for entry in std::fs::read_dir(dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))? {
    let entry = entry.map_err(|err| format!("failed to read entry in {}: {err}", dir.display()))?;
    let path = entry.path();
    if path.is_dir() {
      collect_jsonl_files(&path, paths)?;
    } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
      paths.push(path);
    }
  }
  Ok(())
}

fn inspect_session(path: &Path) -> Result<SessionRef, String> {
  let mut reference = inspect_session_header(path)?;
  let summary = inspect_session_summary(path)?;
  reference.message_count = summary.message_count;
  reference.title = summary.title;
  reference.preview = summary.preview;
  Ok(reference)
}

/// Lightweight presentation accumulator shared by historical and incremental
/// readers. It does not retain messages or normalize an event stream.
#[derive(Clone, Default)]
pub struct PiSessionSummary {
  pub title: Option<String>,
  pub preview: Option<String>,
  message_count: usize,
}

impl PiSessionSummary {
  /// Inspect one complete record. A missing/blank session_info name clears
  /// the previous name; only the first eligible user message supplies preview.
  pub fn ingest_line(&mut self, line: &str) -> Result<(), String> {
    let line: PiSessionLine = serde_json::from_str(line).map_err(|err| err.to_string())?;
    match line.into_item() {
      PiSessionItem::SessionInfo(info) => self.title = info.name.and_then(non_blank),
      PiSessionItem::Message(message) => {
        self.message_count = self.message_count.saturating_add(1);
        if self.preview.is_none() {
          self.preview = message.message.and_then(pi_user_preview);
        }
      }
      _ => {}
    }
    Ok(())
  }
}

fn inspect_session_summary(path: &Path) -> Result<PiSessionSummary, String> {
  let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  let reader = BufReader::new(file);
  let mut summary = PiSessionSummary::default();

  for (index, line) in reader.lines().enumerate() {
    let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    if line.trim().is_empty() {
      continue;
    }
    summary
      .ingest_line(&line)
      .map_err(|err| format!("invalid pi jsonl at {}:{}: {err}", path.display(), index + 1))?;
  }

  Ok(summary)
}

fn pi_user_preview(message: PiMessage) -> Option<String> {
  let PiMessage::User(message) = message else {
    return None;
  };
  match message.content {
    PiUserContent::Text(text) => non_blank(text),
    PiUserContent::Blocks(blocks) => non_blank(
      blocks
        .into_iter()
        .filter_map(|block| match block {
          PiContentBlock::Text(text) => text.text,
          _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"),
    ),
    PiUserContent::Missing | PiUserContent::Unknown(_) => None,
  }
}

fn non_blank(value: String) -> Option<String> {
  let value = value.trim();
  (!value.is_empty()).then(|| value.to_string())
}

fn inspect_session_header(path: &Path) -> Result<SessionRef, String> {
  let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  let reader = BufReader::new(file);
  let mut reference = SessionRef {
    model_baseline: None,
    id: session_id_from_path(path),
    parent_session_id: None,
    agent_path: None,
    agent_nickname: None,
    agent_role: None,
    title: None,
    preview: None,
    path: path.to_path_buf(),
    cwd: None,
    timestamp: None,
    message_count: 0,
  };

  for line in reader.lines() {
    let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    if line.trim().is_empty() {
      continue;
    }
    let value: serde_json::Value =
      serde_json::from_str(&line).map_err(|err| format!("invalid pi jsonl at {}: {err}", path.display()))?;
    if value.get("type").and_then(|value| value.as_str()) != Some("session") {
      continue;
    }

    if let Some(value) = value.get("id").and_then(|value| value.as_str()) {
      reference.id = value.to_string();
    }
    reference.parent_session_id = value
      .get("parentSession")
      .and_then(|value| value.as_str())
      .map(|parent| resolve_parent_session_id(path, parent));
    reference.cwd = value.get("cwd").and_then(|value| value.as_str()).map(str::to_string);
    reference.timestamp = value
      .get("timestamp")
      .and_then(|value| value.as_str())
      .map(str::to_string);
    break;
  }

  Ok(reference)
}

fn resolve_parent_session_id(session_path: &Path, parent: &str) -> String {
  let parent_path = PathBuf::from(parent);
  let parent_path = if parent_path.is_absolute() {
    parent_path
  } else {
    session_path
      .parent()
      .map(|directory| directory.join(&parent_path))
      .unwrap_or(parent_path)
  };

  let parent_id = File::open(&parent_path).ok().and_then(|file| {
    BufReader::new(file).lines().map_while(Result::ok).find_map(|line| {
      let value: serde_json::Value = serde_json::from_str(&line).ok()?;
      if value.get("type").and_then(|value| value.as_str()) != Some("session") {
        return None;
      }
      value.get("id").and_then(|value| value.as_str()).map(str::to_string)
    })
  });

  parent_id.unwrap_or_else(|| session_id_from_path(&parent_path))
}

fn session_id_from_path(path: &Path) -> String {
  path
    .file_stem()
    .and_then(|value| value.to_str())
    .and_then(|stem| stem.rsplit_once('_').map(|(_, id)| id.to_string()))
    .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn resolves_parent_session_path_to_parent_id() {
    let reference = inspect_session(&fixtures_dir().join("tree_child.jsonl")).expect("fixture should be inspectable");

    assert_eq!(reference.id, "pi-tree-child");
    assert_eq!(reference.parent_session_id.as_deref(), Some("pi-tree-root"));
  }

  #[test]
  fn relation_scan_reads_parent_without_counting_the_body() {
    let source = PiSessionSource::new(Some(fixtures_dir()));
    let references = source.list_session_relations().expect("fixture relations should load");
    let reference = references
      .iter()
      .find(|reference| reference.id == "pi-tree-child")
      .expect("tree child should be discovered");

    assert_eq!(reference.parent_session_id.as_deref(), Some("pi-tree-root"));
    assert_eq!(reference.message_count, 0);
    assert_eq!(reference.title, None);
    assert_eq!(reference.preview, None);
  }

  #[test]
  fn summary_uses_latest_name_and_first_meaningful_user_text() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    let records = [
      serde_json::json!({"type":"session","id":"summary"}),
      serde_json::json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"not the preview"}]}}),
      serde_json::json!({"type":"message","message":{"role":"user","content":"   "}}),
      serde_json::json!({"type":"session_info","name":"Earlier title"}),
      serde_json::json!({"type":"message","message":{"role":"user","content":[
        {"type":"image","mimeType":"image/png","data":"opaque"},
        {"type":"text","text":"  first line  "},
        {"type":"text","text":"second line"}
      ]}}),
      serde_json::json!({"type":"session_info","name":"  Latest title  "}),
      serde_json::json!({"type":"message","message":{"role":"user","content":"later prompt"}}),
    ];
    std::fs::write(
      &path,
      records
        .into_iter()
        .map(|record| record.to_string())
        .collect::<Vec<_>>()
        .join("\n"),
    )
    .unwrap();

    let summary = inspect_session_summary(&path).unwrap();

    assert_eq!(summary.title.as_deref(), Some("Latest title"));
    assert_eq!(summary.preview.as_deref(), Some("first line  \nsecond line"));
    assert_eq!(summary.message_count, 4);
  }

  #[test]
  fn latest_blank_session_info_explicitly_clears_name() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    std::fs::write(
      &path,
      [
        serde_json::json!({"type":"session","id":"summary"}),
        serde_json::json!({"type":"session_info","name":"Named"}),
        serde_json::json!({"type":"session_info","name":" \n "}),
      ]
      .into_iter()
      .map(|record| record.to_string())
      .collect::<Vec<_>>()
      .join("\n"),
    )
    .unwrap();

    assert_eq!(inspect_session_summary(&path).unwrap().title, None);
  }

  #[test]
  fn explicit_session_directory_has_highest_precedence_and_is_unchanged() {
    let explicit = Path::new("~/explicit-sessions");

    let resolved = resolve_pi_session_root(
      Some(explicit),
      Some(OsStr::new("environment-sessions")),
      Some(OsStr::new("environment-agent")),
      Some(Path::new("platform-home")),
      false,
    )
    .expect("explicit session directory should resolve");

    assert_eq!(resolved, explicit);
  }

  #[test]
  fn session_environment_override_precedes_agent_directory() {
    let resolved = resolve_pi_session_root(
      None,
      Some(OsStr::new("environment-sessions")),
      Some(OsStr::new("environment-agent")),
      Some(Path::new("platform-home")),
      false,
    )
    .expect("session environment override should resolve");

    assert_eq!(resolved, Path::new("environment-sessions"));
  }

  #[test]
  fn empty_session_override_falls_back_to_agent_directory() {
    let resolved = resolve_pi_session_root(
      None,
      Some(OsStr::new("")),
      Some(OsStr::new("environment-agent")),
      Some(Path::new("platform-home")),
      false,
    )
    .expect("agent directory should resolve");

    assert_eq!(resolved, Path::new("environment-agent").join("sessions"));
  }

  #[test]
  fn empty_environment_overrides_use_cross_platform_home_input() {
    // At runtime dirs::home_dir supplies this value from the Windows profile
    // known folder, rather than this crate depending on HOME being present.
    let user_profile = Path::new(r"C:\Users\Alice");
    let resolved = resolve_pi_session_root(
      None,
      Some(OsStr::new("")),
      Some(OsStr::new("")),
      Some(user_profile),
      true,
    )
    .expect("platform home should resolve");

    assert_eq!(resolved, user_profile.join(".pi").join("agent").join("sessions"));
  }

  #[test]
  fn environment_overrides_expand_tilde_with_platform_home() {
    let platform_home = Path::new("platform-home");
    let session_root = resolve_pi_session_root(
      None,
      Some(OsStr::new("~/custom-sessions")),
      None,
      Some(platform_home),
      false,
    )
    .expect("session override should expand");
    let agent_root = resolve_pi_session_root(
      None,
      None,
      Some(OsStr::new("~/custom-agent")),
      Some(platform_home),
      false,
    )
    .expect("agent override should expand");

    assert_eq!(session_root, platform_home.join("custom-sessions"));
    assert_eq!(agent_root, platform_home.join("custom-agent").join("sessions"));
  }

  #[test]
  fn windows_tilde_separator_uses_platform_home() {
    let platform_home = Path::new(r"C:\Users\Alice");

    let resolved = resolve_pi_session_root(
      None,
      Some(OsStr::new(r"~\custom-sessions")),
      None,
      Some(platform_home),
      true,
    )
    .expect("Windows tilde path should expand");

    assert_eq!(resolved, platform_home.join("custom-sessions"));
  }

  #[test]
  fn missing_platform_home_has_actionable_errors() {
    let default_error =
      resolve_pi_session_root(None, None, None, None, false).expect_err("default discovery requires a platform home");
    let tilde_error = resolve_pi_session_root(None, Some(OsStr::new("~/custom-sessions")), None, None, false)
      .expect_err("tilde expansion requires a platform home");

    assert!(default_error.contains("PI_CODING_AGENT_SESSION_DIR"));
    assert!(default_error.contains("--session-dir"));
    assert!(tilde_error.contains("PI_CODING_AGENT_SESSION_DIR"));
    assert!(tilde_error.contains("--session-dir"));
  }

  fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
  }
}
