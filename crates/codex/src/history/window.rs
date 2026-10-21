//! Backward boundary discovery skips payload materialization and normalization.
//! Explicit turn starts are safe normalization boundaries. Older formats with
//! no provable boundary retain the full-reader correctness fallback.
use super::*;
use serde::Deserialize;
use tokn_session_core::{ModelIdentity, ModelObservation, ModelOrigin};

const MODEL_CHECKPOINT_BYTES: u64 = 1024 * 1024;

/// Only fields that can establish main-thread model state. Ignored payloads
/// are skipped by serde, without materializing transcript bodies or tool data.
#[derive(Default, Deserialize)]
struct ModelProbe {
  #[serde(default, rename = "type")]
  kind: String,
  #[serde(default)]
  payload: ModelPayload,
}

#[derive(Default, Deserialize)]
struct ModelPayload {
  #[serde(default, rename = "type")]
  kind: String,
  model: Option<String>,
  model_provider: Option<String>,
  thread_settings: Option<ModelSettings>,
}

#[derive(Default, Deserialize)]
struct ModelSettings {
  model: Option<String>,
  model_provider_id: Option<String>,
}

/// Find state immediately before the retained range. This is separate from
/// event normalization and never manufactures source rows or changes offsets.
pub(super) fn model_baseline(
  files: &mut [File],
  lengths: &[u64],
  source_start: u64,
  stats: &mut CodexHistoryReadStats,
) -> Result<Option<ModelObservation>, String> {
  let mut logical = lengths.iter().sum::<u64>();
  let mut model = None;
  let mut provider = None;
  let mut probes = CodexHistoryReadStats::default();
  for (file, length) in files.iter_mut().zip(lengths).rev() {
    logical -= length;
    let end = source_start.saturating_sub(logical).min(*length);
    if end == 0 {
      continue;
    }
    // The boundary is already known, so unavailable older model context must
    // never make opening a retained window read an unbounded tool/image row.
    let remaining = MODEL_CHECKPOINT_BYTES.saturating_sub(probes.boundary_bytes_read);
    if remaining == 0 {
      break;
    }
    let mut lines = ReverseLines::within(file, end, end.saturating_sub(remaining));
    while let Some((_, row)) = lines.next(&mut probes)? {
      stats.model_checkpoint_rows_scanned += 1;
      let Ok(probe) = serde_json::from_slice::<ModelProbe>(&row) else {
        continue;
      };
      let (model_id, model_provider, origin) = match (probe.kind.as_str(), probe.payload.kind.as_str()) {
        ("turn_context", _) => (probe.payload.model, None, ModelOrigin::Request),
        ("event_msg", "thread_settings_applied") => {
          let settings = probe.payload.thread_settings.unwrap_or_default();
          (settings.model, settings.model_provider_id, ModelOrigin::Selection)
        }
        ("session_meta", _) => (None, probe.payload.model_provider, ModelOrigin::Catalog),
        _ => continue,
      };
      if provider.is_none() {
        provider = model_provider.filter(|provider| !provider.trim().is_empty());
      }
      if model.is_none() {
        model = model_id
          .filter(|model| !model.trim().is_empty())
          .map(|model_id| (model_id, origin));
      }
      // A per-turn observation need not repeat its provider. Do not scan the
      // entire older transcript merely to qualify it: an earlier selection
      // may belong to a different model, and an initial header can be stale.
      if model.is_some() {
        break;
      }
    }
    if model.is_some() {
      break;
    }
  }
  stats.model_checkpoint_bytes_read += probes.boundary_bytes_read;
  Ok(model.map(|(model_id, origin)| ModelObservation {
    model: ModelIdentity {
      model_provider: provider,
      model_id,
    },
    origin,
  }))
}

#[derive(Default, Deserialize)]
struct Probe {
  #[serde(default, rename = "type")]
  kind: String,
  #[serde(default)]
  payload: Payload,
}

#[derive(Default, Deserialize)]
struct Payload {
  #[serde(default, rename = "type")]
  kind: String,
  turn_id: Option<String>,
  #[serde(default, rename = "turnId")]
  legacy_turn_id: Option<String>,
}

pub(super) fn start(
  files: &mut [File],
  lengths: &[u64],
  turns: usize,
  stats: &mut CodexHistoryReadStats,
) -> Result<u64, String> {
  let mut logical = lengths.iter().sum::<u64>();
  let mut remaining = turns;
  for (file, length) in files.iter_mut().zip(lengths).rev() {
    logical -= length;
    let mut lines = ReverseLines::new(file, *length);
    while let Some((offset, row)) = lines.next(stats)? {
      stats.boundary_rows_scanned += 1;
      let Ok(probe) = serde_json::from_slice::<Probe>(&row) else {
        continue;
      };
      if probe.kind == "event_msg"
        && matches!(probe.payload.kind.as_str(), "task_started" | "turn_started")
        && probe
          .payload
          .turn_id
          .or(probe.payload.legacy_turn_id)
          .is_some_and(|id| !id.trim().is_empty())
      {
        remaining = remaining.saturating_sub(1);
        if remaining == 0 {
          return Ok(logical + offset);
        }
      }
    }
  }
  Ok(0)
}

pub(super) fn is_turn_start(line: &CodexLine) -> bool {
  let native = line.native();
  let payload = &native["payload"];
  native["type"] == "event_msg"
    && matches!(payload["type"].as_str(), Some("task_started" | "turn_started"))
    && payload["turn_id"]
      .as_str()
      .or_else(|| payload["turnId"].as_str())
      .is_some_and(|id| !id.trim().is_empty())
}

struct ReverseLines<'a> {
  file: &'a mut File,
  position: u64,
  lower_bound: u64,
  buffer: Vec<u8>,
  discard_partial: bool,
  first: bool,
}

impl<'a> ReverseLines<'a> {
  fn new(file: &'a mut File, length: u64) -> Self {
    Self::within(file, length, 0)
  }

  fn within(file: &'a mut File, length: u64, lower_bound: u64) -> Self {
    Self {
      file,
      position: length,
      lower_bound,
      buffer: Vec::new(),
      discard_partial: false,
      first: true,
    }
  }

  fn next(&mut self, stats: &mut CodexHistoryReadStats) -> Result<Option<(u64, Vec<u8>)>, String> {
    loop {
      let end = self
        .buffer
        .len()
        .saturating_sub(usize::from(self.buffer.last() == Some(&b'\n')));
      if let Some(previous) = self.buffer[..end].iter().rposition(|byte| *byte == b'\n') {
        let start = previous + 1;
        let row = self.buffer[start..end].to_vec();
        let offset = self.position + start as u64;
        self.buffer.truncate(start);
        if std::mem::take(&mut self.discard_partial) {
          continue;
        }
        return Ok(Some((offset, row)));
      }
      if self.position == self.lower_bound {
        // A budget boundary can bisect a row. That row is incomplete and
        // cannot establish a checkpoint; do not parse its trailing fragment.
        if self.buffer.is_empty() || self.lower_bound != 0 {
          return Ok(None);
        }
        let mut row = std::mem::take(&mut self.buffer);
        if row.last() == Some(&b'\n') {
          row.pop();
        }
        if std::mem::take(&mut self.discard_partial) {
          return Ok(None);
        }
        return Ok(Some((0, row)));
      }
      // Grow reads for long rows, avoiding quadratic prefix copying.
      let length = (self.position - self.lower_bound).min(self.buffer.len().max(64 * 1024) as u64);
      self.position -= length;
      self
        .file
        .seek(SeekFrom::Start(self.position))
        .map_err(|e| e.to_string())?;
      let mut bytes = vec![0; length as usize];
      self.file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
      stats.boundary_bytes_read += length;
      bytes.extend_from_slice(&self.buffer);
      if self.first {
        self.discard_partial = bytes.last() != Some(&b'\n');
        self.first = false;
      }
      self.buffer = bytes;
      if self.buffer.len() > 128 * 1024 * 1024 {
        return Err("Codex history row exceeds the snapshot size limit".into());
      }
    }
  }
}

/// Completed lifecycle items are self-contained snapshots. Only split wire
/// invocation/output records require an earlier invocation for decoding.
#[derive(Default)]
pub(super) struct Dependencies {
  calls: HashSet<String>,
  pub missing: bool,
}

impl Dependencies {
  pub fn observe(&mut self, line: &CodexLine) {
    let native = line.native();
    if native["type"] != "response_item" {
      return;
    }
    let payload = &native["payload"];
    let Some(id) = payload["call_id"].as_str() else {
      return;
    };
    match payload["type"].as_str() {
      Some("function_call" | "custom_tool_call") => {
        self.calls.insert(id.to_owned());
      }
      Some("function_call_output" | "custom_tool_call_output") => {
        self.missing |= !self.calls.contains(id);
      }
      _ => {}
    }
  }
}
