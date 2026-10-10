use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokn_session_codex::{event::CodexLine, normalize::CodexNormalizer};
use tokn_session_core::{AgentEvent, LoadedSessionRecords, NormalizedRecord, Provider};
use tokn_session_opencode::OpenCodeSessionSource;
use tokn_session_pi::{event::PiSessionLine, normalize::PiNormalizer};

use crate::project::ProjectCatalog;
use crate::{NewFileReplay, SessionContext};

type SharedProjectCatalog = Arc<RwLock<ProjectCatalog>>;

#[derive(Clone, Debug)]
pub struct ProviderRoot {
  pub provider: Provider,
  pub path: PathBuf,
}

impl ProviderRoot {
  pub fn new(provider: Provider, path: PathBuf) -> Self {
    Self { provider, path }
  }
}

/// Flattened input for event-oriented renderers, not the wire envelope.
#[derive(Debug)]
pub struct RelayEvent {
  pub path: PathBuf,
  pub topic: String,
  pub session: SessionContext,
  pub event: AgentEvent,
}

/// Wire envelope. Upserts replace the entire event batch for this record ID;
/// removals invalidate an observed SQLite record and carry an empty batch.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RelayRecord {
  pub path: PathBuf,
  pub topic: String,
  pub session: SessionContext,
  pub operation: RecordOperation,
  #[serde(flatten)]
  pub record: NormalizedRecord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordOperation {
  Upsert,
  Remove,
}

impl RelayRecord {
  /// Event-oriented consumers may flatten a record without reparsing its JSON.
  pub fn into_events(self) -> impl Iterator<Item = RelayEvent> {
    self.record.events.into_iter().map(move |event| RelayEvent {
      path: self.path.clone(),
      topic: self.topic.clone(),
      session: self.session.clone(),
      event,
    })
  }
}

#[derive(Debug, Default)]
pub struct TailUpdate {
  pub records: Vec<RelayRecord>,
  pub warnings: Vec<String>,
}

pub struct SessionTailer {
  roots: Vec<ProviderRoot>,
  files: HashMap<PathBuf, FileState>,
  opencode: HashMap<(Provider, PathBuf), OpenCodeState>,
  snapshots: Vec<crate::snapshot_tailer::SnapshotTailer>,
  new_file_replay: NewFileReplay,
  include_native: bool,
  project_catalog: SharedProjectCatalog,
  project_catalog_source: Option<ProjectCatalogSource>,
  project_catalog_warning: Option<String>,
}

impl SessionTailer {
  pub fn initialize(roots: Vec<ProviderRoot>, new_file_replay: NewFileReplay) -> Result<(Self, TailUpdate), String> {
    Self::initialize_with_native(roots, new_file_replay, false)
  }

  pub fn initialize_with_native(
    roots: Vec<ProviderRoot>,
    new_file_replay: NewFileReplay,
    include_native: bool,
  ) -> Result<(Self, TailUpdate), String> {
    let mut tailer = Self::prepare(roots, new_file_replay)?;
    tailer.include_native = include_native;
    let update = tailer.start()?;
    Ok((tailer, update))
  }

  pub(crate) fn prepare(roots: Vec<ProviderRoot>, new_file_replay: NewFileReplay) -> Result<Self, String> {
    let mut tailer = Self::prepare_deferred(roots, new_file_replay)?;
    tailer.discover_initial()?;
    Ok(tailer)
  }

  pub(crate) fn prepare_deferred(roots: Vec<ProviderRoot>, new_file_replay: NewFileReplay) -> Result<Self, String> {
    let (project_catalog, project_catalog_source, project_catalog_warning) = load_project_catalog(&roots);
    let project_catalog = Arc::new(RwLock::new(project_catalog));
    let mut tailer = Self {
      roots,
      files: HashMap::new(),
      opencode: HashMap::new(),
      snapshots: Vec::new(),
      new_file_replay,
      include_native: false,
      project_catalog,
      project_catalog_source,
      project_catalog_warning,
    };
    for root in &tailer.roots {
      if matches!(root.provider, Provider::OpenCode | Provider::ZCode) {
        tailer.opencode.insert(
          (root.provider, root.path.clone()),
          OpenCodeState::with_provider(root.provider, root.path.clone()),
        );
      }
    }
    tailer.snapshots = tailer
      .roots
      .iter()
      .filter(|root| matches!(root.provider, Provider::WorkBuddy | Provider::Dsh))
      .cloned()
      .map(crate::snapshot_tailer::SnapshotTailer::new)
      .collect();
    Ok(tailer)
  }

  pub(crate) fn discover_initial(&mut self) -> Result<(), String> {
    for (path, provider) in self.discover_paths()? {
      self.files.insert(
        path.clone(),
        FileState::open(path, provider, Arc::clone(&self.project_catalog))?,
      );
    }
    Ok(())
  }

  pub(crate) fn set_include_native(&mut self, include_native: bool) {
    self.include_native = include_native;
  }

  pub(crate) fn start(&mut self) -> Result<TailUpdate, String> {
    let mut update = TailUpdate::default();
    if let Some(warning) = self.project_catalog_warning.take() {
      update.warnings.push(warning);
    }
    let new_file_replay = self.new_file_replay;
    for state in self.files.values_mut() {
      state.include_native = self.include_native;
      let mode = if state.matches_initial_snapshot()? {
        InitialRead::Follow
      } else {
        InitialRead::Replay(new_file_replay)
      };
      let initial = read_initial(state, mode)?;
      update.append(initial);
    }
    for state in self.opencode.values_mut() {
      state.include_native = self.include_native;
      update.append(state.scan(false, self.new_file_replay)?);
    }
    for state in &mut self.snapshots {
      update.append(state.scan(false, self.include_native, self.new_file_replay)?);
    }
    Ok(update)
  }

  pub fn scan(&mut self) -> Result<TailUpdate, String> {
    let mut update = TailUpdate::default();
    self.refresh_project_catalog(&mut update);
    let mut discovered = Vec::new();
    let mut seen = HashSet::new();
    for root in &self.roots {
      if matches!(root.provider, Provider::Codex | Provider::Pi)
        && let Err(error) = collect_jsonl_files(&root.path, root.provider, &mut seen, &mut discovered)
      {
        update.warnings.push(error);
      }
    }

    for (path, provider) in discovered {
      if !self.files.contains_key(&path) {
        if let Err(error) = self.add_file(path, provider, InitialRead::Replay(self.new_file_replay), &mut update) {
          update.warnings.push(error);
        }
      }
    }

    let paths = self.files.keys().cloned().collect::<Vec<_>>();
    for path in paths {
      let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
          self.files.remove(&path);
          continue;
        }
        Err(error) => {
          update
            .warnings
            .push(format!("failed to inspect {}: {error}", path.display()));
          continue;
        }
      };
      let Some(state) = self.files.get_mut(&path) else {
        continue;
      };
      match state.read_appended_with_metadata(metadata, true) {
        Ok((mut appended, restarted)) => {
          if restarted {
            apply_replay_policy(&mut appended.records, self.new_file_replay);
          }
          update.append(appended);
        }
        Err(error) => update.warnings.push(error),
      }
    }

    for state in self.opencode.values_mut() {
      match state.scan(true, self.new_file_replay) {
        Ok(scanned) => update.append(scanned),
        Err(error) => update.warnings.push(error),
      }
    }

    for state in &mut self.snapshots {
      match state.scan(true, self.include_native, self.new_file_replay) {
        Ok(scanned) => update.append(scanned),
        Err(error) => update.warnings.push(error),
      }
    }
    Ok(update)
  }

  pub fn scan_paths(&mut self, changed_paths: HashSet<PathBuf>) -> Result<TailUpdate, String> {
    let mut update = TailUpdate::default();
    self.refresh_project_catalog(&mut update);
    for state in &mut self.snapshots {
      if changed_paths.iter().any(|path| state.matches_path(path)) {
        match state.scan(true, self.include_native, self.new_file_replay) {
          Ok(scanned) => update.append(scanned),
          Err(error) => update.warnings.push(error),
        }
      }
    }
    let mut candidates = HashMap::new();
    let mut changed_directories = Vec::new();
    let mut changed_opencode = HashSet::new();

    for path in changed_paths {
      let database_roots = self.open_code_roots_for_event(&path);
      if !database_roots.is_empty() {
        changed_opencode.extend(database_roots);
        continue;
      }
      let Some(provider) = self.provider_for_path(&path) else {
        continue;
      };
      if !matches!(provider, Provider::Codex | Provider::Pi) {
        continue;
      }
      if path.is_dir() {
        changed_directories.push(path.clone());
        let mut seen = HashSet::new();
        let mut discovered = Vec::new();
        if let Err(error) = collect_jsonl_files(&path, provider, &mut seen, &mut discovered) {
          update.warnings.push(error);
        }
        candidates.extend(discovered);
      } else if is_jsonl(&path) {
        candidates.insert(path, provider);
      } else if !path.exists() {
        changed_directories.push(path);
      }
    }

    if !changed_directories.is_empty() {
      self.files.retain(|path, _| {
        if candidates.contains_key(path) || !changed_directories.iter().any(|directory| path.starts_with(directory)) {
          return true;
        }
        match std::fs::metadata(path) {
          Ok(_) => true,
          Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
          Err(error) => {
            update
              .warnings
              .push(format!("failed to inspect {}: {error}", path.display()));
            true
          }
        }
      });
    }

    for (path, provider) in candidates {
      if let Err(error) = self.scan_file(path, provider, &mut update) {
        update.warnings.push(error);
      }
    }
    for path in changed_opencode {
      if let Some(state) = self.opencode.get_mut(&path) {
        match state.scan(true, self.new_file_replay) {
          Ok(scanned) => update.append(scanned),
          Err(error) => update.warnings.push(error),
        }
      }
    }
    Ok(update)
  }

  pub fn roots(&self) -> &[ProviderRoot] {
    &self.roots
  }

  fn scan_file(&mut self, path: PathBuf, provider: Provider, update: &mut TailUpdate) -> Result<(), String> {
    let metadata = match std::fs::metadata(&path) {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
        self.files.remove(&path);
        return Ok(());
      }
      Err(error) => return Err(format!("failed to inspect {}: {error}", path.display())),
    };
    let Some(state) = self.files.get_mut(&path) else {
      return self.add_file_with_metadata(
        path,
        provider,
        InitialRead::Replay(self.new_file_replay),
        metadata,
        update,
      );
    };
    let (mut appended, restarted) = state.read_appended_with_metadata(metadata, true)?;
    if restarted {
      apply_replay_policy(&mut appended.records, self.new_file_replay);
    }
    update.append(appended);
    Ok(())
  }

  fn add_file(
    &mut self,
    path: PathBuf,
    provider: Provider,
    mode: InitialRead,
    update: &mut TailUpdate,
  ) -> Result<(), String> {
    let metadata = std::fs::metadata(&path).map_err(|err| format!("failed to inspect {}: {err}", path.display()))?;
    self.add_file_with_metadata(path, provider, mode, metadata, update)
  }

  fn add_file_with_metadata(
    &mut self,
    path: PathBuf,
    provider: Provider,
    mode: InitialRead,
    metadata: std::fs::Metadata,
    update: &mut TailUpdate,
  ) -> Result<(), String> {
    let mut state = FileState::open_with_metadata(path.clone(), provider, Arc::clone(&self.project_catalog), metadata);
    state.include_native = self.include_native;
    let initial = read_initial(&mut state, mode)?;
    update.append(initial);
    self.files.insert(path, state);
    Ok(())
  }

  fn discover_paths(&self) -> Result<Vec<(PathBuf, Provider)>, String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for root in &self.roots {
      if matches!(root.provider, Provider::Codex | Provider::Pi) {
        collect_jsonl_files(&root.path, root.provider, &mut seen, &mut paths)?;
      }
    }
    Ok(paths)
  }

  fn provider_for_path(&self, path: &Path) -> Option<Provider> {
    self
      .roots
      .iter()
      .filter(|root| path.starts_with(&root.path))
      .max_by_key(|root| root.path.components().count())
      .map(|root| root.provider)
  }

  fn open_code_roots_for_event(&self, path: &Path) -> Vec<(Provider, PathBuf)> {
    self
      .opencode
      .iter()
      .filter(|(_, state)| state.matches_path(path))
      .map(|(root, _)| root.clone())
      .collect()
  }

  fn refresh_project_catalog(&mut self, update: &mut TailUpdate) {
    let Some(source) = &mut self.project_catalog_source else {
      return;
    };
    let Some(result) = source.load_if_changed() else {
      return;
    };
    match result {
      Ok(catalog) => {
        *self
          .project_catalog
          .write()
          .unwrap_or_else(|poisoned| poisoned.into_inner()) = catalog;
      }
      Err(error) => update.warnings.push(error),
    }
  }
}

fn read_initial(state: &mut FileState, mode: InitialRead) -> Result<TailUpdate, String> {
  match mode {
    InitialRead::Follow => state.seed_at_eof(),
    InitialRead::Replay(NewFileReplay::All) => Ok(state.read_appended(true)?.0),
    InitialRead::Replay(NewFileReplay::Messages(message_count)) => {
      let mut update = state.read_appended(true)?.0;
      retain_message_history(&mut update.records, message_count);
      Ok(update)
    }
  }
}

#[derive(Clone, Copy)]
enum InitialRead {
  Follow,
  Replay(NewFileReplay),
}

impl TailUpdate {
  fn append(&mut self, mut other: Self) {
    self.records.append(&mut other.records);
    self.warnings.append(&mut other.warnings);
  }
}

struct OpenCodeState {
  provider: Provider,
  root_path: PathBuf,
  source: OpenCodeSessionSource,
  sessions: HashMap<String, OpenCodeSessionState>,
  database_version: Option<OpenCodeDatabaseVersion>,
  include_native: bool,
}

struct OpenCodeSessionState {
  summary: OpenCodeSessionSummary,
  fingerprints: HashMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OpenCodeSessionSummary {
  parent_session_id: Option<String>,
  cwd: Option<String>,
  timestamp: Option<String>,
  message_count: usize,
}

impl OpenCodeSessionSummary {
  fn from_reference(reference: &tokn_session_core::SessionRef) -> Self {
    Self {
      parent_session_id: reference.parent_session_id.clone(),
      cwd: reference.cwd.clone(),
      timestamp: reference.timestamp.clone(),
      message_count: reference.message_count,
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenCodeDatabaseVersion {
  database: CatalogFileVersion,
  wal: Option<CatalogFileVersion>,
}

impl OpenCodeState {
  #[cfg(test)]
  fn new(root_path: PathBuf) -> Self {
    Self::with_provider(Provider::OpenCode, root_path)
  }

  fn with_provider(provider: Provider, root_path: PathBuf) -> Self {
    Self {
      provider,
      source: crate::providers::database(provider, Some(root_path.clone())),
      root_path,
      sessions: HashMap::new(),
      database_version: None,
      include_native: false,
    }
  }

  fn scan(&mut self, publish_new_sessions: bool, replay: NewFileReplay) -> Result<TailUpdate, String> {
    let Some(database_version) = self.database_version() else {
      self.database_version = None;
      self.sessions.clear();
      return Ok(TailUpdate::default());
    };
    if self.database_version == Some(database_version) {
      return Ok(TailUpdate::default());
    }

    let mut reader = self.source.scan_reader()?;
    let references = reader.list_sessions()?;
    let mut seen = HashSet::new();
    let mut update = TailUpdate::default();
    let mut to_load = Vec::new();
    let mut failed = false;
    for reference in &references {
      let session_id = reference.id.clone();
      seen.insert(session_id.clone());
      let summary = OpenCodeSessionSummary::from_reference(reference);
      let changed = self
        .sessions
        .get(&session_id)
        .is_none_or(|previous| previous.summary != summary);
      if changed {
        to_load.push((session_id, summary));
      }
    }

    let removed_session = self.sessions.keys().any(|session_id| !seen.contains(session_id));
    if to_load.is_empty() && !removed_session && !self.sessions.is_empty() {
      // OpenCode does not expose update timestamps for message or part rows.
      // If the session summary did not move, reload everything once to catch
      // an in-place content edit; normal appends only reload changed sessions.
      to_load = references
        .iter()
        .map(|reference| (reference.id.clone(), OpenCodeSessionSummary::from_reference(reference)))
        .collect();
    }

    for (session_id, summary) in to_load {
      let loaded = match reader.load_session_records_exact(&session_id, self.include_native) {
        Ok(loaded) => loaded,
        Err(error) if !publish_new_sessions => return Err(error),
        Err(error) => {
          update.warnings.push(format!(
            "failed to load {} session {session_id}: {error}",
            provider_name(self.provider)
          ));
          failed = true;
          continue;
        }
      };
      let fingerprints = loaded
        .records
        .iter()
        .map(|record| {
          serde_json::to_string(record)
            .map(|value| (record.record_id.clone(), value))
            .map_err(|err| err.to_string())
        })
        .collect::<Result<HashMap<_, _>, _>>();
      let fingerprints = match fingerprints {
        Ok(fingerprints) => fingerprints,
        Err(error) if !publish_new_sessions => return Err(error),
        Err(error) => {
          update.warnings.push(format!(
            "failed to fingerprint {} session {session_id}: {error}",
            provider_name(self.provider)
          ));
          failed = true;
          continue;
        }
      };
      let context = SessionContext::from_session_ref(self.provider, &loaded.reference);
      let events = relay_records_from_loaded(loaded, &context, &fingerprints, self.sessions.get(&session_id));

      match self.sessions.get(&session_id) {
        None if publish_new_sessions => {
          let mut events = events;
          apply_replay_policy(&mut events, replay);
          update.records.extend(events);
        }
        Some(_) if publish_new_sessions => {
          update.records.extend(events);
        }
        _ => {}
      }

      self
        .sessions
        .insert(session_id, OpenCodeSessionState { summary, fingerprints });
    }
    self.sessions.retain(|session_id, _| seen.contains(session_id));
    if !failed {
      self.database_version = Some(database_version);
    }
    Ok(update)
  }

  fn database_version(&self) -> Option<OpenCodeDatabaseVersion> {
    let database_path = self.source.database_path().ok()?;
    let database = file_version(&database_path)?;
    // SHM is a reader-writable WAL index, not durable session content. Including
    // it would invalidate this cache because of the relay's own reads.
    Some(OpenCodeDatabaseVersion {
      database,
      wal: file_version(&sqlite_sidecar_path(&database_path, "-wal")),
    })
  }

  fn matches_path(&self, path: &Path) -> bool {
    let Ok(database_path) = self.source.database_path() else {
      return false;
    };
    path == &database_path
      || path == sqlite_sidecar_path(&database_path, "-wal")
      // A directory watcher can report SHM creation before it reports the WAL.
      // Treat that as a wake-up, but database_version deliberately ignores SHM.
      || path == sqlite_sidecar_path(&database_path, "-shm")
      || path == self.root_path
      || database_path.parent().is_some_and(|parent| path == parent)
  }
}

fn relay_records_from_loaded(
  loaded: LoadedSessionRecords,
  context: &SessionContext,
  fingerprints: &HashMap<String, String>,
  previous: Option<&OpenCodeSessionState>,
) -> Vec<RelayRecord> {
  let path = loaded.reference.path.clone();
  let topic = format!(
    "{}.{}",
    crate::providers::source(context.provider).as_str(),
    context.session_id
  );
  let mut records: Vec<_> = loaded
    .records
    .into_iter()
    .filter_map(|record| {
      let changed = previous
        .and_then(|previous| previous.fingerprints.get(&record.record_id))
        .is_none_or(|fingerprint| fingerprint != &fingerprints[&record.record_id]);
      changed.then(|| RelayRecord {
        topic: topic.clone(),
        path: path.clone(),
        session: context.clone(),
        operation: RecordOperation::Upsert,
        record,
      })
    })
    .collect();
  if let Some(previous) = previous {
    let mut removed: Vec<_> = previous
      .fingerprints
      .keys()
      .filter(|id| !fingerprints.contains_key(*id))
      .collect();
    removed.sort();
    records.extend(removed.into_iter().map(|id| RelayRecord {
      path: path.clone(),
      topic: topic.clone(),
      session: context.clone(),
      operation: RecordOperation::Remove,
      record: NormalizedRecord {
        record_id: id.clone(),
        native: None,
        events: Vec::new(),
      },
    }));
  }
  records
}

pub struct FileState {
  path: PathBuf,
  provider: Provider,
  identity: FileIdentity,
  initial_length: u64,
  offset: u64,
  pending: Vec<u8>,
  normalizer: SessionNormalizer,
  codex_seed_length: Option<u64>,
  context: SessionContext,
  project_catalog: SharedProjectCatalog,
  include_native: bool,
}

impl FileState {
  /// Service readers start at zero and keep this exact normalizer for follow.
  pub fn for_snapshot(path: PathBuf, provider: Provider, include_native: bool, root: &Path) -> Result<Self, String> {
    let (catalog, _, _) = load_project_catalog(&[ProviderRoot::new(provider, root.to_path_buf())]);
    let mut state = Self::open(path, provider, Arc::new(RwLock::new(catalog)))?;
    state.include_native = include_native;
    Ok(state)
  }

  pub fn follow_snapshot(&mut self) -> Result<(TailUpdate, bool), String> {
    self.read_appended(true)
  }

  fn open(path: PathBuf, provider: Provider, project_catalog: SharedProjectCatalog) -> Result<Self, String> {
    let metadata = std::fs::metadata(&path).map_err(|err| format!("failed to inspect {}: {err}", path.display()))?;
    Ok(Self::open_with_metadata(path, provider, project_catalog, metadata))
  }

  fn open_with_metadata(
    path: PathBuf,
    provider: Provider,
    project_catalog: SharedProjectCatalog,
    metadata: std::fs::Metadata,
  ) -> Self {
    let context = SessionContext::from_path(provider, &path);
    Self {
      path,
      provider,
      identity: file_identity(&metadata),
      initial_length: metadata.len(),
      offset: 0,
      pending: Vec::new(),
      normalizer: SessionNormalizer::new(provider),
      codex_seed_length: None,
      context,
      project_catalog,
      include_native: false,
    }
  }

  fn seed_at_eof(&mut self) -> Result<TailUpdate, String> {
    const MAX_SESSION_HEADER_LINES: usize = 64;

    let file = File::open(&self.path).map_err(|err| format!("failed to open {}: {err}", self.path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut update = TailUpdate::default();
    let project_catalog = self
      .project_catalog
      .read()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    for _ in 0..MAX_SESSION_HEADER_LINES {
      line.clear();
      let bytes = reader
        .read_line(&mut line)
        .map_err(|err| format!("failed to read {}: {err}", self.path.display()))?;
      if bytes == 0 || !line.ends_with('\n') {
        break;
      }
      match self.normalizer.normalize_line(
        line.trim_end_matches(['\r', '\n']),
        &mut self.context,
        &project_catalog,
        false,
      ) {
        Ok((_, events))
          if events
            .iter()
            .any(|event| matches!(event, AgentEvent::SessionStarted(_))) =>
        {
          break;
        }
        Ok(_) => {}
        Err(err) => update
          .warnings
          .push(format!("failed to seed {}: {err}", self.path.display())),
      }
    }

    self.offset = self.initial_length;
    let mut file = reader.into_inner();
    self.pending = trailing_partial_line(&mut file, &self.path, self.offset)?;
    // Restore stateful Codex history only if the file actually appends. Idle
    // rollouts keep the inexpensive header/EOF seed used by other JSONL feeds.
    self.codex_seed_length = (self.provider == Provider::Codex).then_some(self.initial_length);
    Ok(update)
  }

  /// Codex requests, code-mode calls, compactions and settings can affect later
  /// records. Restore their state without replaying historical events. Limit
  /// the read to the discovery snapshot so concurrent appends remain live.
  fn restore_codex_seed(&mut self, file: &mut File, seed_length: u64) -> Result<TailUpdate, String> {
    self.normalizer = SessionNormalizer::new(self.provider);
    self.context = SessionContext::from_path(self.provider, &self.path);
    self.pending.clear();
    let mut reader = BufReader::new(file.take(seed_length));
    let mut line = Vec::new();
    let mut update = TailUpdate::default();
    let project_catalog = self
      .project_catalog
      .read()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
      line.clear();
      let bytes = reader
        .read_until(b'\n', &mut line)
        .map_err(|err| format!("failed to read {}: {err}", self.path.display()))?;
      if bytes == 0 {
        break;
      }
      if !line.ends_with(b"\n") {
        self.pending = line;
        break;
      }
      let line = match std::str::from_utf8(&line) {
        Ok(line) if !line.trim().is_empty() => line.trim_end_matches(['\r', '\n']),
        Ok(_) => continue,
        Err(err) => {
          update
            .warnings
            .push(format!("invalid UTF-8 while seeding {}: {err}", self.path.display()));
          continue;
        }
      };
      if let Err(err) = self
        .normalizer
        .normalize_line(line, &mut self.context, &project_catalog, false)
      {
        update
          .warnings
          .push(format!("failed to seed {}: {err}", self.path.display()));
      }
    }
    Ok(update)
  }

  fn matches_initial_snapshot(&self) -> Result<bool, String> {
    let metadata =
      std::fs::metadata(&self.path).map_err(|err| format!("failed to inspect {}: {err}", self.path.display()))?;
    Ok(file_identity(&metadata) == self.identity && metadata.len() >= self.initial_length)
  }

  fn read_appended(&mut self, publish: bool) -> Result<(TailUpdate, bool), String> {
    let metadata =
      std::fs::metadata(&self.path).map_err(|err| format!("failed to inspect {}: {err}", self.path.display()))?;
    self.read_appended_with_metadata(metadata, publish)
  }

  fn read_appended_with_metadata(
    &mut self,
    metadata: std::fs::Metadata,
    publish: bool,
  ) -> Result<(TailUpdate, bool), String> {
    let identity = file_identity(&metadata);

    let mut should_publish = publish;
    let mut restarted = false;
    if identity != self.identity || metadata.len() < self.offset {
      self.identity = identity;
      self.offset = 0;
      self.pending.clear();
      self.normalizer = SessionNormalizer::new(self.provider);
      self.codex_seed_length = None;
      self.context = SessionContext::from_path(self.provider, &self.path);
      should_publish = true;
      restarted = true;
    }
    let length = metadata.len();
    if length == self.offset {
      return Ok((TailUpdate::default(), restarted));
    }

    let mut file = File::open(&self.path).map_err(|err| format!("failed to open {}: {err}", self.path.display()))?;
    let mut update = TailUpdate::default();
    if let Some(seed_length) = self.codex_seed_length {
      update.append(self.restore_codex_seed(&mut file, seed_length)?);
      self.codex_seed_length = None;
    }
    file
      .seek(SeekFrom::Start(self.offset))
      .map_err(|err| format!("failed to seek {}: {err}", self.path.display()))?;
    let mut appended = Vec::new();
    file
      .read_to_end(&mut appended)
      .map_err(|err| format!("failed to read {}: {err}", self.path.display()))?;
    self.offset += appended.len() as u64;
    self.pending.extend(appended);

    let complete_length = self
      .pending
      .iter()
      .rposition(|byte| *byte == b'\n')
      .map(|index| index + 1)
      .unwrap_or(0);
    if complete_length == 0 {
      return Ok((update, restarted));
    }

    let mut line_offset = self.offset - self.pending.len() as u64;
    let complete = self.pending.drain(..complete_length).collect::<Vec<_>>();
    let project_catalog = self
      .project_catalog
      .read()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    for raw_line in complete.split(|byte| *byte == b'\n') {
      let record_id = format!("jsonl:{line_offset}");
      line_offset += raw_line.len() as u64 + 1;
      let line = match std::str::from_utf8(raw_line) {
        Ok(line) if !line.trim().is_empty() => line,
        Ok(_) => continue,
        Err(err) => {
          update
            .warnings
            .push(format!("invalid UTF-8 in {}: {err}", self.path.display()));
          continue;
        }
      };

      match self
        .normalizer
        .normalize_line(line, &mut self.context, &project_catalog, self.include_native)
      {
        Ok((native, events)) if should_publish => {
          update.records.push(RelayRecord {
            topic: format!("{}.{}", provider_name(self.provider), self.context.session_id),
            path: self.path.clone(),
            session: self.context.clone(),
            operation: RecordOperation::Upsert,
            record: NormalizedRecord {
              record_id,
              native,
              events,
            },
          });
        }
        Ok(_) => {}
        Err(err) => update
          .warnings
          .push(format!("failed to normalize {}: {err}", self.path.display())),
      }
    }
    Ok((update, restarted))
  }
}

fn retain_message_history(events: &mut Vec<RelayRecord>, message_count: usize) {
  if message_count == 0 {
    events.clear();
    return;
  }

  let message_indices = events
    .iter()
    .enumerate()
    .flat_map(|(index, record)| {
      record
        .record
        .events
        .iter()
        .filter_map(move |event| (matches!(event, AgentEvent::Message(_)) && !event.is_hidden()).then_some(index))
    })
    .collect::<Vec<_>>();
  if message_indices.len() <= message_count {
    return;
  }

  let start = message_indices[message_indices.len() - message_count];
  events.drain(..start);
}

pub(crate) fn apply_replay_policy(events: &mut Vec<RelayRecord>, replay: NewFileReplay) {
  if let NewFileReplay::Messages(message_count) = replay {
    retain_message_history(events, message_count);
  }
}

fn trailing_partial_line(file: &mut File, path: &Path, length: u64) -> Result<Vec<u8>, String> {
  const CHUNK_SIZE: usize = 8 * 1024;

  if length == 0 {
    return Ok(Vec::new());
  }

  file
    .seek(SeekFrom::Start(length - 1))
    .map_err(|err| format!("failed to seek {}: {err}", path.display()))?;
  let mut final_byte = [0];
  file
    .read_exact(&mut final_byte)
    .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
  if final_byte[0] == b'\n' {
    return Ok(Vec::new());
  }

  let mut position = length - 1;
  let mut chunks = vec![final_byte.to_vec()];
  while position > 0 {
    let chunk_length = position.min(CHUNK_SIZE as u64) as usize;
    position -= chunk_length as u64;
    file
      .seek(SeekFrom::Start(position))
      .map_err(|err| format!("failed to seek {}: {err}", path.display()))?;
    let mut chunk = vec![0; chunk_length];
    file
      .read_exact(&mut chunk)
      .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

    if let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') {
      chunks.push(chunk[(newline + 1)..].to_vec());
      break;
    }
    chunks.push(chunk);
    if position == 0 {
      break;
    }
  }

  chunks.reverse();
  Ok(chunks.into_iter().flatten().collect())
}

enum SessionNormalizer {
  Codex(CodexNormalizer),
  Pi(PiNormalizer),
}

impl SessionNormalizer {
  fn new(provider: Provider) -> Self {
    match provider {
      Provider::Codex => Self::Codex(CodexNormalizer::new()),
      Provider::Pi => Self::Pi(PiNormalizer::new()),
      Provider::OpenCode | Provider::ZCode | Provider::WorkBuddy | Provider::Dsh => {
        unreachable!("provider is not supported by the JSONL tailer")
      }
    }
  }

  fn normalize_line(
    &mut self,
    line: &str,
    context: &mut SessionContext,
    project_catalog: &ProjectCatalog,
    include_native: bool,
  ) -> Result<(Option<Value>, Vec<AgentEvent>), String> {
    let value: Value = serde_json::from_str(line).map_err(|err| format!("invalid session JSONL: {err}"))?;
    context.update(&value);
    if matches!(context.provider, Provider::Codex) {
      context.resolve_project_name(project_catalog);
    }

    let native = include_native.then(|| value.clone());
    let events = match self {
      Self::Codex(normalizer) => {
        let event: CodexLine = serde_json::from_value(value).map_err(|err| format!("invalid codex JSONL: {err}"))?;
        normalizer.normalize(event)
      }
      Self::Pi(normalizer) => {
        let event: PiSessionLine = serde_json::from_value(value).map_err(|err| format!("invalid pi JSONL: {err}"))?;
        normalizer.normalize(event)
      }
    };
    Ok((native, events))
  }
}

fn load_project_catalog(roots: &[ProviderRoot]) -> (ProjectCatalog, Option<ProjectCatalogSource>, Option<String>) {
  let Some(path) = roots
    .iter()
    .find(|root| matches!(root.provider, Provider::Codex))
    .and_then(|root| root.path.parent())
    .map(|codex_home| codex_home.join(".codex-global-state.json"))
  else {
    return (ProjectCatalog::default(), None, None);
  };
  let mut source = ProjectCatalogSource {
    path,
    observed_version: None,
  };
  match source.load_if_changed() {
    Some(Ok(catalog)) => (catalog, Some(source), None),
    Some(Err(error)) => (ProjectCatalog::default(), Some(source), Some(error)),
    None => (ProjectCatalog::default(), Some(source), None),
  }
}

struct ProjectCatalogSource {
  path: PathBuf,
  observed_version: Option<CatalogFileVersion>,
}

impl ProjectCatalogSource {
  fn load_if_changed(&mut self) -> Option<Result<ProjectCatalog, String>> {
    let metadata = match std::fs::metadata(&self.path) {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
        return self.observed_version.take().map(|_| Ok(ProjectCatalog::default()));
      }
      Err(error) => {
        return Some(Err(format!(
          "failed to inspect Codex Desktop project catalog {}: {error}",
          self.path.display()
        )));
      }
    };
    let version = CatalogFileVersion {
      identity: file_identity(&metadata),
      length: metadata.len(),
      modified: metadata.modified().ok(),
    };
    if self.observed_version == Some(version) {
      return None;
    }
    self.observed_version = Some(version);
    Some(ProjectCatalog::load(&self.path))
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CatalogFileVersion {
  identity: FileIdentity,
  length: u64,
  modified: Option<SystemTime>,
}

fn file_version(path: &Path) -> Option<CatalogFileVersion> {
  let metadata = std::fs::metadata(path).ok()?;
  metadata.is_file().then(|| CatalogFileVersion {
    identity: file_identity(&metadata),
    length: metadata.len(),
    modified: metadata.modified().ok(),
  })
}

fn sqlite_sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
  let name = database_path
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or("opencode.db");
  database_path.with_file_name(format!("{name}{suffix}"))
}

fn provider_name(provider: Provider) -> &'static str {
  match provider {
    Provider::Codex => "codex",
    Provider::Pi => "pi",
    Provider::OpenCode => "opencode",
    Provider::ZCode => "zcode",
    Provider::WorkBuddy => "workbuddy",
    Provider::Dsh => "dsh",
  }
}

fn collect_jsonl_files(
  dir: &Path,
  provider: Provider,
  seen: &mut HashSet<PathBuf>,
  paths: &mut Vec<(PathBuf, Provider)>,
) -> Result<(), String> {
  if !dir.exists() {
    return Ok(());
  }
  if dir.is_file() {
    if is_jsonl(dir) {
      let path = dir.to_path_buf();
      if seen.insert(path.clone()) {
        paths.push((path, provider));
      }
    }
    return Ok(());
  }

  for entry in std::fs::read_dir(dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))? {
    let entry = entry.map_err(|err| format!("failed to read entry in {}: {err}", dir.display()))?;
    let path = entry.path();
    let file_type = entry
      .file_type()
      .map_err(|err| format!("failed to inspect {}: {err}", path.display()))?;
    // A configured root may be a symlink, but following nested directory
    // symlinks can walk an ancestor again without ever reaching an end.
    if file_type.is_dir() {
      collect_jsonl_files(&path, provider, seen, paths)?;
    } else if is_jsonl(&path)
      && (file_type.is_file() || (file_type.is_symlink() && path.is_file()))
      && seen.insert(path.clone())
    {
      paths.push((path, provider));
    }
  }
  Ok(())
}

fn is_jsonl(path: &Path) -> bool {
  path.extension().and_then(|value| value.to_str()) == Some("jsonl")
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
  device: u64,
  inode: u64,
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
  use std::os::unix::fs::MetadataExt;

  FileIdentity {
    device: metadata.dev(),
    inode: metadata.ino(),
  }
}

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
  created: Option<std::time::SystemTime>,
}

#[cfg(not(unix))]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
  FileIdentity {
    created: metadata.created().ok(),
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;
  use std::fs::{File, OpenOptions};
  use std::io::Write;

  use rusqlite::{Connection, params};
  use tempfile::TempDir;
  use tokn_session_core::{AgentEvent, Provider};

  use super::{OpenCodeState, ProviderRoot, SessionTailer, trailing_partial_line};
  use crate::NewFileReplay;

  #[test]
  fn starts_at_eof_but_seeds_pi_session_context() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"type\":\"session\",\"id\":\"pi-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"old\",\"message\":{\"role\":\"user\",\"content\":\"old\"}}\n"
      ),
    )
    .unwrap();

    let (mut tailer, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
    )
    .unwrap();
    assert!(initial.records.is_empty());

    append(
      &path,
      "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n",
    );
    let update = tailer.scan().unwrap();
    assert_eq!(update.records.len(), 1);
    assert_eq!(update.records[0].topic, "pi.pi-session");
    assert_eq!(update.records[0].session.session_id, "pi-session");
    assert_eq!(update.records[0].session.started_at.as_deref(), Some("2026-01-01"));
    assert_eq!(
      update.records[0]
        .session
        .project
        .as_ref()
        .and_then(|project| project.name.as_deref()),
      Some("tmp")
    );
    let AgentEvent::Message(message) = &update.records[0].record.events[0] else {
      panic!("expected message");
    };
    assert_eq!(message.text, "done");
    assert_eq!(message.session_id.as_deref(), Some("pi-session"));
  }

  #[test]
  fn replay_all_does_not_apply_to_files_present_at_startup() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"type\":\"session\",\"id\":\"pi-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
        "this historical line is deliberately invalid JSON\n",
        "{\"type\":\"message\",\"id\":\"old\",\"message\":{\"role\":\"user\",\"content\":\"old\"}}\n"
      ),
    )
    .unwrap();

    let (mut tailer, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::All,
    )
    .unwrap();
    assert!(initial.records.is_empty());
    assert!(initial.warnings.is_empty());

    append(
      &path,
      "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"new\"}}\n",
    );
    let update = tailer.scan().unwrap();
    assert_eq!(update.records.len(), 1);
    let AgentEvent::Message(message) = &update.records[0].record.events[0] else {
      panic!("expected message");
    };
    assert_eq!(message.text, "new");
  }

  #[test]
  fn catches_appends_between_the_snapshot_and_follow_start() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(
      &path,
      "{\"type\":\"session\",\"id\":\"pi-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
    )
    .unwrap();
    let mut tailer = SessionTailer::prepare(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
    )
    .unwrap();

    append(
      &path,
      "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"not lost\"}}\n",
    );
    assert!(tailer.start().unwrap().records.is_empty());

    let update = tailer.scan().unwrap();
    assert_eq!(update.records.len(), 1);
    let AgentEvent::Message(message) = &update.records[0].record.events[0] else {
      panic!("expected message");
    };
    assert_eq!(message.text, "not lost");
  }

  #[test]
  fn backfills_files_replaced_before_follow_start() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(
      &path,
      "{\"type\":\"session\",\"id\":\"old-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
    )
    .unwrap();
    let mut tailer = SessionTailer::prepare(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::Messages(1),
    )
    .unwrap();

    let replacement = fixture.path().join("replacement.jsonl");
    std::fs::write(
      &replacement,
      concat!(
        "{\"type\":\"session\",\"id\":\"new-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"1\",\"message\":{\"role\":\"user\",\"content\":\"old\"}}\n",
        "{\"type\":\"message\",\"id\":\"2\",\"message\":{\"role\":\"user\",\"content\":\"recent\"}}\n"
      ),
    )
    .unwrap();
    std::fs::rename(replacement, &path).unwrap();

    let update = tailer.start().unwrap();
    assert_eq!(update.records.len(), 1);
    let AgentEvent::Message(message) = &update.records[0].record.events[0] else {
      panic!("expected message");
    };
    assert_eq!(message.text, "recent");
    assert_eq!(message.session_id.as_deref(), Some("new-session"));
  }

  #[test]
  fn buffers_partial_lines_until_newline() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(
      &path,
      "{\"type\":\"session\",\"id\":\"pi-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
    )
    .unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
    )
    .unwrap();

    append(
      &path,
      "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}",
    );
    assert!(tailer.scan().unwrap().records.is_empty());
    append(&path, "\n");
    assert_eq!(tailer.scan().unwrap().records.len(), 1);
  }

  #[test]
  fn scans_only_files_named_by_watcher_events() {
    let fixture = TempDir::new().unwrap();
    let first = fixture.path().join("session_first.jsonl");
    let second = fixture.path().join("session_second.jsonl");
    for (path, session_id) in [(&first, "first"), (&second, "second")] {
      std::fs::write(
        path,
        format!("{{\"type\":\"session\",\"id\":\"{session_id}\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}}\n"),
      )
      .unwrap();
    }
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
    )
    .unwrap();

    append(
      &first,
      "{\"type\":\"message\",\"id\":\"first\",\"message\":{\"role\":\"user\",\"content\":\"first\"}}\n",
    );
    append(
      &second,
      "{\"type\":\"message\",\"id\":\"second\",\"message\":{\"role\":\"user\",\"content\":\"second\"}}\n",
    );

    let first_update = tailer.scan_paths(HashSet::from([first])).unwrap();
    assert_eq!(first_update.records.len(), 1);
    assert_eq!(first_update.records[0].topic, "pi.first");

    let second_update = tailer.scan_paths(HashSet::from([second])).unwrap();
    assert_eq!(second_update.records.len(), 1);
    assert_eq!(second_update.records[0].topic, "pi.second");
  }

  #[test]
  fn codex_directory_event_reads_append_and_removes_deleted_file() {
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join("sessions");
    let day = root.join("2026/10/09");
    std::fs::create_dir_all(&day).unwrap();
    let active = day.join("rollout-active.jsonl");
    let deleted = day.join("rollout-deleted.jsonl");
    std::fs::write(&active, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"active\"}}\n").unwrap();
    std::fs::write(
      &deleted,
      "{\"type\":\"session_meta\",\"payload\":{\"id\":\"deleted\"}}\n",
    )
    .unwrap();
    let (mut tailer, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, root.clone())],
      NewFileReplay::All,
    )
    .unwrap();
    assert!(initial.records.is_empty());

    append(
      &active,
      "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n",
    );
    std::fs::remove_file(&deleted).unwrap();

    let update = tailer.scan_paths(HashSet::from([day])).unwrap();
    assert!(update.warnings.is_empty());
    assert_eq!(update.records.len(), 1);
    assert_eq!(update.records[0].topic, "codex.active");
    assert!(tailer.files.contains_key(&active));
    assert!(!tailer.files.contains_key(&deleted));
  }

  #[cfg(unix)]
  #[test]
  fn directory_event_keeps_file_state_after_metadata_error() {
    let fixture = TempDir::new().unwrap();
    let root = fixture.path().join("sessions");
    std::fs::create_dir(&root).unwrap();
    let path = root.join("rollout-session.jsonl");
    std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"old\"}}\n").unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, root.clone())],
      NewFileReplay::All,
    )
    .unwrap();

    // Keep the replacement's inode distinct from the tracked file's inode.
    let replacement = fixture.path().join("replacement");
    std::fs::write(
      &replacement,
      concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"new\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n"
      ),
    )
    .unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();

    let failed = tailer.scan_paths(HashSet::from([root.clone()])).unwrap();
    assert!(
      failed
        .warnings
        .iter()
        .any(|warning| warning.contains("failed to inspect"))
    );
    assert!(tailer.files.contains_key(&path));

    std::fs::remove_file(&path).unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let recovered = tailer.scan_paths(HashSet::from([root])).unwrap();
    assert!(recovered.warnings.is_empty());
    assert_eq!(recovered.records.len(), 2);
    assert!(recovered.records.iter().all(|record| record.topic == "codex.new"));
  }

  #[test]
  fn full_scan_removes_missing_file_and_replays_recreated_path() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(&path, "{\"type\":\"session\",\"id\":\"old-session\"}\n").unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::All,
    )
    .unwrap();

    std::fs::remove_file(&path).unwrap();
    let missing = tailer.scan().unwrap();
    assert!(missing.records.is_empty());
    assert!(missing.warnings.is_empty());
    assert!(!tailer.files.contains_key(&path));

    std::fs::write(
      &path,
      concat!(
        "{\"type\":\"session\",\"id\":\"new-session\"}\n",
        "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n"
      ),
    )
    .unwrap();
    let recreated = tailer.scan().unwrap();
    assert!(recreated.warnings.is_empty());
    assert_eq!(recreated.records.len(), 2);
    assert!(recreated.records.iter().all(|record| record.topic == "pi.new-session"));
  }

  #[cfg(unix)]
  #[test]
  fn metadata_error_keeps_existing_file_state_for_retry() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(&path, "{\"type\":\"session\",\"id\":\"old-session\"}\n").unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::All,
    )
    .unwrap();

    // Allocate a distinct inode before removing the tracked file. Immediate
    // recreation can reuse its inode on Linux and look like an append.
    let replacement = fixture.path().join("replacement");
    std::fs::write(
      &replacement,
      concat!(
        "{\"type\":\"session\",\"id\":\"new-session\"}\n",
        "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n"
      ),
    )
    .unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();
    let full = tailer.scan().unwrap();
    assert!(
      full
        .warnings
        .iter()
        .any(|warning| warning.contains("failed to inspect"))
    );
    assert!(tailer.files.contains_key(&path));
    let watcher = tailer.scan_paths(HashSet::from([path.clone()])).unwrap();
    assert!(
      watcher
        .warnings
        .iter()
        .any(|warning| warning.contains("failed to inspect"))
    );
    assert!(tailer.files.contains_key(&path));

    std::fs::remove_file(&path).unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let recovered = tailer.scan_paths(HashSet::from([path])).unwrap();
    assert!(recovered.warnings.is_empty());
    assert_eq!(recovered.records.len(), 2);
    assert!(recovered.records.iter().all(|record| record.topic == "pi.new-session"));
  }

  #[test]
  fn full_scan_keeps_new_file_records_when_an_existing_file_fails() {
    let fixture = TempDir::new().unwrap();
    let existing = fixture.path().join("session_existing.jsonl");
    std::fs::write(&existing, "{\"type\":\"session\",\"id\":\"existing\"}\n").unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::All,
    )
    .unwrap();

    let saved = fixture.path().join("existing.saved");
    std::fs::rename(&existing, &saved).unwrap();
    std::fs::create_dir(&existing).unwrap();
    std::fs::write(existing.join("entry"), b"x").unwrap();
    let fresh = fixture.path().join("session_fresh.jsonl");
    std::fs::write(&fresh, "{\"type\":\"session\",\"id\":\"fresh\"}\n").unwrap();

    let update = tailer.scan().unwrap();
    assert!(update.records.iter().any(|record| record.topic == "pi.fresh"));
    assert!(
      update
        .warnings
        .iter()
        .any(|warning| warning.contains("session_existing.jsonl"))
    );

    std::fs::remove_dir_all(&existing).unwrap();
    std::fs::rename(saved, existing).unwrap();
    let retry = tailer.scan().unwrap();
    assert!(!retry.records.iter().any(|record| record.topic == "pi.fresh"));
    assert!(retry.warnings.is_empty());
  }

  #[test]
  fn watcher_scan_keeps_file_records_when_another_provider_fails() {
    let fixture = TempDir::new().unwrap();
    let pi_root = fixture.path().join("pi");
    std::fs::create_dir(&pi_root).unwrap();
    let database = fixture.path().join("opencode.db");
    let (mut tailer, _) = SessionTailer::initialize(
      vec![
        ProviderRoot::new(Provider::Pi, pi_root.clone()),
        ProviderRoot::new(Provider::OpenCode, database.clone()),
      ],
      NewFileReplay::All,
    )
    .unwrap();

    let fresh = pi_root.join("session_fresh.jsonl");
    std::fs::write(&fresh, "{\"type\":\"session\",\"id\":\"fresh\"}\n").unwrap();
    std::fs::write(&database, b"not a database").unwrap();

    let update = tailer
      .scan_paths(HashSet::from([fresh.clone(), database.clone()]))
      .unwrap();
    assert!(update.records.iter().any(|record| record.topic == "pi.fresh"));
    assert!(update.warnings.iter().any(|warning| warning.contains("database")));

    let retry = tailer.scan_paths(HashSet::from([database])).unwrap();
    assert!(retry.warnings.iter().any(|warning| warning.contains("database")));
    assert!(tailer.scan_paths(HashSet::from([fresh])).unwrap().records.is_empty());
  }

  #[cfg(unix)]
  #[test]
  fn full_scan_continues_after_a_root_traversal_error() {
    let fixture = TempDir::new().unwrap();
    let pi_root = fixture.path().join("pi");
    std::fs::create_dir(&pi_root).unwrap();
    let invalid_root = fixture.path().join("invalid-root");
    let (mut tailer, _) = SessionTailer::initialize(
      vec![
        ProviderRoot::new(Provider::Pi, pi_root.clone()),
        ProviderRoot::new(Provider::Pi, invalid_root.clone()),
      ],
      NewFileReplay::All,
    )
    .unwrap();

    let _socket = std::os::unix::net::UnixListener::bind(&invalid_root).unwrap();
    std::fs::write(
      pi_root.join("session_fresh.jsonl"),
      "{\"type\":\"session\",\"id\":\"fresh\"}\n",
    )
    .unwrap();

    let update = tailer.scan().unwrap();
    assert!(update.records.iter().any(|record| record.topic == "pi.fresh"));
    assert!(update.warnings.iter().any(|warning| warning.contains("invalid-root")));
  }

  #[cfg(unix)]
  #[test]
  fn symlinked_provider_root_does_not_follow_nested_directory_loop() {
    let fixture = TempDir::new().unwrap();
    let actual_root = fixture.path().join("sessions");
    std::fs::create_dir(&actual_root).unwrap();
    let session = actual_root.join("session_one.jsonl");
    std::fs::write(&session, "{\"type\":\"session\",\"id\":\"one\"}\n").unwrap();
    std::os::unix::fs::symlink(&actual_root, actual_root.join("loop")).unwrap();
    let linked_root = fixture.path().join("linked-sessions");
    std::os::unix::fs::symlink(&actual_root, &linked_root).unwrap();

    let (mut tailer, initial) =
      SessionTailer::initialize(vec![ProviderRoot::new(Provider::Pi, linked_root)], NewFileReplay::All).unwrap();
    assert!(initial.records.is_empty());
    assert!(initial.warnings.is_empty());

    append(
      &session,
      "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
    );
    let update = tailer.scan().unwrap();
    assert_eq!(update.records.len(), 1);
    assert_eq!(update.records[0].topic, "pi.one");
    assert!(update.warnings.is_empty());
  }

  #[test]
  fn opencode_wakes_for_sidecars_but_versions_only_durable_data() {
    let fixture = TempDir::new().unwrap();
    let database = fixture.path().join("opencode.db");
    let wal = fixture.path().join("opencode.db-wal");
    let shm = fixture.path().join("opencode.db-shm");
    std::fs::write(&database, b"database").unwrap();
    std::fs::write(&wal, b"wal").unwrap();
    std::fs::write(&shm, b"shm").unwrap();
    let state = OpenCodeState::new(fixture.path().to_path_buf());

    assert!(state.matches_path(&database));
    assert!(state.matches_path(&wal));
    assert!(state.matches_path(&shm));
    assert!(state.matches_path(fixture.path()));
    assert!(!state.matches_path(&fixture.path().join("opencode.log")));
    assert!(!state.matches_path(&fixture.path().join("storage/snapshot")));

    let version = state.database_version();
    std::fs::write(&shm, b"reader-owned shm change").unwrap();
    assert_eq!(state.database_version(), version);
    std::fs::write(&wal, b"durable wal change").unwrap();
    assert_ne!(state.database_version(), version);
  }

  #[test]
  fn preserves_a_partial_line_present_at_startup() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"type\":\"session\",\"id\":\"pi-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}"
      ),
    )
    .unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
    )
    .unwrap();

    append(&path, "\n");
    let update = tailer.scan().unwrap();
    assert_eq!(update.records.len(), 1);
    let AgentEvent::Message(message) = &update.records[0].record.events[0] else {
      panic!("expected message");
    };
    assert_eq!(message.text, "hello");
  }

  #[test]
  fn trailing_partial_line_handles_complete_and_long_incomplete_lines() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("rollout.jsonl");
    let mut complete = vec![b'x'; 16 * 1024];
    complete.push(b'\n');
    std::fs::write(&path, &complete).unwrap();
    let mut file = File::open(&path).unwrap();
    assert!(
      trailing_partial_line(&mut file, &path, complete.len() as u64)
        .unwrap()
        .is_empty()
    );

    let partial = vec![b'y'; 16 * 1024 + 1];
    complete.extend_from_slice(&partial);
    std::fs::write(&path, &complete).unwrap();
    let mut file = File::open(&path).unwrap();
    assert_eq!(
      trailing_partial_line(&mut file, &path, complete.len() as u64).unwrap(),
      partial
    );
  }

  #[test]
  fn replays_all_events_for_new_files() {
    let fixture = TempDir::new().unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::All,
    )
    .unwrap();
    let path = fixture.path().join("session_new.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"type\":\"session\",\"id\":\"new-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"old\",\"message\":{\"role\":\"user\",\"content\":\"old\"}}\n",
        "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n"
      ),
    )
    .unwrap();

    let update = tailer.scan_paths(HashSet::from([path])).unwrap();
    assert_eq!(update.records.len(), 3);
    assert!(update.records.iter().all(|event| event.topic == "pi.new-session"));
  }

  #[test]
  fn backfills_new_files_from_the_third_most_recent_message() {
    let fixture = TempDir::new().unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
    )
    .unwrap();
    let path = fixture.path().join("session_new.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"type\":\"session\",\"id\":\"new-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"1\",\"message\":{\"role\":\"user\",\"content\":\"one\"}}\n",
        "{\"type\":\"error\",\"message\":\"before window\"}\n",
        "{\"type\":\"message\",\"id\":\"2\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"two\"}]}}\n",
        "{\"type\":\"message\",\"id\":\"3\",\"message\":{\"role\":\"user\",\"content\":\"three\"}}\n",
        "{\"type\":\"error\",\"message\":\"inside window\"}\n",
        "{\"type\":\"message\",\"id\":\"4\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"four\"}]}}\n",
        "{\"type\":\"custom_message\",\"id\":\"hidden\",\"parentId\":\"4\",\"timestamp\":\"2026-01-01\",\"customType\":\"plugin\",\"display\":false,\"content\":\"hidden\"}\n",
        "{\"type\":\"message\",\"id\":\"5\",\"message\":{\"role\":\"user\",\"content\":\"five\"}}\n"
      ),
    )
    .unwrap();

    let update = tailer.scan_paths(HashSet::from([path])).unwrap();
    assert_eq!(update.records.len(), 5);
    assert_eq!(
      update
        .records
        .iter()
        .filter(|event| event.record.events[0].is_hidden())
        .count(),
      1
    );
    let texts = update
      .records
      .iter()
      .filter(|event| !event.record.events[0].is_hidden())
      .filter_map(|event| match &event.record.events[0] {
        AgentEvent::Message(message) => Some(message.text.as_str()),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(texts, ["three", "four", "five"]);
    assert!(
      update
        .records
        .iter()
        .any(|event| matches!(event.record.events[0], AgentEvent::Error(_)))
    );
  }

  #[test]
  fn codex_tail_restores_accounting_without_replaying_historical_snapshots() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("rollout-usage.jsonl");
    let counters = serde_json::json!({"input_tokens":100,"cached_input_tokens":20,
      "output_tokens":5,"reasoning_output_tokens":2,"total_tokens":105});
    let record = serde_json::json!({"type":"event_msg","payload":{"type":"token_count",
      "info":{"total_token_usage":counters,"last_token_usage":counters},"rate_limits":null}});
    let accounting_line = format!("{record}\n");
    std::fs::write(
      &path,
      format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"accounting-session\"}}}}\n{accounting_line}"),
    )
    .unwrap();
    let (mut tailer, initial) = SessionTailer::initialize_with_native(
      vec![ProviderRoot::new(Provider::Codex, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
      true,
    )
    .unwrap();
    assert!(initial.records.is_empty());
    append(&path, &accounting_line);
    append(&path, &accounting_line);
    let update = tailer.scan_paths(HashSet::from([path])).unwrap();
    assert!(update.warnings.is_empty());
    assert_eq!(update.records.len(), 2);
    assert_eq!(update.records[0].topic, "codex.accounting-session");
    assert!(update.records[0].record.events.is_empty());
    assert!(update.records[1].record.events.is_empty());
    assert_eq!(update.records[1].record.native.as_ref(), Some(&record));
    assert_ne!(update.records[0].record.record_id, update.records[1].record.record_id);
    let changed = serde_json::json!({"type":"event_msg","payload":{"type":"token_count",
      "info":{"total_token_usage":{"input_tokens":110,"cached_input_tokens":20,
        "output_tokens":5,"reasoning_output_tokens":2,"total_tokens":115},
        "last_token_usage":counters},"rate_limits":null}});
    append(&update.records[0].path, &format!("{changed}\n{changed}\n"));
    let update = tailer.scan().unwrap();
    assert_eq!(update.records.len(), 2);
    assert!(matches!(&update.records[0].record.events[0], AgentEvent::Usage(event)
      if event.kind == tokn_session_core::UsageKind::SessionSnapshot
        && event.input_tokens == 110 && event.total_tokens == Some(115)));
    assert!(update.records[1].record.events.is_empty());
  }

  #[test]
  fn codex_restores_requests_and_settings_before_a_split_startup_reply() {
    for mode in ["legacy", "paginated"] {
      let fixture = TempDir::new().unwrap();
      let path = fixture.path().join("rollout-questions.jsonl");
      let mut history = format!(
        "{}\n",
        serde_json::json!({"type":"session_meta","payload":{"id":"questions-session",
          "history_mode":mode,"cwd":"/old","timestamp":"2026-10-09T00:00:00Z"}})
      );
      // Required state can occur arbitrarily far beyond the cheap startup header.
      for _ in 0..70 {
        history.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"old\"}}\n");
      }
      history.push_str(&format!(
        "{}\n{}\n",
        serde_json::json!({"type":"event_msg","payload":{"type":"thread_settings_applied",
          "thread_settings":{"cwd":"/new"}}}),
        serde_json::json!({"type":"response_item","payload":{"type":"function_call",
          "name":"request_user_input","call_id":"call-1","arguments":serde_json::json!({"questions":[
            {"id":"storage","header":"Storage","question":"Which storage?",
              "options":[{"label":"SQLite","description":"Local"}]}
          ]}).to_string()}})
      ));
      let output = format!(
        "{}\n",
        serde_json::json!({"type":"response_item","payload":{"type":"function_call_output",
          "call_id":"call-1","output":serde_json::json!({"answers":{"storage":{"answers":["数据库"]}}}).to_string()}})
      );
      let split = output.find('数').unwrap() + 1;
      let mut initial_bytes = history.into_bytes();
      initial_bytes.extend_from_slice(&output.as_bytes()[..split]);
      std::fs::write(&path, initial_bytes).unwrap();
      let mut tailer = SessionTailer::prepare(
        vec![ProviderRoot::new(Provider::Codex, fixture.path().to_path_buf())],
        NewFileReplay::Messages(3),
      )
      .unwrap();
      // Completion racing startup must remain a live observation exactly once.
      OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&output.as_bytes()[split..])
        .unwrap();
      let initial = tailer.start().unwrap();
      assert!(initial.records.is_empty());
      assert!(initial.warnings.is_empty());
      assert!(tailer.files[&path].codex_seed_length.is_some());

      let update = tailer.scan_paths(HashSet::from([path.clone()])).unwrap();
      assert!(update.warnings.is_empty());
      assert_eq!(update.records.len(), 1);
      assert_eq!(update.records[0].session.cwd.as_deref(), Some("/new"));
      let AgentEvent::QuestionReply(reply) = &update.records[0].record.events[0] else {
        panic!("expected restored question reply in {mode} mode");
      };
      assert_eq!(reply.request_id.as_deref(), Some("call-1"));
      assert_eq!(reply.replies[0].question.as_deref(), Some("Which storage?"));
      assert_eq!(reply.replies[0].answers, ["数据库"]);
      assert!(tailer.files[&path].codex_seed_length.is_none());
      assert!(tailer.scan().unwrap().records.is_empty());
    }
  }

  #[test]
  fn codex_restores_preexisting_code_mode_calls_before_their_results() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("rollout-code-mode.jsonl");
    let mut lines = include_str!("../tests/fixtures/codex/code_mode_wrappers.jsonl").lines();
    let header = lines.next().unwrap();
    let invocation = lines.next().unwrap();
    let output = lines.next().unwrap();
    std::fs::write(&path, format!("{header}\n{invocation}\n")).unwrap();
    let (mut tailer, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, fixture.path().to_path_buf())],
      NewFileReplay::All,
    )
    .unwrap();
    assert!(initial.records.is_empty());
    append(&path, &format!("{output}\n"));
    let update = tailer.scan().unwrap();
    assert!(update.warnings.is_empty());
    assert_eq!(update.records.len(), 1);
    let AgentEvent::ToolCall(result) = &update.records[0].record.events[0] else {
      panic!("expected code-mode result");
    };
    assert_eq!(result.tool_name.as_deref(), Some("write_stdin"));
    assert_eq!(result.provider_tool_name.as_deref(), Some("exec"));
    assert_eq!(result.turn_id.as_deref(), Some("turn-write"));
    assert_eq!(result.input.as_ref().unwrap()["session_id"], 90855);
    assert_eq!(result.output.as_ref().unwrap()["text"], "Refreshing checks status");
    assert_eq!(result.transport, Some(tokn_session_core::ToolTransport::CodeExecution));
  }

  #[test]
  fn codex_restores_preexisting_compaction_correlation() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("rollout-compaction.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"compaction-session\"}}\n",
        "{\"type\":\"compacted\",\"payload\":{\"message\":\"Checkpoint\"}}\n"
      ),
    )
    .unwrap();
    let (mut tailer, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, fixture.path().to_path_buf())],
      NewFileReplay::All,
    )
    .unwrap();
    assert!(initial.records.is_empty());
    append(
      &path,
      "{\"type\":\"event_msg\",\"payload\":{\"type\":\"context_compacted\"}}\n",
    );
    let update = tailer.scan().unwrap();
    assert!(update.warnings.is_empty());
    assert_eq!(update.records.len(), 1);
    let AgentEvent::Compaction(completion) = &update.records[0].record.events[0] else {
      panic!("expected correlated compaction completion");
    };
    assert_eq!(completion.compaction_id.as_deref(), Some("checkpoint:1"));
  }

  #[test]
  fn codex_replacement_discards_deferred_seed_state() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("rollout-replaced.jsonl");
    std::fs::write(
      &path,
      "{\"type\":\"session_meta\",\"payload\":{\"id\":\"old-session\"}}\n",
    )
    .unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, fixture.path().to_path_buf())],
      NewFileReplay::All,
    )
    .unwrap();
    assert!(tailer.files[&path].codex_seed_length.is_some());
    let replacement = fixture.path().join("replacement");
    std::fs::write(
      &replacement,
      concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"new-session\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"new\"}}\n"
      ),
    )
    .unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let update = tailer.scan().unwrap();
    assert!(update.warnings.is_empty());
    assert_eq!(update.records.len(), 2);
    assert!(update.records.iter().all(|record| record.topic == "codex.new-session"));
    assert!(tailer.files[&path].codex_seed_length.is_none());
  }

  #[test]
  fn seeds_codex_context_before_following() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("rollout-session-fixture.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"timestamp\":\"2026-06-04T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{",
        "\"id\":\"codex-session\",",
        "\"parent_thread_id\":\"parent-session\",",
        "\"agent_path\":\"/root/researcher\",",
        "\"agent_nickname\":\"Hubble\",",
        "\"agent_role\":\"explorer\",",
        "\"timestamp\":\"2026-06-04T00:00:00Z\",",
        "\"cwd\":\"/tmp/worktree\",",
        "\"model_provider\":\"openai\",",
        "\"git\":{",
        "\"commit_hash\":\"abcdef123456\",",
        "\"branch\":\"main\",",
        "\"repository_url\":\"https://github.com/agentic-rs/tokn-session.git\"",
        "}}}\n",
      ),
    )
    .unwrap();
    let (mut tailer, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
    )
    .unwrap();
    assert!(initial.records.is_empty());

    append(
      &path,
      concat!(
        "{\"timestamp\":\"2026-06-04T00:00:01Z\",\"type\":\"event_msg\",\"payload\":{",
        "\"type\":\"thread_settings_applied\",",
        "\"thread_settings\":{\"model\":\"gpt-5\",\"cwd\":\"/tmp/worktree/subdir\"}}}\n",
        "{\"timestamp\":\"2026-06-04T00:00:02Z\",\"type\":\"event_msg\",",
        "\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n",
      ),
    );
    let update = tailer.scan_paths(HashSet::from([path])).unwrap();
    assert_eq!(update.records.len(), 2);
    assert_eq!(update.records[0].topic, "codex.codex-session");
    let context = &update.records[0].session;
    assert_eq!(context.session_id, "codex-session");
    assert_eq!(context.parent_session_id.as_deref(), Some("parent-session"));
    assert_eq!(context.agent_path.as_deref(), Some("/root/researcher"));
    assert_eq!(context.agent_nickname.as_deref(), Some("Hubble"));
    assert_eq!(context.agent_role.as_deref(), Some("explorer"));
    assert_eq!(context.cwd.as_deref(), Some("/tmp/worktree/subdir"));
    assert_eq!(context.started_at.as_deref(), Some("2026-06-04T00:00:00Z"));
    let relay_json = serde_json::to_value(&update.records[0]).unwrap();
    assert_eq!(relay_json["session"]["agent_path"], "/root/researcher");
    assert_eq!(relay_json["session"]["agent_nickname"], "Hubble");
    assert_eq!(relay_json["session"]["agent_role"], "explorer");
    let project = context.project.as_ref().unwrap();
    assert_eq!(
      project.id.as_deref(),
      Some("https://github.com/agentic-rs/tokn-session.git")
    );
    assert_eq!(project.name.as_deref(), Some("tokn-session"));
    assert_eq!(project.project_name, None);
    assert_eq!(project.folder.as_deref(), Some("/tmp/worktree"));
    assert_eq!(project.folder_name.as_deref(), Some("worktree"));
    assert_eq!(project.repository_name.as_deref(), Some("tokn-session"));
    assert_eq!(project.branch.as_deref(), Some("main"));
    assert_eq!(project.commit_hash.as_deref(), Some("abcdef123456"));
    let AgentEvent::SessionSettingsApplied(settings) = &update.records[0].record.events[0] else {
      panic!("expected settings event");
    };
    assert_eq!(settings.cwd.as_deref(), Some("/tmp/worktree/subdir"));
    let AgentEvent::Message(message) = &update.records[1].record.events[0] else {
      panic!("expected message");
    };
    assert_eq!(message.text, "hello");
    assert_eq!(message.session_id.as_deref(), Some("codex-session"));
  }

  #[test]
  fn adds_desktop_project_folder_and_repository_names_to_codex_events() {
    let fixture = TempDir::new().unwrap();
    let sessions = fixture.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    std::fs::write(
      fixture.path().join(".codex-global-state.json"),
      r#"{
        "local-projects": {
          "project-id": {
            "id": "project-id",
            "name": "llm-router_2",
            "rootPaths": ["/workspace/llm-router"]
          }
        },
        "thread-project-assignments": {
          "root-session": {
            "projectKind": "local",
            "projectId": "project-id",
            "cwd": "/workspace/llm-router"
          }
        }
      }"#,
    )
    .unwrap();
    let path = sessions.join("rollout-session-fixture.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"timestamp\":\"2026-06-04T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{",
        "\"id\":\"child-session\",",
        "\"parent_thread_id\":\"root-session\",",
        "\"timestamp\":\"2026-06-04T00:00:00Z\",",
        "\"cwd\":\"/workspace/llm-router\",",
        "\"git\":{\"repository_url\":\"https://github.com/agentic-rs/tokn\"}",
        "}}\n",
      ),
    )
    .unwrap();

    let (mut tailer, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, sessions)],
      NewFileReplay::Messages(3),
    )
    .unwrap();
    assert!(initial.records.is_empty());
    assert!(initial.warnings.is_empty());

    append(
      &path,
      "{\"timestamp\":\"2026-06-04T00:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n",
    );
    let update = tailer.scan_paths(HashSet::from([path])).unwrap();
    assert_eq!(update.records.len(), 1);
    let project = update.records[0].session.project.as_ref().unwrap();
    assert_eq!(project.name.as_deref(), Some("tokn"));
    assert_eq!(project.project_name.as_deref(), Some("llm-router_2"));
    assert_eq!(project.folder.as_deref(), Some("/workspace/llm-router"));
    assert_eq!(project.folder_name.as_deref(), Some("llm-router"));
    assert_eq!(project.repository_name.as_deref(), Some("tokn"));
    assert_eq!(
      project.repository_url.as_deref(),
      Some("https://github.com/agentic-rs/tokn")
    );
  }

  #[test]
  fn reports_an_invalid_desktop_project_catalog_without_stopping() {
    let fixture = TempDir::new().unwrap();
    let sessions = fixture.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    std::fs::write(fixture.path().join(".codex-global-state.json"), "{not json").unwrap();

    let (_, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, sessions)],
      NewFileReplay::Messages(3),
    )
    .unwrap();

    assert!(initial.records.is_empty());
    assert_eq!(initial.warnings.len(), 1);
    assert!(initial.warnings[0].contains("Codex Desktop project catalog"));
  }

  #[test]
  fn reloads_desktop_project_catalog_after_startup() {
    let fixture = TempDir::new().unwrap();
    let sessions = fixture.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let path = sessions.join("rollout-session-fixture.jsonl");
    std::fs::write(
      &path,
      concat!(
        "{\"timestamp\":\"2026-06-04T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{",
        "\"id\":\"child-session\",",
        "\"parent_thread_id\":\"root-session\",",
        "\"cwd\":\"/workspace/llm-router\"",
        "}}\n",
      ),
    )
    .unwrap();
    let (mut tailer, initial) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Codex, sessions)],
      NewFileReplay::Messages(3),
    )
    .unwrap();
    assert!(initial.warnings.is_empty());

    let state_path = fixture.path().join(".codex-global-state.json");
    std::fs::write(
      &state_path,
      r#"{
        "local-projects": {
          "project-id": {
            "name": "llm-router_2",
            "rootPaths": ["/workspace/llm-router"]
          }
        },
        "thread-project-assignments": {
          "root-session": {
            "projectKind": "local",
            "projectId": "project-id"
          }
        }
      }"#,
    )
    .unwrap();
    append(
      &path,
      "{\"timestamp\":\"2026-06-04T00:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"first\"}}\n",
    );
    let first = tailer.scan_paths(HashSet::from([path.clone()])).unwrap();
    assert_eq!(
      first.records[0]
        .session
        .project
        .as_ref()
        .and_then(|project| project.project_name.as_deref()),
      Some("llm-router_2")
    );

    let replacement = fixture.path().join("replacement-state.json");
    std::fs::write(
      &replacement,
      r#"{
        "local-projects": {
          "project-id": {
            "name": "llm-router-renamed",
            "rootPaths": ["/workspace/llm-router"]
          }
        },
        "thread-project-assignments": {
          "root-session": {
            "projectKind": "local",
            "projectId": "project-id"
          }
        }
      }"#,
    )
    .unwrap();
    std::fs::remove_file(&state_path).unwrap();
    std::fs::rename(replacement, &state_path).unwrap();
    append(
      &path,
      "{\"timestamp\":\"2026-06-04T00:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"second\"}}\n",
    );
    let second = tailer.scan_paths(HashSet::from([path.clone()])).unwrap();
    assert_eq!(
      second.records[0]
        .session
        .project
        .as_ref()
        .and_then(|project| project.project_name.as_deref()),
      Some("llm-router-renamed")
    );

    std::fs::remove_file(&state_path).unwrap();
    append(
      &path,
      "{\"timestamp\":\"2026-06-04T00:00:03Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"third\"}}\n",
    );
    let third = tailer.scan_paths(HashSet::from([path])).unwrap();
    assert_eq!(
      third.records[0]
        .session
        .project
        .as_ref()
        .and_then(|project| project.project_name.as_deref()),
      None
    );
  }

  #[test]
  fn follows_atomically_replaced_session_files() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("session_test.jsonl");
    std::fs::write(
      &path,
      "{\"type\":\"session\",\"id\":\"old-session\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n",
    )
    .unwrap();
    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())],
      NewFileReplay::Messages(3),
    )
    .unwrap();

    let replacement = fixture.path().join("replacement");
    std::fs::write(
      &replacement,
      concat!(
        "{\"type\":\"session\",\"id\":\"new-session\",\"timestamp\":\"2026-01-02\",\"cwd\":\"/tmp\"}\n",
        "{\"type\":\"message\",\"id\":\"new\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n"
      ),
    )
    .unwrap();
    std::fs::rename(replacement, &path).unwrap();

    let update = tailer.scan_paths(HashSet::from([path])).unwrap();
    assert_eq!(update.records.len(), 2);
    assert!(update.records.iter().all(|event| event.topic == "pi.new-session"));
  }

  #[test]
  fn tails_opencode_database_updates() {
    let fixture = TempDir::new().unwrap();
    let database = fixture.path().join("opencode.db");
    let connection = Connection::open(&database).unwrap();
    connection
      .execute_batch(
        "pragma journal_mode = wal;
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
         );",
      )
      .unwrap();
    drop(connection);

    let (mut tailer, initial) = SessionTailer::initialize_with_native(
      vec![ProviderRoot::new(Provider::OpenCode, database.clone())],
      NewFileReplay::Messages(3),
      true,
    )
    .unwrap();
    assert!(initial.records.is_empty());

    let connection = Connection::open(&database).unwrap();
    connection
      .execute(
        "insert into session (id, parent_id, directory, time_created, time_updated) values (?1, null, ?2, ?3, ?4)",
        params!["ses_1", "/tmp/opencode", 1, 2],
      )
      .unwrap();
    insert_opencode_message(&connection, "msg_user", "ses_1", 1, r#"{"role":"user"}"#);
    insert_opencode_part(
      &connection,
      "part_user",
      "msg_user",
      "ses_1",
      1,
      r#"{"type":"text","text":"hello"}"#,
    );

    let first = tailer.scan().unwrap();
    assert_eq!(first.records.len(), 2);
    assert!(first.records.iter().all(|event| event.topic == "opencode.ses_1"));
    assert!(
      first
        .records
        .iter()
        .any(|event| matches!(event.record.events[0], AgentEvent::SessionStarted(_)))
    );
    assert!(
      first
        .records
        .iter()
        .any(|event| matches!(event.record.events[0], AgentEvent::Message(_)))
    );

    insert_opencode_message(
      &connection,
      "msg_assistant",
      "ses_1",
      3,
      r#"{"role":"assistant","parentID":"msg_user"}"#,
    );
    insert_opencode_part(
      &connection,
      "part_assistant",
      "msg_assistant",
      "ses_1",
      3,
      r#"{"type":"text","text":"world"}"#,
    );

    let second = tailer.scan().unwrap();
    assert_eq!(second.records.len(), 1);
    let AgentEvent::Message(message) = &second.records[0].record.events[0] else {
      panic!("expected assistant message");
    };
    assert_eq!(message.text, "world");
    assert_eq!(second.records[0].record.record_id, "message:msg_assistant");
    assert_eq!(
      second.records[0].record.native.as_ref().unwrap()["parts"][0]["data"]["text"],
      "world"
    );

    connection
      .execute(
        "update part set data = ?1 where id = ?2",
        params![r#"{"type":"text","text":"updated"}"#, "part_assistant"],
      )
      .unwrap();
    let third = tailer.scan().unwrap();
    assert_eq!(third.records.len(), 1);
    let AgentEvent::Message(message) = &third.records[0].record.events[0] else {
      panic!("expected updated assistant message");
    };
    assert_eq!(message.text, "updated");
    assert_eq!(third.records[0].record.record_id, second.records[0].record.record_id);

    connection
      .execute(
        "update part set data = ?1 where id = 'part_assistant'",
        params![r#"{"type":"text","text":"updated","future_field":42}"#],
      )
      .unwrap();
    let native_only = tailer.scan().unwrap();
    assert_eq!(native_only.records.len(), 1);
    assert_eq!(
      serde_json::to_value(&native_only.records[0].record.events).unwrap(),
      serde_json::to_value(&third.records[0].record.events).unwrap()
    );
    assert_eq!(
      native_only.records[0].record.native.as_ref().unwrap()["parts"][0]["data"]["future_field"],
      42
    );

    // Inserting an earlier message must not re-emit later, unchanged records.
    insert_opencode_message(&connection, "msg_earlier", "ses_1", 0, r#"{"role":"user"}"#);
    insert_opencode_part(
      &connection,
      "part_earlier",
      "msg_earlier",
      "ses_1",
      0,
      r#"{"type":"text","text":"earlier"}"#,
    );
    let fourth = tailer.scan().unwrap();
    assert_eq!(fourth.records.len(), 1);
    assert_eq!(fourth.records[0].record.record_id, "message:msg_earlier");

    // A second part belongs to the same replacement snapshot, not a new record.
    insert_opencode_part(
      &connection,
      "part_more",
      "msg_assistant",
      "ses_1",
      4,
      r#"{"type":"text","text":"more"}"#,
    );
    let fifth = tailer.scan().unwrap();
    assert_eq!(fifth.records.len(), 1);
    assert_eq!(fifth.records[0].record.record_id, "message:msg_assistant");
    assert_eq!(fifth.records[0].record.events.len(), 2);
    assert_eq!(
      fifth.records[0].record.native.as_ref().unwrap()["parts"]
        .as_array()
        .unwrap()
        .len(),
      2
    );

    connection
      .execute("delete from message where id = 'msg_earlier'", [])
      .unwrap();
    let removed = tailer.scan().unwrap();
    assert_eq!(removed.records.len(), 1);
    assert_eq!(removed.records[0].operation, super::RecordOperation::Remove);
    assert_eq!(removed.records[0].record.record_id, "message:msg_earlier");
    assert!(removed.records[0].record.events.is_empty());
    assert!(tailer.scan().unwrap().records.is_empty());
  }

  #[test]
  fn opencode_retries_a_failed_session_without_losing_other_sessions() {
    let fixture = TempDir::new().unwrap();
    let database = fixture.path().join("opencode.db");
    let connection = Connection::open(&database).unwrap();
    connection
      .execute_batch(
        "pragma journal_mode = wal;
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
         );",
      )
      .unwrap();

    let (mut tailer, _) = SessionTailer::initialize(
      vec![ProviderRoot::new(Provider::OpenCode, database)],
      NewFileReplay::All,
    )
    .unwrap();
    for (id, created) in [("good", 2), ("bad", 1)] {
      connection
        .execute(
          "insert into session (id, parent_id, directory, time_created, time_updated) values (?1, null, ?2, ?3, ?3)",
          params![id, "/tmp/opencode", created],
        )
        .unwrap();
    }
    insert_opencode_message(&connection, "msg_good", "good", 2, r#"{"role":"user"}"#);
    insert_opencode_part(
      &connection,
      "part_good",
      "msg_good",
      "good",
      2,
      r#"{"type":"text","text":"good"}"#,
    );
    insert_opencode_message(&connection, "msg_bad", "bad", 1, "invalid JSON");

    let first = tailer.scan().unwrap();
    assert!(first.records.iter().any(|record| record.topic == "opencode.good"));
    assert!(first.warnings.iter().any(|warning| warning.contains("session bad")));

    let unchanged_retry = tailer.scan().unwrap();
    assert!(unchanged_retry.records.is_empty());
    assert!(
      unchanged_retry
        .warnings
        .iter()
        .any(|warning| warning.contains("session bad"))
    );

    connection
      .execute(
        "update message set data = ?1 where id = 'msg_bad'",
        params![r#"{"role":"user"}"#],
      )
      .unwrap();
    let repaired = tailer.scan().unwrap();
    assert!(repaired.warnings.is_empty());
    assert!(repaired.records.iter().any(|record| record.topic == "opencode.bad"));
    assert!(!repaired.records.iter().any(|record| record.topic == "opencode.good"));
  }

  #[test]
  fn preserves_pi_record_batches_native_fields_and_byte_offsets() {
    let fixture = TempDir::new().unwrap();
    let path = fixture.path().join("pi.jsonl");
    let header = "{\"type\":\"session\",\"id\":\"pi-session\"}\n";
    std::fs::write(&path, header).unwrap();
    let roots = || vec![ProviderRoot::new(Provider::Pi, fixture.path().to_path_buf())];
    let (mut native, _) = SessionTailer::initialize_with_native(roots(), NewFileReplay::All, true).unwrap();
    let (mut plain, _) = SessionTailer::initialize(roots(), NewFileReplay::All).unwrap();
    let value = serde_json::json!({
      "type": "message", "id": "assistant-1", "future_field": {"answer": 42},
      "message": {"role": "assistant", "content": [
        {"type": "thinking", "thinking": "planning"},
        {"type": "text", "text": "hello"},
        {"type": "toolCall", "id": "call-1", "name": "read", "arguments": {"path": "a"}}
      ]}
    });
    let line = serde_json::to_string(&value).unwrap();
    append(&path, &line[..20]);
    assert!(native.scan().unwrap().records.is_empty());
    append(&path, &format!("{}\r\n", &line[20..]));
    let native_update = native.scan().unwrap();
    assert_eq!(native_update.records.len(), 1);
    let record = &native_update.records[0].record;
    assert_eq!(record.record_id, format!("jsonl:{}", header.len()));
    assert_eq!(record.native.as_ref(), Some(&value));
    assert_eq!(record.events.len(), 3);
    let plain_update = plain.scan().unwrap();
    let serialized = serde_json::to_value(&plain_update.records[0]).unwrap();
    assert!(serialized.get("native").is_none());
    assert!(serialized.get("event").is_none());
    assert_eq!(serialized["events"], serde_json::to_value(&record.events).unwrap());
    assert_eq!(serialized["operation"], "upsert");

    // Add an earlier visible message so the replay cutoff actually advances.
    let mut records = plain_update.records;
    records[0].record.record_id = "earlier-record".into();
    records.extend(native_update.records);
    super::retain_message_history(&mut records, 1);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record.record_id, format!("jsonl:{}", header.len()));
    assert_eq!(
      records[0].record.events.len(),
      3,
      "replay must keep the entire source record"
    );
  }

  fn insert_opencode_message(connection: &Connection, id: &str, session_id: &str, time_created: i64, data: &str) {
    connection
      .execute(
        "insert into message (id, session_id, time_created, data) values (?1, ?2, ?3, ?4)",
        params![id, session_id, time_created, data],
      )
      .unwrap();
  }

  fn insert_opencode_part(
    connection: &Connection,
    id: &str,
    message_id: &str,
    session_id: &str,
    time_created: i64,
    data: &str,
  ) {
    connection
      .execute(
        "insert into part (id, message_id, session_id, time_created, data) values (?1, ?2, ?3, ?4, ?5)",
        params![id, message_id, session_id, time_created, data],
      )
      .unwrap();
  }

  fn append(path: &std::path::Path, value: &str) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(value.as_bytes()).unwrap();
    file.flush().unwrap();
  }
}
