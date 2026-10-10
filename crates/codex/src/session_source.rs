use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::event::CodexLine;
use crate::normalize::{CodexHistoryBoundary, CodexNormalizer};
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde::Deserialize;
use tokn_codex_protocol::{ContentItem, ResponseItem, RolloutItem};
use tokn_session_core::{LoadedSession, SessionHeader, SessionHistoryStatus, SessionRef};

mod catalog;

const STATE_DB_FILENAME: &str = "state_5.sqlite";
const SESSION_INDEX_FILENAME: &str = "session_index.jsonl";
const PREVIEW_SCAN_LIMIT: usize = 210;
const SQLITE_ID_BATCH_SIZE: usize = 500;
const USER_MESSAGE_BEGIN: &str = "## My request for Codex:";

pub struct CodexSessionSource {
  session_dir: Option<PathBuf>,
}

impl CodexSessionSource {
  pub fn new(session_dir: Option<PathBuf>) -> Self {
    Self { session_dir }
  }

  pub fn list_sessions(&self) -> Result<Vec<SessionRef>, String> {
    let mut references = self.list_session_refs(inspect_session)?;
    self.apply_indexed_metadata(&mut references);
    Ok(references)
  }

  pub fn list_session_relations(&self) -> Result<Vec<SessionRef>, String> {
    let mut references = self.list_session_refs(inspect_session_header)?;
    self.apply_indexed_metadata(&mut references);
    Ok(references)
  }

  /// Returns the session directory roots that this source would discover.
  ///
  /// Consumers that maintain a file watcher should use these effective roots
  /// instead of recreating Codex's environment and archive-directory
  /// resolution themselves.
  pub fn session_roots(&self) -> Result<Vec<PathBuf>, String> {
    self.roots()
  }

  /// Enumerates current rollout paths without scanning their bodies.
  ///
  /// Durable indexes use this to reconcile filesystem membership cheaply and
  /// only reopen headers whose file revision changed. Duplicate native paths
  /// are resolved against Desktop's catalog and the selected owning header.
  pub fn session_paths(&self) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for root in self.session_roots()? {
      collect_jsonl_files(&root, &mut paths)?;
    }
    paths.sort();
    if let Ok(home) = default_codex_home()
      && owns_metadata_root(self.session_dir.as_deref(), &home)
    {
      catalog::retain_current_rollouts(&home, &mut paths);
    }
    Ok(paths)
  }

  /// Applies Codex Desktop's optional title and preview metadata to an
  /// already-discovered header inventory without reopening rollout files.
  pub fn apply_catalog_metadata(&self, headers: &mut [SessionHeader]) {
    let mut references = headers
      .iter()
      .map(|header| SessionRef {
        id: header.id.clone(),
        parent_session_id: header.parent_session_id.clone(),
        agent_path: header.agent_path.clone(),
        agent_nickname: header.agent_nickname.clone(),
        agent_role: header.agent_role.clone(),
        title: header.title.clone(),
        preview: header.preview.clone(),
        path: header.path.clone(),
        cwd: header.cwd.clone(),
        timestamp: header.timestamp.clone(),
        message_count: 0,
      })
      .collect::<Vec<_>>();
    self.apply_indexed_metadata(&mut references);
    for (header, reference) in headers.iter_mut().zip(references) {
      header.title = reference.title;
      header.preview = reference.preview;
    }
  }

  /// Reads the header relation for one known rollout path without scanning its
  /// conversation body or sibling session files.
  ///
  /// This applies the same optional Codex Desktop metadata as bulk relation
  /// discovery. Callers that need a complete catalog-compatible presentation
  /// should use this method; high-frequency file watchers should use
  /// [`Self::session_relation_at_path_for_incremental_catalog`] instead.
  pub fn session_relation_at_path(&self, path: &Path) -> Result<SessionRef, String> {
    let mut reference = inspect_session_header(path)?;
    self.apply_indexed_metadata(std::slice::from_mut(&mut reference));
    Ok(reference)
  }

  /// Reads one rollout's native header for an incremental catalog update.
  ///
  /// Unlike [`Self::session_relation_at_path`], this never opens Codex
  /// Desktop's optional state database or legacy session index. An active
  /// rollout can receive many append notifications, so that external metadata
  /// belongs to the infrequent complete catalog path rather than the append
  /// hot path.
  pub fn session_relation_at_path_for_incremental_catalog(&self, path: &Path) -> Result<SessionRef, String> {
    inspect_session_header(path)
  }

  /// Resolves prompt-derived metadata for one already-discovered session.
  ///
  /// Bulk listing deliberately does not scan rollout bodies. Callers can use
  /// this for a visible or search-relevant header and cache the result.
  pub fn hydrate_session_header(&self, mut header: SessionHeader) -> Result<SessionHeader, String> {
    if header.preview.is_none() {
      header.preview = inspect_session_preview(&header.path)?;
    }
    Ok(header)
  }

  fn list_session_refs(&self, inspect: fn(&Path) -> Result<SessionRef, String>) -> Result<Vec<SessionRef>, String> {
    let mut refs = Vec::new();
    for path in self.session_paths()? {
      if let Ok(reference) = inspect(&path) {
        refs.push(reference);
      }
    }
    catalog::retain_lineage_heads(self, &mut refs);
    refs.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.path.cmp(&a.path)));
    Ok(refs)
  }

  pub fn load_session(&self, id_or_path: &str) -> Result<LoadedSession, String> {
    let path = self.resolve_session(id_or_path)?;
    self.load_session_path(&path)
  }

  pub fn load_session_path(&self, path: &Path) -> Result<LoadedSession, String> {
    let header = crate::history::history_header(path)?;
    let RolloutItem::SessionMeta(meta) = header.item() else {
      unreachable!("history header validates session metadata")
    };
    if meta
      .history_base()
      .map_err(|err| format!("invalid Codex history base: {err}"))?
      .is_some()
    {
      return self
        .load_session_records_path(path, false, 128 * 1024 * 1024)
        .map(Into::into);
    }
    let mut reference = inspect_session(path)?;
    self.apply_indexed_metadata(std::slice::from_mut(&mut reference));
    let file = File::open(&path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let reader = BufReader::new(file);
    let mut normalizer = CodexNormalizer::new_historical();
    let mut events = Vec::new();

    for (index, line) in reader.lines().enumerate() {
      let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
      if line.trim().is_empty() {
        continue;
      }
      let event: CodexLine = serde_json::from_str(&line)
        .map_err(|err| format!("invalid codex jsonl at {}:{}: {err}", path.display(), index + 1))?;
      events.extend(normalizer.normalize(event));
    }

    Ok(LoadedSession {
      reference,
      events,
      history_status: normalizer.history_status(),
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
      [] => Err(format!("no codex session found for `{id_or_path}`")),
      _ => Err(format!("multiple codex sessions match `{id_or_path}`")),
    }
  }

  fn roots(&self) -> Result<Vec<PathBuf>, String> {
    if let Some(root) = &self.session_dir {
      return Ok(vec![root.clone()]);
    }

    let codex_home = default_codex_home()?;
    Ok(vec![codex_home.join("sessions"), codex_home.join("archived_sessions")])
  }

  pub(crate) fn apply_indexed_metadata(&self, references: &mut [SessionRef]) {
    if references.is_empty() {
      return;
    }

    let Some(codex_home) = default_codex_home().ok() else {
      return;
    };
    // Relay supplies resolved roots explicitly. Those still belong to this
    // home's metadata, but arbitrary custom directories must remain isolated.
    if !owns_metadata_root(self.session_dir.as_deref(), &codex_home) {
      return;
    }
    let metadata = indexed_metadata(&codex_home, references);
    for reference in references {
      let Some(native) = metadata.get(&reference.id) else {
        continue;
      };
      apply_indexed_metadata_to_reference(reference, native);
    }
  }

  pub(crate) fn history_roots(&self, path: &Path) -> Result<Vec<PathBuf>, String> {
    if let Ok(home) = default_codex_home()
      && owns_metadata_root(self.session_dir.as_deref(), &home)
    {
      let roots = vec![home.join("sessions"), home.join("archived_sessions")];
      let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
      if roots
        .iter()
        .any(|root| root.canonicalize().is_ok_and(|root| canonical.starts_with(root)))
      {
        return Ok(roots);
      }
    }
    if let Some(root) = &self.session_dir {
      return Ok(vec![root.clone()]);
    }
    Ok(vec![path.parent().unwrap_or(Path::new(".")).to_path_buf()])
  }
}

fn owns_metadata_root(session_dir: Option<&Path>, codex_home: &Path) -> bool {
  let Some(root) = session_dir else {
    return true;
  };
  let Ok(root) = root.canonicalize() else {
    return false;
  };
  ["sessions", "archived_sessions"].into_iter().any(|directory| {
    codex_home
      .join(directory)
      .canonicalize()
      .is_ok_and(|owned| owned == root)
  })
}

fn default_codex_home() -> Result<PathBuf, String> {
  let configured_home = std::env::var_os("CODEX_HOME");
  let platform_home = dirs::home_dir();
  resolve_codex_home(configured_home.as_deref(), platform_home.as_deref())
}

fn resolve_codex_home(configured_home: Option<&OsStr>, platform_home: Option<&Path>) -> Result<PathBuf, String> {
  if let Some(configured_home) = configured_home.filter(|value| !value.is_empty()) {
    let path = PathBuf::from(configured_home);
    let metadata = std::fs::metadata(&path).map_err(|err| {
      if err.kind() == std::io::ErrorKind::NotFound {
        format!(
          "CODEX_HOME points to `{}`, but that path does not exist; create the directory, set CODEX_HOME to a valid Codex home, or pass --session-dir",
          path.display()
        )
      } else {
        format!("failed to inspect CODEX_HOME `{}`: {err}", path.display())
      }
    })?;

    if !metadata.is_dir() {
      return Err(format!(
        "CODEX_HOME points to `{}`, but that path is not a directory; set CODEX_HOME to a valid Codex home or pass --session-dir",
        path.display()
      ));
    }

    return path
      .canonicalize()
      .map_err(|err| format!("failed to canonicalize CODEX_HOME `{}`: {err}", path.display()));
  }

  let platform_home = platform_home
    .filter(|path| !path.as_os_str().is_empty())
    .ok_or_else(|| "could not determine the user home directory; set CODEX_HOME or pass --session-dir".to_string())?;
  Ok(platform_home.join(".codex"))
}

#[derive(Debug, Default)]
struct IndexedSessionMetadata {
  name: Option<String>,
  legacy_name: Option<String>,
  title: Option<String>,
  preview: Option<String>,
  rollout_path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct CodexConfigFile {
  sqlite_home: Option<String>,
}

#[derive(Deserialize)]
struct SessionIndexEntry {
  id: String,
  thread_name: String,
}

fn indexed_metadata(codex_home: &Path, references: &[SessionRef]) -> HashMap<String, IndexedSessionMetadata> {
  let ids: HashSet<_> = references.iter().map(|reference| reference.id.as_str()).collect();
  let paths_by_id: HashMap<_, Vec<_>> = references.iter().fold(HashMap::new(), |mut paths, reference| {
    paths
      .entry(reference.id.as_str())
      .or_insert_with(Vec::new)
      .push(reference.path.as_path());
    paths
  });
  let mut metadata = read_state_metadata(codex_home, &paths_by_id).unwrap_or_default();

  // `session_index.jsonl` predates the state database. It remains a useful
  // fallback for explicit names that have not been migrated into `threads`.
  let mut legacy_names = HashMap::new();
  if let Ok(file) = File::open(codex_home.join(SESSION_INDEX_FILENAME)) {
    for line in BufReader::new(file).lines().map_while(Result::ok) {
      let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(&line) else {
        continue;
      };
      if !ids.contains(entry.id.as_str()) {
        continue;
      }
      let Some(name) = clean_discovery_text(&entry.thread_name) else {
        continue;
      };
      // The index is append-only, so a later valid row replaces an earlier
      // name for the same thread.
      legacy_names.insert(entry.id, name);
    }
  }
  for (id, name) in legacy_names {
    metadata.entry(id).or_default().legacy_name = Some(name);
  }

  metadata
}

fn read_state_metadata(
  codex_home: &Path,
  paths_by_id: &HashMap<&str, Vec<&Path>>,
) -> rusqlite::Result<HashMap<String, IndexedSessionMetadata>> {
  let sqlite_home = resolve_sqlite_home(codex_home);
  let database_path = sqlite_home.join(STATE_DB_FILENAME);
  if !database_path.is_file() {
    return Ok(HashMap::new());
  }

  // This is an intentionally private, optional adapter. Read-only flags and
  // schema capability checks ensure the viewer never creates or migrates a
  // Codex database and simply falls back when the private schema changes.
  let connection = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
  let columns = sqlite_columns(&connection, "threads")?;
  if !columns.contains("id")
    || !columns.contains("rollout_path")
    || !["name", "title", "preview", "first_user_message"]
      .iter()
      .any(|column| columns.contains(*column))
  {
    return Ok(HashMap::new());
  }

  let mut metadata = HashMap::new();
  let ids: Vec<_> = paths_by_id.keys().copied().collect();
  for chunk in ids.chunks(SQLITE_ID_BATCH_SIZE) {
    let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
    let select = ["name", "title", "preview", "first_user_message", "rollout_path"]
      .map(|column| {
        if columns.contains(column) {
          column.to_string()
        } else {
          format!("null as {column}")
        }
      })
      .join(", ");
    let query = format!("select id, {select} from threads where id in ({placeholders})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, Option<String>>(1)?,
        row.get::<_, Option<String>>(2)?,
        row.get::<_, Option<String>>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, Option<String>>(5)?,
      ))
    })?;

    for row in rows {
      let (id, name, title, preview, first_user_message, rollout_path) = row?;
      let Some(reference_paths) = paths_by_id.get(id.as_str()) else {
        continue;
      };
      let Some(rollout_path) = rollout_path.as_deref().and_then(non_blank).map(PathBuf::from) else {
        continue;
      };
      if !reference_paths
        .iter()
        .any(|reference_path| paths_refer_to_same_file(&rollout_path, reference_path))
      {
        continue;
      }

      metadata.insert(
        id,
        IndexedSessionMetadata {
          name: name.as_deref().and_then(clean_discovery_text),
          legacy_name: None,
          title: title.as_deref().and_then(clean_discovery_text),
          preview: preview
            .as_deref()
            .and_then(clean_discovery_text)
            .or_else(|| first_user_message.as_deref().and_then(clean_discovery_text)),
          rollout_path: Some(rollout_path),
        },
      );
    }
  }

  Ok(metadata)
}

fn apply_indexed_metadata_to_reference(reference: &mut SessionRef, metadata: &IndexedSessionMetadata) {
  // Codex Desktop currently carries a parent thread's title, preview, and
  // first user message into rows it creates for subagents. Those fields do not
  // identify the child's work and make a session tree look like duplicated
  // root conversations. The rollout header already gives us an explicit
  // parent relationship and agent identity, so keep child presentation empty
  // and let consumers use that identity instead.
  if reference.parent_session_id.is_some() {
    return;
  }
  let state_matches = metadata
    .rollout_path
    .as_deref()
    .is_some_and(|path| paths_refer_to_same_file(path, &reference.path));
  reference.title = if state_matches {
    metadata
      .name
      .clone()
      .or_else(|| metadata.legacy_name.clone())
      .or_else(|| metadata.title.clone())
  } else {
    metadata.legacy_name.clone()
  };
  if state_matches {
    reference.preview = metadata.preview.clone().or_else(|| reference.preview.clone());
  }
}

fn sqlite_columns(connection: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
  let mut statement = connection.prepare("select name from pragma_table_info(?1)")?;
  statement
    .query_map([table], |row| row.get(0))?
    .collect::<rusqlite::Result<HashSet<_>>>()
}

fn resolve_sqlite_home(codex_home: &Path) -> PathBuf {
  let configured = std::fs::read_to_string(codex_home.join("config.toml"))
    .ok()
    .and_then(|contents| toml::from_str::<CodexConfigFile>(&contents).ok())
    .and_then(|config| config.sqlite_home)
    .and_then(|value| non_blank(&value).map(str::to_string));
  if let Some(configured) = configured {
    return resolve_metadata_path(OsStr::new(&configured), codex_home);
  }

  if let Some(configured) = std::env::var_os("CODEX_SQLITE_HOME").filter(|value| !value.is_empty()) {
    let base = std::env::current_dir().unwrap_or_else(|_| codex_home.to_path_buf());
    return resolve_metadata_path(&configured, &base);
  }

  codex_home.to_path_buf()
}

fn resolve_metadata_path(value: &OsStr, base: &Path) -> PathBuf {
  if let Some(value) = value.to_str()
    && let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix(r"~\"))
    && let Some(home) = dirs::home_dir()
  {
    return home.join(rest);
  }
  if value == OsStr::new("~")
    && let Some(home) = dirs::home_dir()
  {
    return home;
  }

  let path = PathBuf::from(value);
  if path.is_absolute() { path } else { base.join(path) }
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
  if left == right {
    return true;
  }
  match (left.canonicalize(), right.canonicalize()) {
    (Ok(left), Ok(right)) => left == right,
    _ => false,
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
  let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  let reader = BufReader::new(file);
  let mut message_count = 0;
  let mut history_boundary = CodexHistoryBoundary::new();
  let mut saw_session_meta = false;

  for line in reader.lines() {
    let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    if line.trim().is_empty() {
      continue;
    }
    let parsed_line: CodexLine =
      serde_json::from_str(&line).map_err(|err| format!("invalid codex jsonl at {}: {err}", path.display()))?;
    let value = parsed_line.native();
    let counts_as_message = counts_as_display_message(value);

    let accepted = history_boundary.accepts(parsed_line.item());
    saw_session_meta |= matches!(parsed_line.item(), RolloutItem::SessionMeta(_));
    if accepted && saw_session_meta && reference.preview.is_none() {
      reference.preview = preview_from_item(parsed_line.item(), history_boundary.status());
    }
    if history_boundary.status() == SessionHistoryStatus::SubagentBodyUnavailable {
      message_count = 0;
    }
    if accepted && counts_as_message {
      message_count += 1;
    }
  }

  reference.message_count = message_count;
  Ok(reference)
}

pub(crate) fn inspect_session_header(path: &Path) -> Result<SessionRef, String> {
  let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  let reader = BufReader::new(file);
  let mut reference = SessionRef {
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
    let parsed_line: CodexLine =
      serde_json::from_str(&line).map_err(|err| format!("invalid codex jsonl at {}: {err}", path.display()))?;
    let value = parsed_line.native();
    if value.get("type").and_then(|value| value.as_str()) != Some("session_meta") {
      continue;
    }
    let Some(payload) = value.get("payload") else {
      break;
    };

    if let Some(value) = payload.get("id").and_then(|value| value.as_str()) {
      reference.id = value.to_string();
    }
    // A user fork is a new root session. Only Codex's explicit parent-thread
    // relationship identifies a session that belongs in the subagent tree.
    reference.parent_session_id = string_field(payload, "parent_thread_id");
    let thread_spawn = payload
      .get("source")
      .and_then(|source| source.get("subagent"))
      .and_then(|subagent| subagent.get("thread_spawn"));
    reference.agent_path = string_field(payload, "agent_path")
      .or_else(|| thread_spawn.and_then(|thread_spawn| string_field(thread_spawn, "agent_path")));
    reference.agent_nickname = string_field(payload, "agent_nickname")
      .or_else(|| thread_spawn.and_then(|thread_spawn| string_field(thread_spawn, "agent_nickname")));
    reference.agent_role = first_string_field(payload, &["agent_role", "agent_type"]).or_else(|| {
      thread_spawn.and_then(|thread_spawn| first_string_field(thread_spawn, &["agent_role", "agent_type"]))
    });
    reference.cwd = string_field(payload, "cwd");
    reference.timestamp = string_field(payload, "timestamp").or_else(|| {
      value
        .get("timestamp")
        .and_then(|value| value.as_str())
        .map(str::to_string)
    });
    break;
  }

  Ok(reference)
}

fn inspect_session_preview(path: &Path) -> Result<Option<String>, String> {
  let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  let reader = BufReader::new(file);
  let mut boundary = CodexHistoryBoundary::new();
  let mut saw_session_meta = false;
  let mut scanned = 0;

  for line in reader.lines() {
    let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    if line.trim().is_empty() {
      continue;
    }
    scanned += 1;
    if scanned > PREVIEW_SCAN_LIMIT {
      break;
    }

    // Discovery is best effort: a future or partially-written record should
    // not hide a later stable user-message record within the bounded scan.
    let Ok(parsed_line) = serde_json::from_str::<CodexLine>(&line) else {
      continue;
    };
    let accepted = boundary.accepts(parsed_line.item());
    saw_session_meta |= matches!(parsed_line.item(), RolloutItem::SessionMeta(_));
    if !accepted || !saw_session_meta {
      continue;
    }
    if let Some(preview) = preview_from_item(parsed_line.item(), boundary.status()) {
      return Ok(Some(preview));
    }
  }

  Ok(None)
}

fn preview_from_item(item: &RolloutItem, history_status: SessionHistoryStatus) -> Option<String> {
  match item {
    RolloutItem::EventMessage(event) => event_message_preview(&event.native),
    RolloutItem::ResponseItem(ResponseItem::Message(message)) if message.role.as_deref() == Some("user") => {
      content_preview(&message.content)
    }
    RolloutItem::ResponseItem(ResponseItem::AgentMessage(message))
      if history_status == SessionHistoryStatus::FilteredSubagent =>
    {
      content_preview(&message.content)
    }
    RolloutItem::InterAgentCommunication(message)
      if history_status == SessionHistoryStatus::FilteredSubagent && message.trigger_turn == Some(true) =>
    {
      message.content.as_deref().and_then(clean_user_prompt)
    }
    _ => None,
  }
}

fn event_message_preview(event: &serde_json::Value) -> Option<String> {
  match event.get("type").and_then(serde_json::Value::as_str) {
    Some("user_message") => event
      .get("message")
      .and_then(serde_json::Value::as_str)
      .and_then(clean_user_prompt)
      .or_else(|| {
        if has_non_empty_array(event, "images") || has_non_empty_array(event, "local_images") {
          Some("[Image]".to_string())
        } else if has_non_empty_array(event, "audio") || has_non_empty_array(event, "local_audio") {
          Some("[Audio]".to_string())
        } else {
          None
        }
      }),
    Some("item_completed") => event.get("item").and_then(completed_user_message_preview),
    _ => None,
  }
}

fn completed_user_message_preview(item: &serde_json::Value) -> Option<String> {
  let item_type = item.get("type").and_then(serde_json::Value::as_str)?;
  if !matches!(item_type, "UserMessage" | "user_message") {
    return None;
  }
  let content = item.get("content").and_then(serde_json::Value::as_array)?;
  let text = content
    .iter()
    .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
    .collect::<Vec<_>>()
    .join(" ");
  clean_user_prompt(&text).or_else(|| {
    if content.iter().any(|part| {
      matches!(
        part.get("type").and_then(serde_json::Value::as_str),
        Some("image" | "local_image")
      )
    }) {
      Some("[Image]".to_string())
    } else if content.iter().any(|part| {
      matches!(
        part.get("type").and_then(serde_json::Value::as_str),
        Some("audio" | "local_audio")
      )
    }) {
      Some("[Audio]".to_string())
    } else {
      None
    }
  })
}

fn content_preview(content: &[ContentItem]) -> Option<String> {
  let text = content
    .iter()
    .filter_map(|part| part.text.as_deref())
    .collect::<Vec<_>>()
    .join(" ");
  clean_user_prompt(&text)
}

fn has_non_empty_array(value: &serde_json::Value, field: &str) -> bool {
  value
    .get(field)
    .and_then(serde_json::Value::as_array)
    .is_some_and(|values| !values.is_empty())
}

fn clean_user_prompt(value: &str) -> Option<String> {
  let value = value
    .find(USER_MESSAGE_BEGIN)
    .map(|index| &value[index + USER_MESSAGE_BEGIN.len()..])
    .unwrap_or(value);
  clean_discovery_text(value)
}

fn clean_discovery_text(value: &str) -> Option<String> {
  non_blank(value).map(str::to_string)
}

fn non_blank(value: &str) -> Option<&str> {
  let value = value.trim();
  (!value.is_empty()).then_some(value)
}

fn counts_as_display_message(value: &serde_json::Value) -> bool {
  match value.get("type").and_then(|value| value.as_str()) {
    Some("response_item") => value.get("payload").is_some_and(|payload| {
      payload.get("type").and_then(|value| value.as_str()) == Some("message")
        && payload.get("role").and_then(|value| value.as_str()) == Some("assistant")
    }),
    Some("event_msg") => {
      value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(|value| value.as_str())
        == Some("user_message")
    }
    _ => false,
  }
}

fn string_field(value: &serde_json::Value, field: &str) -> Option<String> {
  value.get(field).and_then(|value| value.as_str()).map(str::to_string)
}

fn first_string_field(value: &serde_json::Value, fields: &[&str]) -> Option<String> {
  fields.iter().find_map(|field| string_field(value, field))
}

fn session_id_from_path(path: &Path) -> String {
  path
    .file_stem()
    .and_then(|value| value.to_str())
    .and_then(|stem| stem.rsplit_once('-').map(|(_, id)| id.to_string()))
    .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use rusqlite::params;
  use tempfile::tempdir;
  use tokn_session_core::AgentEvent;

  #[test]
  fn first_session_meta_owns_subagent_identity() {
    let reference = inspect_session(&fixtures_dir().join("tree_child.jsonl")).expect("fixture should be inspectable");

    assert_eq!(reference.id, "tree-child");
    assert_eq!(reference.parent_session_id.as_deref(), Some("tree-root"));
    assert_eq!(reference.agent_path.as_deref(), Some("/root/researcher"));
    assert_eq!(reference.agent_nickname.as_deref(), Some("Hubble"));
    assert_eq!(reference.agent_role.as_deref(), Some("explorer"));
    assert_eq!(reference.timestamp.as_deref(), Some("2026-07-29T00:01:00Z"));
    assert_eq!(reference.preview.as_deref(), Some("investigate this"));
    assert_eq!(reference.message_count, 1);
  }

  #[test]
  fn treats_user_forks_as_root_sessions() {
    let reference = inspect_session(&fixtures_dir().join("forked_session.jsonl")).expect("fork should be inspectable");

    assert_eq!(reference.id, "forked-session");
    assert_eq!(reference.parent_session_id, None);
    assert_eq!(reference.agent_path, None);
    assert_eq!(reference.message_count, 2);
  }

  #[test]
  fn relation_scan_reads_identity_without_counting_the_body() {
    let source = CodexSessionSource::new(Some(fixtures_dir()));
    let references = source.list_session_relations().expect("fixture relations should load");
    let reference = references
      .iter()
      .find(|reference| reference.id == "tree-child")
      .expect("tree child should be discovered");

    assert_eq!(reference.parent_session_id.as_deref(), Some("tree-root"));
    assert_eq!(reference.agent_path.as_deref(), Some("/root/researcher"));
    assert_eq!(reference.preview, None);
    assert_eq!(reference.message_count, 0);
  }

  #[test]
  fn hydrates_one_root_preview_without_bulk_relation_scans() {
    let source = CodexSessionSource::new(Some(fixtures_dir()));
    let path = fixtures_dir().join("basic_session.jsonl");
    let header = header_from_reference(inspect_session_header(&path).expect("header should load"));

    let hydrated = source.hydrate_session_header(header).expect("preview should hydrate");

    assert_eq!(hydrated.preview.as_deref(), Some("build a tiny test"));
  }

  #[test]
  fn hydrates_paginated_user_item_preview() {
    let source = CodexSessionSource::new(Some(fixtures_dir()));
    let path = fixtures_dir().join("item_lifecycle_session.jsonl");
    let header = header_from_reference(inspect_session_header(&path).expect("header should load"));

    let hydrated = source.hydrate_session_header(header).expect("preview should hydrate");

    // Provider adapters retain meaningful internal whitespace. Presentation
    // clients apply their own single-line sanitization consistently across
    // providers.
    assert_eq!(hydrated.preview.as_deref(), Some("hello  world"));
  }

  #[test]
  fn hydration_uses_owned_subagent_prompt_after_history_boundary() {
    let source = CodexSessionSource::new(Some(fixtures_dir()));
    let path = fixtures_dir().join("tree_child.jsonl");
    let header = header_from_reference(inspect_session_header(&path).expect("header should load"));

    let hydrated = source.hydrate_session_header(header).expect("preview should hydrate");

    assert_eq!(hydrated.preview.as_deref(), Some("investigate this"));
  }

  #[test]
  fn hydration_never_uses_copied_parent_prompt_without_boundary() {
    let source = CodexSessionSource::new(Some(fixtures_dir()));
    let path = fixtures_dir().join("tree_child_no_boundary.jsonl");
    let header = header_from_reference(inspect_session_header(&path).expect("header should load"));

    let hydrated = source
      .hydrate_session_header(header)
      .expect("bounded hydration should succeed");

    assert_eq!(hydrated.preview, None);
  }

  #[test]
  fn reads_private_state_metadata_read_only_and_checks_rollout_path() {
    let directory = tempdir().expect("temporary directory should be created");
    let codex_home = directory.path().join("codex-home");
    let sqlite_home = directory.path().join("state");
    std::fs::create_dir_all(&codex_home).expect("Codex home should be created");
    std::fs::create_dir_all(&sqlite_home).expect("SQLite home should be created");
    std::fs::write(
      codex_home.join("config.toml"),
      format!("sqlite_home = {:?}\n", sqlite_home.display().to_string()),
    )
    .expect("config should be written");

    let matched_rollout = directory.path().join("matched.jsonl");
    let duplicate_rollout = directory.path().join("matched-copy.jsonl");
    let stale_rollout = directory.path().join("stale.jsonl");
    let blank_rollout = directory.path().join("blank.jsonl");
    let foreign_rollout = directory.path().join("foreign.jsonl");
    for path in [
      &matched_rollout,
      &duplicate_rollout,
      &stale_rollout,
      &blank_rollout,
      &foreign_rollout,
    ] {
      std::fs::write(path, "").expect("rollout should be created");
    }

    let database = Connection::open(sqlite_home.join(STATE_DB_FILENAME)).expect("state database should open");
    database
      .execute_batch(
        "create table threads (
           id text primary key,
           name text,
           title text,
           preview text,
           first_user_message text,
           rollout_path text
         );",
      )
      .expect("threads schema should be created");
    database
      .execute(
        "insert into threads (id, name, title, preview, first_user_message, rollout_path)
         values (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
          "matched",
          "  Explicit   name  ",
          "Generated title",
          "  first\nrequest  ",
          "unused fallback",
          matched_rollout.to_string_lossy()
        ],
      )
      .expect("matching metadata should be inserted");
    database
      .execute(
        "insert into threads (id, name, title, preview, first_user_message, rollout_path)
         values (?1, null, ?2, ?3, null, ?4)",
        params![
          "stale",
          "Wrong title",
          "Wrong preview",
          foreign_rollout.to_string_lossy()
        ],
      )
      .expect("stale metadata should be inserted");
    database
      .execute(
        "insert into threads (id, name, title, preview, first_user_message, rollout_path)
         values (?1, null, ?2, ?3, null, ?4)",
        params!["blank", "Uncorrelated title", "Uncorrelated preview", "   "],
      )
      .expect("blank-path metadata should be inserted");
    drop(database);

    std::fs::write(
      codex_home.join(SESSION_INDEX_FILENAME),
      concat!(
        "{\"id\":\"legacy\",\"thread_name\":\"Old name\"}\n",
        "{\"id\":\"legacy\",\"thread_name\":\"Latest name\"}\n"
      ),
    )
    .expect("legacy index should be written");
    let legacy_rollout = directory.path().join("legacy.jsonl");
    std::fs::write(&legacy_rollout, "").expect("legacy rollout should be created");

    let references = vec![
      session_reference("matched", matched_rollout.clone()),
      session_reference("matched", duplicate_rollout),
      session_reference("stale", stale_rollout),
      session_reference("blank", blank_rollout),
      session_reference("legacy", legacy_rollout),
    ];
    let metadata = indexed_metadata(&codex_home, &references);

    let matched = metadata.get("matched").expect("matching state row should resolve");
    assert_eq!(matched.name.as_deref(), Some("Explicit   name"));
    assert_eq!(matched.title.as_deref(), Some("Generated title"));
    assert_eq!(matched.preview.as_deref(), Some("first\nrequest"));
    assert!(!metadata.contains_key("stale"));
    assert!(!metadata.contains_key("blank"));
    assert_eq!(
      metadata.get("legacy").and_then(|value| value.legacy_name.as_deref()),
      Some("Latest name")
    );

    let mut matching_reference = session_reference("matched", matched_rollout.clone());
    apply_indexed_metadata_to_reference(&mut matching_reference, matched);
    assert_eq!(matching_reference.title.as_deref(), Some("Explicit   name"));
    assert_eq!(matching_reference.preview.as_deref(), Some("first\nrequest"));

    let mut duplicate_reference = references[1].clone();
    apply_indexed_metadata_to_reference(&mut duplicate_reference, matched);
    assert_eq!(duplicate_reference.title, None);
    assert_eq!(duplicate_reference.preview, None);

    let mut child_reference = session_reference("matched", directory.path().join("child.jsonl"));
    child_reference.parent_session_id = Some("root".to_string());
    child_reference.agent_nickname = Some("Hubble".to_string());
    child_reference.path = matched_rollout;
    apply_indexed_metadata_to_reference(&mut child_reference, matched);
    assert_eq!(child_reference.title, None);
    assert_eq!(child_reference.preview, None);
    assert_eq!(child_reference.agent_nickname.as_deref(), Some("Hubble"));
  }

  #[test]
  fn loads_only_owned_subagent_history_after_the_trigger_boundary() {
    let source = CodexSessionSource::new(Some(fixtures_dir()));
    let loaded = source
      .load_session_path(&fixtures_dir().join("tree_child.jsonl"))
      .expect("child session should load from its selected path");

    assert_eq!(loaded.reference.id, "tree-child");
    assert_eq!(loaded.reference.parent_session_id.as_deref(), Some("tree-root"));
    assert_eq!(loaded.reference.message_count, 1);
    assert_eq!(loaded.history_status, SessionHistoryStatus::FilteredSubagent);
    let messages: Vec<_> = loaded
      .events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Message(event) => Some(event.text.as_str()),
        _ => None,
      })
      .collect();
    assert_eq!(messages, vec!["child result"]);
    assert!(loaded.events.iter().any(
      |event| matches!(event, AgentEvent::AgentActivity(event) if event.target_agent_path.as_deref() == Some("/root/researcher"))
    ));
  }

  #[test]
  fn reports_subagent_body_unavailable_without_a_trigger_boundary() {
    let source = CodexSessionSource::new(Some(fixtures_dir()));
    let loaded = source
      .load_session_path(&fixtures_dir().join("tree_child_no_boundary.jsonl"))
      .expect("incomplete child session should still load");

    assert_eq!(loaded.reference.message_count, 0);
    assert_eq!(loaded.history_status, SessionHistoryStatus::SubagentBodyUnavailable);
    assert_eq!(loaded.events.len(), 1);
    assert!(matches!(
      &loaded.events[0],
      AgentEvent::SessionStarted(event) if event.session_id == "tree-child-no-boundary"
    ));
  }

  #[test]
  fn keeps_non_thread_spawn_subagent_history_without_a_boundary() {
    let source = CodexSessionSource::new(Some(fixtures_dir()));
    let loaded = source
      .load_session_path(&fixtures_dir().join("tree_guardian.jsonl"))
      .expect("guardian session should load");

    assert_eq!(loaded.reference.parent_session_id.as_deref(), Some("guardian-parent"));
    assert_eq!(loaded.reference.message_count, 2);
    assert_eq!(loaded.history_status, SessionHistoryStatus::Complete);
    let messages: Vec<_> = loaded
      .events
      .iter()
      .filter_map(|event| match event {
        AgentEvent::Message(event) => Some(event.text.as_str()),
        _ => None,
      })
      .collect();
    assert_eq!(messages, vec!["review this action", "allow"]);
  }

  #[test]
  fn explicit_session_directory_bypasses_default_discovery() {
    let explicit = PathBuf::from("explicit-codex-session-directory");
    let source = CodexSessionSource::new(Some(explicit.clone()));

    assert_eq!(source.roots().expect("explicit root should resolve"), vec![explicit]);
  }

  #[test]
  fn resolved_roots_keep_metadata_but_custom_roots_do_not_inherit_it() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("codex-home");
    for name in ["sessions", "archived_sessions"] {
      let root = home.join(name);
      std::fs::create_dir_all(&root).unwrap();
      assert!(owns_metadata_root(Some(&root), &home));
      assert!(owns_metadata_root(Some(&root.join(".")), &home));
      let nested = root.join("nested");
      std::fs::create_dir(&nested).unwrap();
      assert!(
        !owns_metadata_root(Some(&nested), &home),
        "only exact owned roots opt in"
      );
    }
    let custom = directory.path().join("other-home/sessions");
    std::fs::create_dir_all(&custom).unwrap();
    assert!(!owns_metadata_root(Some(&custom), &home));
    assert!(!owns_metadata_root(Some(&directory.path().join("missing")), &home));
    assert!(owns_metadata_root(None, &home));
  }

  #[cfg(unix)]
  #[test]
  fn metadata_root_ownership_uses_filesystem_identity_not_path_spelling() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let alias = directory.path().join("session-alias");
    std::os::unix::fs::symlink(&sessions, &alias).unwrap();
    assert!(owns_metadata_root(Some(&alias), &home));
  }

  #[test]
  fn configured_codex_home_precedes_platform_home_and_is_canonicalized() {
    let configured = fixtures_dir();
    let platform_home = Path::new("platform-home-must-not-win");

    let resolved = resolve_codex_home(Some(configured.as_os_str()), Some(platform_home))
      .expect("fixture directory should be a valid CODEX_HOME");

    assert_eq!(
      resolved,
      configured.canonicalize().expect("fixtures should canonicalize")
    );
  }

  #[test]
  fn empty_codex_home_uses_cross_platform_home_input() {
    // This models the Windows profile path that dirs::home_dir obtains from the
    // platform known-folder API even when HOME is not set.
    let platform_home = Path::new(r"C:\Users\Alice");

    let resolved = resolve_codex_home(Some(OsStr::new("")), Some(platform_home))
      .expect("empty CODEX_HOME should be treated as unset");

    assert_eq!(resolved, platform_home.join(".codex"));
  }

  #[test]
  fn missing_configured_codex_home_is_actionable() {
    let missing = fixtures_dir().join("missing-codex-home");

    let error = resolve_codex_home(Some(missing.as_os_str()), Some(Path::new("unused-home")))
      .expect_err("missing CODEX_HOME should fail");

    assert!(error.contains("CODEX_HOME"));
    assert!(error.contains("does not exist"));
    assert!(error.contains("--session-dir"));
  }

  #[test]
  fn file_configured_as_codex_home_is_actionable() {
    let file = fixtures_dir().join("tree_child.jsonl");

    let error = resolve_codex_home(Some(file.as_os_str()), Some(Path::new("unused-home")))
      .expect_err("a file cannot be CODEX_HOME");

    assert!(error.contains("CODEX_HOME"));
    assert!(error.contains("not a directory"));
    assert!(error.contains("--session-dir"));
  }

  #[test]
  fn missing_platform_home_has_an_actionable_fallback() {
    let error = resolve_codex_home(None, None).expect_err("a home is required without CODEX_HOME");

    assert!(error.contains("CODEX_HOME"));
    assert!(error.contains("--session-dir"));
  }

  fn header_from_reference(reference: SessionRef) -> SessionHeader {
    SessionHeader {
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
      updated_at: None,
      updated_at_ms: None,
    }
  }

  fn session_reference(id: &str, path: PathBuf) -> SessionRef {
    SessionRef {
      id: id.to_string(),
      parent_session_id: None,
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      title: None,
      preview: None,
      path,
      cwd: None,
      timestamp: None,
      message_count: 0,
    }
  }

  fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
  }
}
