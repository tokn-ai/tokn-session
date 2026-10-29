use super::*;
use crate::storage::{CodexScratchBudget, ReadWorkBudget};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime};
use tokn_session_core::{SessionHistoryStatus, SessionRef};

const TAIL_GUARD_BYTES: usize = 4096;

/// Independent limits for record parsing, materialized batches, disk, and work.
/// None of these limits describes the total size of a streamed source.
#[derive(Clone)]
pub struct CodexHistoryLimits {
  pub max_record_bytes: usize,
  pub max_snapshot_bytes: usize,
  pub scratch: CodexScratchBudget,
  pub max_duration: Duration,
}

impl Default for CodexHistoryLimits {
  fn default() -> Self {
    Self {
      max_record_bytes: DEFAULT_RECORD_BYTES,
      max_snapshot_bytes: 128 * 1024 * 1024,
      scratch: CodexScratchBudget::default(),
      max_duration: Duration::from_secs(120),
    }
  }
}

pub(crate) const DEFAULT_RECORD_BYTES: usize = 128 * 1024 * 1024;

/// Work performed by a reader, useful for diagnosing history amplification.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CodexHistoryReadStats {
  /// Decoded bytes read for normalization. Boundary probes are counted below;
  /// streaming lineage verification and temporary decompression are excluded.
  pub source_bytes_read: u64,
  pub rows_parsed: u64,
  pub guard_bytes_read: u64,
  pub lineage_resolutions: u64,
  pub boundary_bytes_read: u64,
  pub boundary_rows_scanned: u64,
  pub model_checkpoint_bytes_read: u64,
  pub model_checkpoint_rows_scanned: u64,
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

/// Metadata for a successfully validated streaming projection update.
pub struct CodexHistoryProjectionUpdate {
  pub reference: SessionRef,
  pub history_status: SessionHistoryStatus,
  pub reset: bool,
}

/// A full-history cursor that delivers one bounded normalized record at a time.
///
/// Unlike a snapshot, a projection does not retain the complete transcript or
/// impose a source-size limit. `max_record_bytes` bounds complete and incomplete
/// rows; each poll has a separate work deadline. Streaming uses no scratch disk.
pub struct CodexHistoryProjectionReader {
  reader: CodexHistoryReader,
}

impl CodexHistoryProjectionReader {
  pub fn new(path: PathBuf, max_record_bytes: usize) -> Self {
    Self {
      reader: CodexHistoryReader::with_limits(
        path,
        false,
        CodexHistoryLimits {
          max_record_bytes,
          ..Default::default()
        },
      ),
    }
  }

  pub fn with_limits(path: PathBuf, limits: CodexHistoryLimits) -> Self {
    Self {
      reader: CodexHistoryReader::with_limits(path, false, limits),
    }
  }

  pub fn stats(&self) -> CodexHistoryReadStats {
    self.reader.stats()
  }

  /// Fold records into disposable staging state and publish it only after this
  /// call succeeds. Validation can fail after invoking `consume`; an error
  /// invalidates the cursor and the next poll starts again with `reset = true`.
  /// The first (owner) record of a rebuild has `reset = true`; all other records
  /// have `reset = false`. An unchanged source does not invoke `consume`.
  pub fn poll(
    &mut self,
    source: &CodexSessionSource,
    mut consume: impl FnMut(bool, NormalizedRecord),
  ) -> Result<Option<CodexHistoryProjectionUpdate>, String> {
    self.reader.poll_with(source, &mut Some(&mut consume)).map(|update| {
      update.map(|update| CodexHistoryProjectionUpdate {
        reference: update.reference,
        history_status: update.history_status,
        reset: update.reset,
      })
    })
  }

  pub fn invalidate(&mut self) {
    self.reader.invalidate();
  }
}

type ProjectionSink<'a> = Option<&'a mut dyn FnMut(bool, NormalizedRecord)>;

fn emit_record(
  projection: &mut ProjectionSink<'_>,
  records: &mut Vec<NormalizedRecord>,
  reset: bool,
  record: NormalizedRecord,
) {
  if let Some(consume) = projection {
    consume(reset, record);
  } else {
    records.push(record);
  }
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
  limits: CodexHistoryLimits,
  state: Option<ReaderState>,
  stats: CodexHistoryReadStats,
  window_turns: Option<usize>,
  model_checkpoint: Option<ModelCheckpoint>,
}

struct ModelCheckpoint {
  source_start: u64,
  versions: Vec<FileVersion>,
  baseline: Option<tokn_session_core::ModelObservation>,
}

struct ReaderState {
  roots: Vec<PathBuf>,
  segments: Vec<CodexHistorySegment>,
  versions: Vec<FileVersion>,
  reference: SessionRef,
  normalizer: CodexNormalizer,
  offset: u64,
  pending: Vec<u8>,
  ordinals: HistoryOrdinals,
  header_guard: HeaderGuard,
  tail_guard: Vec<u8>,
  prefix_guards: Vec<PrefixGuard>,
  source_start: u64,
  dependencies: super::window::Dependencies,
}

struct HeaderGuard {
  length: u64,
  digest: [u8; 32],
}

struct PrefixGuard {
  header: HeaderGuard,
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
    Self::with_limits(
      path,
      include_native,
      CodexHistoryLimits {
        max_snapshot_bytes: max_bytes,
        ..Default::default()
      },
    )
  }

  pub fn with_limits(path: PathBuf, include_native: bool, limits: CodexHistoryLimits) -> Self {
    Self {
      path,
      include_native,
      limits,
      state: None,
      stats: CodexHistoryReadStats::default(),
      window_turns: None,
      model_checkpoint: None,
    }
  }

  pub fn stats(&self) -> CodexHistoryReadStats {
    self.stats
  }

  /// Start at the requested number of latest provable turns, leaving older history on disk.
  /// Formats without checkpoints and dependencies can require a wider range.
  pub fn new_window(path: PathBuf, include_native: bool, max_bytes: usize, turns: usize) -> Self {
    Self::new_window_with_limits(
      path,
      include_native,
      CodexHistoryLimits {
        max_snapshot_bytes: max_bytes,
        ..Default::default()
      },
      turns,
    )
  }

  pub fn new_window_with_limits(path: PathBuf, include_native: bool, limits: CodexHistoryLimits, turns: usize) -> Self {
    let mut reader = Self::with_limits(path, include_native, limits);
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
    self.poll_with(source, &mut None)
  }

  fn poll_with(
    &mut self,
    source: &CodexSessionSource,
    projection: &mut ProjectionSink<'_>,
  ) -> Result<Option<CodexHistoryUpdate>, String> {
    let work = ReadWorkBudget::new(self.limits.max_duration);
    self.poll_budgeted(source, projection, &work)
  }

  fn poll_budgeted(
    &mut self,
    source: &CodexSessionSource,
    projection: &mut ProjectionSink<'_>,
    work: &ReadWorkBudget,
  ) -> Result<Option<CodexHistoryUpdate>, String> {
    // Take the state before doing fallible work. Only a fully successful read
    // returns it to the cache; callers retain their last-good published records.
    let previous = self.state.take();
    work.check()?;
    let Some(mut state) = previous else {
      return self.rebuild(source, projection, work).map(Some);
    };
    if source.history_roots(&self.path)? != state.roots {
      return self.rebuild(source, projection, work).map(Some);
    }
    if state
      .segments
      .iter()
      .any(|segment| crate::storage::resolve_path(&segment.path) != segment.path)
    {
      return self.rebuild(source, projection, work).map(Some);
    }
    let versions = state
      .segments
      .iter()
      .map(|segment| FileVersion::read(&segment.path))
      .collect::<Result<Vec<_>, _>>();
    // Native compression can retire a plain physical file while preserving
    // every decoded byte. Resolve the replacement before reporting it missing.
    let Ok(versions) = versions else {
      return self.rebuild(source, projection, work).map(Some);
    };
    if versions == state.versions {
      work.check()?;
      self.state = Some(state);
      return Ok(None);
    }
    if state
      .segments
      .iter()
      .zip(&state.versions)
      .zip(&versions)
      .any(|((segment, before), after)| crate::storage::is_compressed(&segment.path) && before != after)
    {
      // Compressed offsets describe decoded bytes, so physical growth cannot
      // use the append cursor. Rebuild and validate the entire selected range.
      return self.rebuild(source, projection, work).map(Some);
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
        return self.rebuild(source, projection, work).map(Some);
      }
    }
    if head == previous {
      // A referenced parent can keep growing beyond this child's exclusive
      // cutoff. Its new rows do not change this already verified history.
      state.versions = versions;
      work.check()?;
      self.state = Some(state);
      return Ok(None);
    }
    if head.identity != previous.identity || head.length <= state.offset {
      return self.rebuild(source, projection, work).map(Some);
    }
    if projection.is_none()
      && (state.pending.len() as u64).saturating_add(head.length - state.offset) > self.limits.max_snapshot_bytes as u64
    {
      return Err("Codex history exceeds the snapshot size limit".into());
    }
    let mut file = File::open(&state.segments.last().unwrap().path).map_err(|err| err.to_string())?;
    if !guards_match(
      &mut file,
      &state.header_guard,
      &state.tail_guard,
      state.offset,
      &mut self.stats,
    )? {
      return self.rebuild(source, projection, work).map(Some);
    }
    let mut records = Vec::new();
    let mut emitted = false;
    read_rows(
      &mut file,
      state.offset,
      head.length - state.offset,
      &mut state.pending,
      self.limits.max_record_bytes,
      work,
      &mut self.stats,
      Some(&mut state.tail_guard),
      |row, row_offset, stats| {
        let line = parse_row(row, &self.path, row_offset, stats)?;
        state.ordinals.accept(&line, &self.path, row_offset)?;
        if matches!(line.item(), RolloutItem::SessionMeta(_)) {
          return Ok(());
        }
        if let Some(turns) = &mut self.window_turns {
          if super::window::is_turn_start(&line) {
            *turns = turns.saturating_add(1);
          }
          state.dependencies.observe(&line);
        }
        emit_record(
          projection,
          &mut records,
          false,
          normalize_record(
            line,
            &self.path,
            row_offset,
            self.include_native,
            false,
            &mut state.normalizer,
            &mut state.reference,
          ),
        );
        emitted = true;
        Ok(())
      },
    )?;
    state.offset = head.length;
    if state.source_start > 0 && state.dependencies.missing {
      self.widen_dependencies();
      return self.rebuild(source, projection, work).map(Some);
    }
    validate_versions(&state.segments, &versions, &state.prefix_guards, &mut self.stats)?;
    state.versions = versions;
    work.check()?;
    let update = emitted.then(|| CodexHistoryUpdate {
      reference: state.reference.clone(),
      records,
      history_status: state.normalizer.history_status(),
      reset: false,
      source_start: self.window_turns.map(|_| state.source_start),
    });
    self.state = Some(state);
    Ok(update)
  }

  fn rebuild(
    &mut self,
    source: &CodexSessionSource,
    projection: &mut ProjectionSink<'_>,
    work: &ReadWorkBudget,
  ) -> Result<CodexHistoryUpdate, String> {
    self.stats.lineage_resolutions += 1;
    let roots = source.history_roots(&self.path)?;
    let segments = source.history_segments_with_budget(&self.path, self.limits.max_record_bytes, work)?;
    let versions = segments
      .iter()
      .map(|segment| FileVersion::read(&segment.path))
      .collect::<Result<Vec<_>, _>>()?;
    let owner = history_header_with_budget(&self.path, work)?;
    if segments
      .last()
      .is_none_or(|segment| segment.header_key != header_key(&owner))
    {
      return Err("Codex history changed while resolving its prefix".into());
    }
    let thread_spawn =
      matches!(owner.item(), RolloutItem::SessionMeta(meta) if crate::normalize::requires_thread_spawn_boundary(meta));
    let mut reference = crate::session_source::inspect_session_header_with_budget(&self.path, work)?;
    if owner.native()["payload"]["id"].as_str() != Some(reference.id.as_str()) {
      return Err("Codex history owner changed while reading its metadata".into());
    }
    source.apply_indexed_metadata(std::slice::from_mut(&mut reference));
    work.check()?;
    // Only backward discovery needs seekable decoded storage. Projections and
    // full snapshots stream each source directly, with no total-source budget.
    let mut files = if self.window_turns.is_some() && !thread_spawn {
      Some(
        segments
          .iter()
          .map(|segment| crate::storage::seekable(&segment.path, segment.end_byte_offset, &self.limits.scratch, work))
          .collect::<Result<Vec<_>, _>>()?,
      )
    } else {
      None
    };
    let lengths = match &files {
      Some(files) => segments
        .iter()
        .zip(files)
        .map(|(segment, file)| {
          segment.end_byte_offset.map_or_else(
            || {
              file
                .metadata()
                .map(|metadata| metadata.len())
                .map_err(|err| err.to_string())
            },
            Ok,
          )
        })
        .collect::<Result<Vec<_>, _>>()?,
      None => Vec::new(),
    };
    // Thread-spawn filtering requires an earlier trigger. Its full range is
    // streamed until a provider checkpoint can safely seed that boundary.
    let source_start = match (&mut files, self.window_turns) {
      (Some(files), Some(turns)) => super::window::start(
        files,
        &lengths,
        turns,
        self.limits.max_record_bytes,
        work,
        &mut self.stats,
      )?,
      _ => 0,
    };
    reference.model_baseline = if source_start == 0 {
      None
    } else if let Some(checkpoint) = self
      .model_checkpoint
      .as_ref()
      .filter(|checkpoint| checkpoint.source_start == source_start && checkpoint.versions == versions)
    {
      checkpoint.baseline.clone()
    } else {
      let baseline = super::window::model_baseline(
        files.as_mut().unwrap(),
        &lengths,
        source_start,
        self.limits.max_record_bytes,
        work,
        &mut self.stats,
      )?;
      self.model_checkpoint = Some(ModelCheckpoint {
        source_start,
        versions: versions.clone(),
        baseline: baseline.clone(),
      });
      baseline
    };
    let mut normalizer = CodexNormalizer::new_historical();
    let owner_events = normalizer.normalize(owner.clone());
    normalizer.seed_model(reference.model_baseline.as_ref());
    let mut records = Vec::new();
    if source_start == 0 {
      emit_record(
        projection,
        &mut records,
        true,
        NormalizedRecord {
          record_id: format!("session:{}", reference.id),
          native: self.include_native.then(|| owner.native().clone()),
          events: owner_events,
        },
      );
    }
    let mut dependencies = super::window::Dependencies::default();
    let mut consumed = 0u64;
    let mut selected_bytes = 0u64;
    let mut head_state = None;
    let mut prefix_guards = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
      work.check()?;
      let known_length = files
        .as_ref()
        .map(|_| lengths[index])
        .or(segment.end_byte_offset)
        .or_else(|| (!crate::storage::is_compressed(&segment.path)).then_some(versions[index].length));
      let start = source_start.saturating_sub(consumed).min(known_length.unwrap_or(0));
      let header_guard = read_header_guard_stream(crate::storage::reader_with_budget(&segment.path, work)?)?;
      let header = history_header_with_budget(&segment.path, work)?;
      if header_key(&header) != segment.header_key {
        return Err("Codex history changed while reading its prefix".into());
      }
      let mut tail_guard = Vec::new();
      let sequential = files.is_none();
      let mut input: Box<dyn BufRead + '_> = match &mut files {
        Some(files) => {
          let file = &mut *files[index];
          let length = known_length.unwrap();
          tail_guard = read_range(
            file,
            length.saturating_sub(TAIL_GUARD_BYTES as u64),
            length.min(TAIL_GUARD_BYTES as u64),
          )?;
          file.seek(SeekFrom::Start(start)).map_err(|err| err.to_string())?;
          Box::new(BufReader::new(Read::by_ref(file).take(length - start)))
        }
        None => {
          let input = crate::storage::reader_with_budget(&segment.path, work)?;
          match known_length {
            Some(length) => Box::new(input.take(length)),
            None => input,
          }
        }
      };
      let snapshot_remaining = projection
        .is_none()
        .then(|| (self.limits.max_snapshot_bytes as u64).saturating_sub(selected_bytes));
      if known_length.is_some_and(|length| snapshot_remaining.is_some_and(|remaining| length - start > remaining)) {
        return Err("Codex history exceeds the snapshot size limit".into());
      }
      let mut pending = Vec::new();
      let mut ordinals = HistoryOrdinals::new(segment.header_key.paginated);
      let inherited_parent = thread_spawn && header.native()["payload"]["id"].as_str() != Some(reference.id.as_str());
      let bytes = read_stream_rows(
        &mut input,
        start,
        &mut pending,
        self.limits.max_record_bytes,
        snapshot_remaining,
        work,
        &mut self.stats,
        // Seekable input already supplied its exact tail, including omitted
        // bytes if this segment is outside the selected window.
        sequential.then_some(&mut tail_guard),
        |row, row_offset, stats| {
          let line = parse_row(row, &segment.path, row_offset, stats)?;
          ordinals.accept(&line, &segment.path, row_offset)?;
          if matches!(line.item(), RolloutItem::SessionMeta(_)) {
            return Ok(());
          }
          if self.window_turns.is_some() {
            dependencies.observe(&line);
          }
          emit_record(
            projection,
            &mut records,
            false,
            normalize_record(
              line,
              &segment.path,
              row_offset,
              self.include_native,
              inherited_parent,
              &mut normalizer,
              &mut reference,
            ),
          );
          Ok(())
        },
      )?;
      drop(input);
      let length = start + bytes;
      if known_length.is_some_and(|expected| length != expected) {
        return Err("Codex history changed while reading its prefix".into());
      }
      selected_bytes += bytes;
      if segment.end_byte_offset.is_some() && !pending.is_empty() {
        return Err("invalid Codex history lineage: cutoff is not a complete record".into());
      }
      if start < length
        && segment
          .end_ordinal_exclusive
          .is_some_and(|end| !ordinals.ends_before(end))
      {
        return Err("invalid Codex history lineage: cutoff ordinal disagrees with its bytes".into());
      }
      if segment.end_byte_offset.is_none() {
        head_state = Some((length, pending, ordinals, header_guard, tail_guard));
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
      // A retry must release its decoded files before reserving a wider range.
      drop(files);
      return self.rebuild(source, projection, work);
    }
    validate_versions(&segments, &versions, &prefix_guards, &mut self.stats)?;
    work.check()?;
    let update = CodexHistoryUpdate {
      reference: reference.clone(),
      records,
      history_status: normalizer.history_status(),
      reset: true,
      source_start: self.window_turns.map(|_| source_start),
    };
    let (offset, pending, ordinals, header_guard, tail_guard) = head_state.unwrap();
    self.state = Some(ReaderState {
      roots,
      segments,
      versions,
      reference,
      normalizer,
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

fn read_header_guard_stream(input: impl Read) -> Result<HeaderGuard, String> {
  let mut reader = BufReader::new(input.take(MAX_HEADER_BYTES));
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
        return Ok(HeaderGuard {
          length: bytes.len() as u64,
          digest: Sha256::digest(&bytes).into(),
        });
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

/// Keep only one source record, including a previous incomplete fragment.
/// `take` bounds both the file snapshot and each read_until allocation before
/// parsing. A fragmented long row is scanned only in its newly appended bytes.
#[allow(clippy::too_many_arguments)]
fn read_rows(
  file: &mut File,
  offset: u64,
  length: u64,
  pending: &mut Vec<u8>,
  max_record_bytes: usize,
  work: &ReadWorkBudget,
  stats: &mut CodexHistoryReadStats,
  tail: Option<&mut Vec<u8>>,
  consume: impl FnMut(&[u8], u64, &mut CodexHistoryReadStats) -> Result<(), String>,
) -> Result<(), String> {
  file.seek(SeekFrom::Start(offset)).map_err(|err| err.to_string())?;
  let mut reader = BufReader::new(Read::by_ref(file).take(length));
  let consumed = read_stream_rows(
    &mut reader,
    offset,
    pending,
    max_record_bytes,
    None,
    work,
    stats,
    tail,
    consume,
  )?;
  if consumed != length {
    return Err("Codex history changed while reading its prefix".into());
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn read_stream_rows(
  reader: &mut dyn BufRead,
  offset: u64,
  pending: &mut Vec<u8>,
  max_record_bytes: usize,
  max_snapshot_bytes: Option<u64>,
  work: &ReadWorkBudget,
  stats: &mut CodexHistoryReadStats,
  mut tail: Option<&mut Vec<u8>>,
  mut consume: impl FnMut(&[u8], u64, &mut CodexHistoryReadStats) -> Result<(), String>,
) -> Result<u64, String> {
  let mut row_offset = offset - pending.len() as u64;
  let initial_row_offset = row_offset;
  let mut consumed = 0u64;
  loop {
    work.check()?;
    let previous = pending.len();
    let mut allowance = max_record_bytes.saturating_sub(previous).saturating_add(1) as u64;
    if let Some(limit) = max_snapshot_bytes {
      allowance = allowance.min(limit.saturating_sub(consumed).saturating_add(1));
    }
    let read = Read::take(&mut *reader, allowance)
      .read_until(b'\n', pending)
      .map_err(|err| err.to_string())?;
    consumed += read as u64;
    stats.source_bytes_read += read as u64;
    if max_snapshot_bytes.is_some_and(|limit| consumed > limit) {
      return Err("Codex history exceeds the snapshot size limit".into());
    }
    if pending.len() > max_record_bytes {
      return Err("Codex history record exceeds the record size limit".into());
    }
    if read == 0 {
      break;
    }
    if let Some(tail) = &mut tail {
      update_tail(tail, &pending[previous..]);
    }
    if pending.last() == Some(&b'\n') {
      if !pending.iter().all(u8::is_ascii_whitespace) {
        consume(pending, row_offset, stats)?;
      }
      row_offset += pending.len() as u64;
      pending.clear();
    }
  }
  // Do not cache the allocation for a completed large record. Keep only the
  // unfinished suffix, without repeatedly copying an ever-growing fragment.
  if row_offset != initial_row_offset {
    pending.shrink_to_fit();
  }
  Ok(consumed)
}

fn guards_match(
  file: &mut File,
  header_guard: &HeaderGuard,
  tail_guard: &[u8],
  offset: u64,
  stats: &mut CodexHistoryReadStats,
) -> Result<bool, String> {
  let header = read_range(file, 0, header_guard.length)?;
  stats.guard_bytes_read += header.len() as u64;
  if <[u8; 32]>::from(Sha256::digest(&header)) != header_guard.digest {
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
  if crate::storage::is_compressed(path) || after.identity != before.identity || after.length <= before.length {
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
    record_id: format!("segment:{}:{offset}", crate::storage::logical_path(path).display()),
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
    if crate::storage::resolve_path(&segment.path) != segment.path {
      return Err("Codex history changed while reading its prefix".into());
    }
    let after = FileVersion::read(&segment.path)?;
    let valid = if crate::storage::is_compressed(&segment.path) {
      after == *before
    } else if segment.end_byte_offset.is_some() {
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
