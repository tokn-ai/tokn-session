//! Read-only assembly of bounded, inherited Codex rollout prefixes.
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use tokn_codex_protocol::{HistoryPosition, RolloutItem};
use tokn_session_core::{AgentEvent, LoadedSessionRecords, NormalizedRecord};

use crate::CodexSessionSource;
use crate::event::CodexLine;
use crate::normalize::CodexNormalizer;
use crate::session_source::inspect_session_header;

mod reader;
mod window;
pub use reader::{CodexHistoryReadStats, CodexHistoryReader, CodexHistoryUpdate};

const MAX_SEGMENTS: usize = 64;
const MAX_HEADER_BYTES: u64 = 8 * 1024 * 1024;

/// One physical range, in oldest-to-newest logical history order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexHistorySegment {
  pub path: PathBuf,
  pub end_byte_offset: Option<u64>,
  pub end_ordinal_exclusive: Option<u64>,
  header_key: serde_json::Value,
}

/// Reads the owning metadata without scanning the rollout body.
pub fn history_header(path: &Path) -> Result<CodexLine, String> {
  let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  for line in BufReader::new(file.take(MAX_HEADER_BYTES)).lines() {
    let line = line.map_err(|err| err.to_string())?;
    if line.trim().is_empty() {
      continue;
    }
    let parsed: CodexLine = serde_json::from_str(&line)
      .map_err(|err| format!("invalid Codex history metadata at {}: {err}", path.display()))?;
    if parsed.native()["type"] == "session_meta" {
      return match parsed.item() {
        RolloutItem::SessionMeta(_) => Ok(parsed),
        _ => Err(format!("invalid Codex history metadata at {}", path.display())),
      };
    }
  }
  Err(format!("missing Codex history metadata at {}", path.display()))
}

impl CodexSessionSource {
  /// Resolves a logical history using physical metadata and exact exclusive
  /// cutoffs. The native database is not needed, including after export.
  pub fn history_segments(&self, path: &Path) -> Result<Vec<CodexHistorySegment>, String> {
    let mut current = path.to_path_buf();
    let mut end: Option<HistoryPosition> = None;
    let mut segments = Vec::new();
    let mut seen = HashSet::new();
    let mut candidates = None;
    loop {
      let canonical = current.canonicalize().map_err(|err| err.to_string())?;
      if !seen.insert(canonical) || segments.len() == MAX_SEGMENTS {
        return Err("invalid Codex history lineage: cycle or excessive depth".into());
      }
      let header = history_header(&current)?;
      let RolloutItem::SessionMeta(meta) = header.item() else {
        unreachable!()
      };
      if (end.is_some() || meta.history_base.is_some()) && meta.history_mode.as_deref() != Some("paginated") {
        return Err("invalid Codex history lineage: inherited rollouts must be paginated".into());
      }
      if end.is_some() && meta.history_base.is_none() && header.ordinal() != Some(0) {
        return Err("invalid Codex history lineage: initial segment does not begin at ordinal zero".into());
      }
      segments.push(CodexHistorySegment {
        path: current.clone(),
        end_byte_offset: end.as_ref().map(|end| end.end_byte_offset),
        end_ordinal_exclusive: end.as_ref().map(|end| end.end_ordinal_exclusive),
        header_key: header_key(&header),
      });
      let Some(base) = meta.history_base.clone() else { break };
      if header.ordinal() != Some(base.end_ordinal_exclusive) {
        return Err("invalid Codex history lineage: metadata ordinal disagrees with its base".into());
      }
      if candidates.is_none() {
        let mut paths = Vec::new();
        for root in self.history_roots(path)? {
          collect_paths(&root, &mut paths)?;
        }
        candidates = Some(paths);
      }
      current = resolve_prefix(candidates.as_ref().unwrap(), &current, &base)?;
      end = Some(base);
    }
    segments.reverse();
    Ok(segments)
  }

  /// Loads complete source records atomically; missing or invalid prefixes
  /// fail instead of publishing a suffix as though it were complete history.
  pub fn load_session_records_path(
    &self,
    path: &Path,
    include_native: bool,
    max_bytes: usize,
  ) -> Result<LoadedSessionRecords, String> {
    let update = CodexHistoryReader::new(path.to_path_buf(), include_native, max_bytes)
      .poll(self)?
      .ok_or("Codex history produced no initial snapshot")?;
    Ok(LoadedSessionRecords {
      reference: update.reference,
      records: update.records,
      history_status: update.history_status,
    })
  }
}

fn header_key(line: &CodexLine) -> serde_json::Value {
  let payload = &line.native()["payload"];
  serde_json::json!({
    "ordinal": line.ordinal(),
    "id": payload["id"],
    "history_mode": payload["history_mode"],
    "history_base": payload["history_base"],
  })
}

struct HistoryOrdinals {
  paginated: bool,
  last: Option<u64>,
}

impl HistoryOrdinals {
  fn new(paginated: bool) -> Self {
    Self { paginated, last: None }
  }

  fn accept(&mut self, line: &CodexLine, path: &Path, offset: u64) -> Result<(), String> {
    if !self.paginated {
      return Ok(());
    }
    let ordinal = line.ordinal().ok_or_else(|| {
      format!(
        "invalid Codex history lineage at {} byte {offset}: missing ordinal",
        path.display()
      )
    })?;
    // Persisted Codex rows can repeat an ordinal or resume across a gap.
    // Physical record IDs preserve distinct rows in file order.
    // Retaining the last ordinal also keeps validation active after u64::MAX.
    if let Some(last) = self.last
      && ordinal < last
    {
      return Err(format!(
        "invalid Codex history lineage at {} byte {offset}: out-of-order ordinal {ordinal} after previous {last}",
        path.display()
      ));
    }
    self.last = Some(ordinal);
    Ok(())
  }

  fn ends_before(&self, end: u64) -> bool {
    self.last.is_some_and(|last| last < end)
  }
}

fn collect_paths(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
  if !root.exists() {
    return Ok(());
  }
  for entry in std::fs::read_dir(root).map_err(|err| err.to_string())? {
    let entry = entry.map_err(|err| err.to_string())?;
    let kind = entry.file_type().map_err(|err| err.to_string())?;
    if kind.is_dir() {
      collect_paths(&entry.path(), paths)?;
    } else if kind.is_file() && entry.path().extension().is_some_and(|extension| extension == "jsonl") {
      paths.push(entry.path());
    }
  }
  Ok(())
}

fn resolve_prefix(paths: &[PathBuf], current: &Path, base: &HistoryPosition) -> Result<PathBuf, String> {
  if base.end_ordinal_exclusive == 0 || base.end_byte_offset == 0 {
    return Err("invalid Codex history lineage: empty prefix cutoff".into());
  }
  let current = current.canonicalize().map_err(|err| err.to_string())?;
  let mut matches = Vec::new();
  for path in paths {
    if path.canonicalize().is_ok_and(|path| path == current) {
      continue;
    }
    // Native filenames carry their owning thread ID. Exported arbitrary names
    // are still supported, while unrelated native headers are never reopened.
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    if name.starts_with("rollout-") && !name.contains(&base.thread_id) {
      continue;
    }
    let Ok(header) = history_header(path) else { continue };
    let RolloutItem::SessionMeta(meta) = header.item() else {
      continue;
    };
    // Desktop can reference a continuation's physical UUID, while its
    // session_meta.id remains the logical owner across every segment.
    let identity_matches = meta.id.as_deref() == Some(base.thread_id.as_str())
      || crate::rollout_path::rollout_segment_matches(path, meta.id.as_deref(), &base.thread_id);
    if !identity_matches
      || meta.history_mode.as_deref() != Some("paginated")
      || !header
        .ordinal()
        .is_some_and(|ordinal| ordinal < base.end_ordinal_exclusive)
    {
      continue;
    }
    if cutoff_matches(path, base)? {
      matches.push(path.clone());
    }
  }
  match matches.as_slice() {
    [path] => Ok(path.clone()),
    [] => Err(format!(
      "Codex history prefix is unavailable for {} at ordinal {}",
      base.thread_id, base.end_ordinal_exclusive
    )),
    _ => Err(format!(
      "Codex history prefix is ambiguous for {} at ordinal {}",
      base.thread_id, base.end_ordinal_exclusive
    )),
  }
}

fn cutoff_matches(path: &Path, base: &HistoryPosition) -> Result<bool, String> {
  let mut file = File::open(path).map_err(|err| err.to_string())?;
  if file.metadata().map_err(|err| err.to_string())?.len() < base.end_byte_offset {
    return Ok(false);
  }
  // Only inspect the final bounded record; full ordinal ordering is checked
  // by the atomic loader. This keeps dependency discovery independent of size.
  let mut window = 64 * 1024;
  loop {
    let start = base.end_byte_offset.saturating_sub(window);
    file.seek(SeekFrom::Start(start)).map_err(|err| err.to_string())?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
      .take(base.end_byte_offset - start)
      .read_to_end(&mut bytes)
      .map_err(|err| err.to_string())?;
    if bytes.last() != Some(&b'\n') {
      return Ok(false);
    }
    let end = bytes.len() - 1;
    let record_start = bytes[..end]
      .iter()
      .rposition(|byte| *byte == b'\n')
      .map_or(0, |index| index + 1);
    if record_start == 0 && start != 0 {
      if window == MAX_HEADER_BYTES {
        return Ok(false);
      }
      window = (window * 2).min(MAX_HEADER_BYTES);
      continue;
    }
    let Ok(line) = serde_json::from_slice::<CodexLine>(&bytes[record_start..end]) else {
      return Ok(false);
    };
    let Some(ordinal) = line.ordinal() else {
      return Ok(false);
    };
    if ordinal.checked_add(1) == Some(base.end_ordinal_exclusive) {
      return Ok(true);
    }
    if ordinal >= base.end_ordinal_exclusive {
      return Ok(false);
    }
    // A BeforeTurn cutoff uses the excluded turn's ordinal across a gap. Verify
    // that row at the exact boundary so another same-owner segment with a lower
    // ordinal cannot masquerade as this prefix.
    file
      .seek(SeekFrom::Start(base.end_byte_offset))
      .map_err(|err| err.to_string())?;
    let mut next = Vec::new();
    BufReader::new(Read::by_ref(&mut file).take(MAX_HEADER_BYTES))
      .read_until(b'\n', &mut next)
      .map_err(|err| err.to_string())?;
    if next.last() != Some(&b'\n') {
      return Ok(false);
    }
    return Ok(
      serde_json::from_slice::<CodexLine>(&next).is_ok_and(|line| line.ordinal() == Some(base.end_ordinal_exclusive)),
    );
  }
}

#[cfg(test)]
mod window_tests;
