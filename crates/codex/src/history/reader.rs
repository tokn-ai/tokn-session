use super::*;
use std::time::SystemTime;
use tokn_session_core::{SessionHistoryStatus, SessionRef};

const TAIL_GUARD_BYTES: usize = 4096;

/// Work performed by a reader, useful for diagnosing history amplification.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CodexHistoryReadStats {
  pub source_bytes_read: u64,
  pub rows_parsed: u64,
  pub guard_bytes_read: u64,
  pub lineage_resolutions: u64,
  pub boundary_bytes_read: u64,
  pub boundary_rows_scanned: u64,
}

/// Atomic normalized delta. A reset replaces the previous logical transcript.
pub struct CodexHistoryUpdate {
  pub reference: SessionRef,
  pub records: Vec<NormalizedRecord>,
  pub history_status: SessionHistoryStatus,
  pub reset: bool,
  /// Logical source byte anchor of a lazy range; None for full history.
  pub source_start: Option<u64>,
}

/// A verified lineage plus a cursor into its growing active JSONL segment.
///
/// Native rollout growth is treated as append-only. Replacement, truncation,
/// same-size edits, changed inherited files, and changed header/tail guards
/// rebuild the history. Arbitrary edits in the middle followed by growth cannot
/// be detected without rereading that history; they are outside this contract.
/// On any error the cursor is discarded, so retrying cannot skip a failed batch.
pub struct CodexHistoryReader {
  path: PathBuf,
  include_native: bool,
  max_bytes: usize,
  state: Option<ReaderState>,
  stats: CodexHistoryReadStats,
  window_turns: Option<usize>,
}

struct ReaderState {
  roots: Vec<PathBuf>,
  segments: Vec<CodexHistorySegment>,
  versions: Vec<FileVersion>,
  reference: SessionRef,
  normalizer: CodexNormalizer,
  prefix_bytes: u64,
  offset: u64,
  pending: Vec<u8>,
  ordinals: HistoryOrdinals,
  header_guard: Vec<u8>,
  tail_guard: Vec<u8>,
  prefix_guards: Vec<PrefixGuard>,
  source_start: u64,
  dependencies: super::window::Dependencies,
}

struct PrefixGuard {
  header: Vec<u8>,
  tail: Vec<u8>,
  cutoff: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileVersion {
  length: u64,
  modified: SystemTime,
  #[cfg(unix)]
  identity: (u64, u64),
  #[cfg(not(unix))]
  identity: Option<SystemTime>,
}

impl FileVersion {
  fn read(path: &Path) -> Result<Self, String> {
    let metadata = std::fs::metadata(path).map_err(|err| format!("failed to inspect {}: {err}", path.display()))?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Self {
      length: metadata.len(),
      modified: metadata.modified().map_err(|err| err.to_string())?,
      #[cfg(unix)]
      identity: (metadata.dev(), metadata.ino()),
      #[cfg(not(unix))]
      identity: metadata.created().ok(),
    })
  }
}

impl CodexHistoryReader {
  pub fn new(path: PathBuf, include_native: bool, max_bytes: usize) -> Self {
    Self {
      path,
      include_native,
      max_bytes,
      state: None,
      stats: CodexHistoryReadStats::default(),
      window_turns: None,
    }
  }

  pub fn stats(&self) -> CodexHistoryReadStats {
    self.stats
  }

  /// Start at the requested number of latest provable turns, leaving older history on disk.
  /// Formats without checkpoints and dependencies can require a wider range.
  pub fn new_window(path: PathBuf, include_native: bool, max_bytes: usize, turns: usize) -> Self {
    let mut reader = Self::new(path, include_native, max_bytes);
    reader.window_turns = Some(turns.max(1));
    reader
  }

  /// Older ranges are explicit; ordinary polls never slide an open window.
  pub fn expand(&mut self, turns: Option<usize>) {
    self.window_turns = turns.map(|turns| self.window_turns.unwrap_or(0).saturating_add(turns));
    self.invalidate();
  }

  /// Forces the next successful read to replace the current snapshot.
  pub fn invalidate(&mut self) {
    self.state = None;
  }

  pub fn poll(&mut self, source: &CodexSessionSource) -> Result<Option<CodexHistoryUpdate>, String> {
    // Take the state before doing fallible work. Only a fully successful read
    // returns it to the cache; callers retain their last-good published records.
    let Some(mut state) = self.state.take() else {
      return self.rebuild(source).map(Some);
    };
    if source.history_roots(&self.path)? != state.roots {
      return self.rebuild(source).map(Some);
    }
    let versions = state
      .segments
      .iter()
      .map(|segment| FileVersion::read(&segment.path))
      .collect::<Result<Vec<_>, _>>()?;
    if versions == state.versions {
      self.state = Some(state);
      return Ok(None);
    }
    let head = versions.last().unwrap();
    let previous = state.versions.last().unwrap();
    for (index, guard) in state.prefix_guards.iter().enumerate() {
      let before = &state.versions[index];
      let after = &versions[index];
      if after == before {
        continue;
      }
      if !guarded_prefix_growth(&state.segments[index].path, before, after, guard, &mut self.stats)? {
        return self.rebuild(source).map(Some);
      }
    }
    if head == previous {
      // A referenced parent can keep growing beyond this child's exclusive
      // cutoff. Its new rows do not change this already verified history.
      state.versions = versions;
      self.state = Some(state);
      return Ok(None);
    }
    if head.identity != previous.identity || head.length <= state.offset {
      return self.rebuild(source).map(Some);
    }
    if state
      .prefix_bytes
      .saturating_add(head.length)
      .saturating_sub(state.source_start)
      > self.max_bytes as u64
    {
      return Err("Codex history exceeds the snapshot size limit".into());
    }
    let mut file = File::open(&self.path).map_err(|err| err.to_string())?;
    if !guards_match(
      &mut file,
      &state.header_guard,
      &state.tail_guard,
      state.offset,
      &mut self.stats,
    )? {
      return self.rebuild(source).map(Some);
    }
    let appended = read_range(&mut file, state.offset, head.length - state.offset)?;
    self.stats.source_bytes_read += appended.len() as u64;
    update_tail(&mut state.tail_guard, &appended);
    state.offset = head.length;
    // Pending bytes contain no newline. A fragmented long row must not scan
    // its whole accumulated prefix every time another fragment arrives.
    let appended_complete = complete_length(&appended);
    let complete = if appended_complete == 0 {
      0
    } else {
      state.pending.len() + appended_complete
    };
    state.pending.extend(appended);
    let start = state.offset - state.pending.len() as u64;
    let mut records = Vec::new();
    let mut offset = start;
    for row in state.pending[..complete].split_inclusive(|byte| *byte == b'\n') {
      let row_offset = offset;
      offset += row.len() as u64;
      if row.iter().all(u8::is_ascii_whitespace) {
        continue;
      }
      let line = parse_row(row, &self.path, row_offset, &mut self.stats)?;
      state.ordinals.accept(&line, &self.path, row_offset)?;
      if matches!(line.item(), RolloutItem::SessionMeta(_)) {
        continue;
      }
      if super::window::is_turn_start(&line)
        && let Some(turns) = &mut self.window_turns
      {
        *turns = turns.saturating_add(1);
      }
      state.dependencies.observe(&line);
      records.push(normalize_record(
        line,
        &self.path,
        row_offset,
        self.include_native,
        false,
        &mut state.normalizer,
        &mut state.reference,
      ));
    }
    if state.source_start > 0 && state.dependencies.missing {
      self.widen_dependencies();
      return self.rebuild(source).map(Some);
    }
    state.pending.drain(..complete);
    validate_versions(&state.segments, &versions, &state.prefix_guards, &mut self.stats)?;
    state.versions = versions;
    let update = (!records.is_empty()).then(|| CodexHistoryUpdate {
      reference: state.reference.clone(),
      records,
      history_status: state.normalizer.history_status(),
      reset: false,
      source_start: self.window_turns.map(|_| state.source_start),
    });
    self.state = Some(state);
    Ok(update)
  }

  fn rebuild(&mut self, source: &CodexSessionSource) -> Result<CodexHistoryUpdate, String> {
    self.stats.lineage_resolutions += 1;
    let roots = source.history_roots(&self.path)?;
    let segments = source.history_segments(&self.path)?;
    let versions = segments
      .iter()
      .map(|segment| FileVersion::read(&segment.path))
      .collect::<Result<Vec<_>, _>>()?;
    let owner = history_header(&self.path)?;
    if segments
      .last()
      .is_none_or(|segment| segment.header_key != header_key(&owner))
    {
      return Err("Codex history changed while resolving its prefix".into());
    }
    let thread_spawn =
      matches!(owner.item(), RolloutItem::SessionMeta(meta) if crate::normalize::requires_thread_spawn_boundary(meta));
    let mut reference = inspect_session_header(&self.path)?;
    if owner.native()["payload"]["id"].as_str() != Some(reference.id.as_str()) {
      return Err("Codex history owner changed while reading its metadata".into());
    }
    source.apply_indexed_metadata(std::slice::from_mut(&mut reference));
    let lengths = segments
      .iter()
      .zip(&versions)
      .map(|(segment, version)| segment.end_byte_offset.unwrap_or(version.length))
      .collect::<Vec<_>>();
    // Thread-spawn filtering requires an earlier trigger; keep its proven full
    // reader until a provider checkpoint can seed that boundary safely.
    let source_start = if !thread_spawn {
      match self.window_turns {
        Some(turns) => super::window::start(&segments, &lengths, turns, &mut self.stats)?,
        None => 0,
      }
    } else {
      0
    };
    let mut normalizer = CodexNormalizer::new_historical();
    let owner_events = normalizer.normalize(owner.clone());
    let mut records = if source_start == 0 {
      vec![NormalizedRecord {
        record_id: format!("session:{}", reference.id),
        native: self.include_native.then(|| owner.native().clone()),
        events: owner_events,
      }]
    } else {
      Vec::new()
    };
    let mut dependencies = super::window::Dependencies::default();
    let mut consumed = 0u64;
    let mut selected_bytes = 0u64;
    let mut head_state = None;
    let mut prefix_guards = Vec::new();
    for (segment, length) in segments.iter().zip(&lengths) {
      let length = *length;
      let start = source_start.saturating_sub(consumed).min(length);
      let mut file = File::open(&segment.path).map_err(|err| err.to_string())?;
      let header_guard = read_header_guard(&mut file)?;
      let header = history_header(&segment.path)?;
      if header_key(&header) != segment.header_key {
        return Err("Codex history changed while reading its prefix".into());
      }
      let tail_guard = read_range(
        &mut file,
        length.saturating_sub(TAIL_GUARD_BYTES as u64),
        length.min(TAIL_GUARD_BYTES as u64),
      )?;
      if selected_bytes.saturating_add(length - start) > self.max_bytes as u64 {
        return Err("Codex history exceeds the snapshot size limit".into());
      }
      let bytes = read_range(&mut file, start, length - start)?;
      selected_bytes += bytes.len() as u64;
      self.stats.source_bytes_read += bytes.len() as u64;
      let complete = complete_length(&bytes);
      if segment.end_byte_offset.is_some() && complete != bytes.len() {
        return Err("invalid Codex history lineage: cutoff is not a complete record".into());
      }
      let mut offset = start;
      let mut ordinals = HistoryOrdinals::new(segment.header_key["history_mode"].as_str() == Some("paginated"));
      let inherited_parent = thread_spawn && header.native()["payload"]["id"].as_str() != Some(reference.id.as_str());
      for row in bytes[..complete].split_inclusive(|byte| *byte == b'\n') {
        let row_offset = offset;
        offset += row.len() as u64;
        if row.iter().all(u8::is_ascii_whitespace) {
          continue;
        }
        let line = parse_row(row, &segment.path, row_offset, &mut self.stats)?;
        ordinals.accept(&line, &segment.path, row_offset)?;
        if matches!(line.item(), RolloutItem::SessionMeta(_)) {
          continue;
        }
        dependencies.observe(&line);
        records.push(normalize_record(
          line,
          &segment.path,
          row_offset,
          self.include_native,
          inherited_parent,
          &mut normalizer,
          &mut reference,
        ));
      }
      if start < length
        && segment
          .end_ordinal_exclusive
          .is_some_and(|end| !ordinals.ends_before(end))
      {
        return Err("invalid Codex history lineage: cutoff ordinal disagrees with its bytes".into());
      }
      if segment.end_byte_offset.is_none() {
        head_state = Some((
          consumed,
          length,
          bytes[complete..].to_vec(),
          ordinals,
          header_guard,
          tail_guard,
        ));
      } else {
        prefix_guards.push(PrefixGuard {
          header: header_guard,
          tail: tail_guard,
          cutoff: length,
        });
      }
      consumed += length;
    }
    // A result can arrive after its invocation's turn. Widen only when that
    // dependency is actually needed, before publishing a partial operation.
    if source_start > 0 && dependencies.missing {
      self.widen_dependencies();
      return self.rebuild(source);
    }
    validate_versions(&segments, &versions, &prefix_guards, &mut self.stats)?;
    let update = CodexHistoryUpdate {
      reference: reference.clone(),
      records,
      history_status: normalizer.history_status(),
      reset: true,
      source_start: self.window_turns.map(|_| source_start),
    };
    let (prefix_bytes, offset, pending, ordinals, header_guard, tail_guard) = head_state.unwrap();
    self.state = Some(ReaderState {
      roots,
      segments,
      versions,
      reference,
      normalizer,
      prefix_bytes,
      offset,
      pending,
      ordinals,
      header_guard,
      tail_guard,
      prefix_guards,
      source_start,
      dependencies,
    });
    Ok(update)
  }

  fn widen_dependencies(&mut self) {
    // Bound repeated parsing for distant or absent invocations.
    self.window_turns = self
      .window_turns
      .and_then(|turns| (turns < 32).then_some((turns * 2).min(32)));
  }
}

fn read_header_guard(file: &mut File) -> Result<Vec<u8>, String> {
  file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
  let mut reader = BufReader::new(Read::by_ref(file).take(MAX_HEADER_BYTES));
  let mut bytes = Vec::new();
  loop {
    let mut row = Vec::new();
    if reader.read_until(b'\n', &mut row).map_err(|e| e.to_string())? == 0 {
      return Err("Codex history metadata is not yet complete".into());
    }
    bytes.extend_from_slice(&row);
    if !row.iter().all(u8::is_ascii_whitespace) {
      let line: CodexLine = serde_json::from_slice(&row).map_err(|e| e.to_string())?;
      if matches!(line.item(), RolloutItem::SessionMeta(_)) {
        return Ok(bytes);
      }
    }
  }
}

fn read_range(file: &mut File, offset: u64, length: u64) -> Result<Vec<u8>, String> {
  file.seek(SeekFrom::Start(offset)).map_err(|err| err.to_string())?;
  let mut bytes = Vec::new();
  Read::by_ref(file)
    .take(length)
    .read_to_end(&mut bytes)
    .map_err(|err| err.to_string())?;
  if bytes.len() as u64 != length {
    return Err("Codex history changed while reading its prefix".into());
  }
  Ok(bytes)
}

fn guards_match(
  file: &mut File,
  header_guard: &[u8],
  tail_guard: &[u8],
  offset: u64,
  stats: &mut CodexHistoryReadStats,
) -> Result<bool, String> {
  let header = read_range(file, 0, header_guard.len() as u64)?;
  stats.guard_bytes_read += header.len() as u64;
  if header != header_guard {
    return Ok(false);
  }
  let tail = read_range(file, offset - tail_guard.len() as u64, tail_guard.len() as u64)?;
  stats.guard_bytes_read += tail.len() as u64;
  Ok(tail == tail_guard)
}

fn guarded_prefix_growth(
  path: &Path,
  before: &FileVersion,
  after: &FileVersion,
  guard: &PrefixGuard,
  stats: &mut CodexHistoryReadStats,
) -> Result<bool, String> {
  if after.identity != before.identity || after.length <= before.length {
    return Ok(false);
  }
  let mut file = File::open(path).map_err(|err| err.to_string())?;
  guards_match(&mut file, &guard.header, &guard.tail, guard.cutoff, stats)
}

fn update_tail(tail: &mut Vec<u8>, appended: &[u8]) {
  if appended.len() >= TAIL_GUARD_BYTES {
    tail.clear();
    tail.extend_from_slice(&appended[appended.len() - TAIL_GUARD_BYTES..]);
  } else {
    tail.extend_from_slice(appended);
    let excess = tail.len().saturating_sub(TAIL_GUARD_BYTES);
    tail.drain(..excess);
  }
}

fn complete_length(bytes: &[u8]) -> usize {
  bytes
    .iter()
    .rposition(|byte| *byte == b'\n')
    .map_or(0, |index| index + 1)
}

fn parse_row(row: &[u8], path: &Path, offset: u64, stats: &mut CodexHistoryReadStats) -> Result<CodexLine, String> {
  stats.rows_parsed += 1;
  serde_json::from_slice(row).map_err(|err| format!("invalid Codex history at {} byte {offset}: {err}", path.display()))
}

fn normalize_record(
  line: CodexLine,
  path: &Path,
  offset: u64,
  include_native: bool,
  inherited_parent: bool,
  normalizer: &mut CodexNormalizer,
  reference: &mut SessionRef,
) -> NormalizedRecord {
  let native = include_native.then(|| line.native().clone());
  let events = if inherited_parent {
    Vec::new()
  } else {
    normalizer.normalize(line)
  };
  reference.message_count += events
    .iter()
    .filter(|event| matches!(event, AgentEvent::Message(_)))
    .count();
  NormalizedRecord {
    record_id: format!("segment:{}:{offset}", path.display()),
    native,
    events,
  }
}

fn validate_versions(
  segments: &[CodexHistorySegment],
  versions: &[FileVersion],
  guards: &[PrefixGuard],
  stats: &mut CodexHistoryReadStats,
) -> Result<(), String> {
  for (index, (segment, before)) in segments.iter().zip(versions).enumerate() {
    let after = FileVersion::read(&segment.path)?;
    let valid = if segment.end_byte_offset.is_some() {
      after == *before || guarded_prefix_growth(&segment.path, before, &after, &guards[index], stats)?
    } else {
      after.identity == before.identity && (after == *before || after.length > before.length)
    };
    if !valid {
      return Err("Codex history changed while reading its prefix".into());
    }
  }
  Ok(())
}
