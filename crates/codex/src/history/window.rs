//! Backward boundary discovery skips payload materialization and normalization.
//! Explicit turn starts are safe normalization boundaries. Older formats with
//! no provable boundary retain the full-reader correctness fallback.
use super::*;
use serde::Deserialize;

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
  segments: &[CodexHistorySegment],
  lengths: &[u64],
  turns: usize,
  stats: &mut CodexHistoryReadStats,
) -> Result<u64, String> {
  let mut logical = lengths.iter().sum::<u64>();
  let mut remaining = turns;
  for (segment, length) in segments.iter().zip(lengths).rev() {
    logical -= length;
    let mut lines = ReverseLines::new(&segment.path, *length)?;
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

struct ReverseLines {
  file: File,
  position: u64,
  buffer: Vec<u8>,
  discard_partial: bool,
  first: bool,
}

impl ReverseLines {
  fn new(path: &Path, length: u64) -> Result<Self, String> {
    Ok(Self {
      file: File::open(path).map_err(|e| e.to_string())?,
      position: length,
      buffer: Vec::new(),
      discard_partial: false,
      first: true,
    })
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
      if self.position == 0 {
        if self.buffer.is_empty() {
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
      let length = self.position.min(self.buffer.len().max(64 * 1024) as u64);
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
