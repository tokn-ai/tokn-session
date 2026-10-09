//! Managed live feed: stdout is protocol-only, stderr carries diagnostics,
//! and stdin EOF ends the child even if its async runtime stalls.
use crate::{PROVIDERS, RelayConfig, RelayRecord, SessionRelay, provider_roots};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokn_session_core::Provider;

pub const CHILD_FLAG: &str = "--tokn-viewer-relay-child";
pub const VERSION: u32 = 2;
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BATCH_SESSIONS: usize = 256;
/// Managed viewers already have native index watches and their own durable
/// recovery pass. Keep Relay's whole-history scan as a coarse safety net so
/// an idle embedded child does not traverse every provider twice a minute.
const MANAGED_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Managed viewers reconstruct event history through their snapshot readers.
/// Only source identity crosses this pipe; native/event payloads stay local.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, Hash)]
pub struct SessionHint {
  pub provider: Provider,
  pub path: PathBuf,
  pub session_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SessionChanges {
  pub sessions: Vec<SessionHint>,
}

pub fn default_config(native: bool) -> Result<RelayConfig, String> {
  let mut roots = Vec::new();
  for provider in PROVIDERS {
    roots.extend(provider_roots(provider, None)?);
  }
  let mut config = RelayConfig::new(roots);
  config.include_native = native;
  Ok(config)
}

pub fn run_if_requested() {
  let mut args = std::env::args_os().skip(1);
  if args.next().as_deref() != Some(std::ffi::OsStr::new(CHILD_FLAG)) {
    return;
  }
  match (args.next(), args.next()) {
    (None, None) => {}
    (Some(flag), None) if flag == "--native" => {}
    _ => std::process::exit(2),
  };
  std::thread::spawn(|| {
    let mut byte = [0];
    while matches!(std::io::stdin().read(&mut byte), Ok(1)) {}
    std::process::exit(0);
  });
  let result = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .map_err(|e| e.to_string())
    .and_then(|runtime| runtime.block_on(run()));
  if let Err(error) = result {
    eprintln!("Relay stopped: {error}");
    std::process::exit(1);
  }
  std::process::exit(0);
}

async fn run() -> Result<(), String> {
  let mut stdout = std::io::stdout().lock();
  let mut relay = initialize(&mut stdout, managed_config()?).await?;
  loop {
    let update = relay.next_update().await?;
    for warning in update.warnings {
      eprintln!("{warning}");
    }
    write_changes(&mut stdout, &update.records)?;
  }
}

fn managed_config() -> Result<RelayConfig, String> {
  // Native inclusion belongs to the authoritative snapshot service. The
  // managed pipe carries source hints regardless of that viewer setting.
  let mut config = default_config(false)?;
  config.poll_interval = MANAGED_POLL_INTERVAL;
  Ok(config)
}

/// Readiness describes the managed pipe, not completion of Relay's seed scan.
/// Viewer-core owns catalogs and snapshots, so it can connect as soon as the
/// transport exists while this child seeds its live-feed cursors.
async fn initialize(writer: &mut impl Write, config: RelayConfig) -> Result<SessionRelay, String> {
  SessionRelay::new_with_ready(config, || {
    write_line(writer, &serde_json::json!({"type":"ready", "version":VERSION}))
  })
  .await
}

fn write_line(writer: &mut impl Write, value: &impl serde::Serialize) -> Result<(), String> {
  let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
  if bytes.len() >= MAX_LINE_BYTES {
    return Err("Relay record exceeds pipe frame limit".into());
  }
  write_bytes_line(writer, &bytes)
}

fn write_changes(writer: &mut impl Write, records: &[RelayRecord]) -> Result<(), String> {
  write_changes_with_limit(writer, records, MAX_LINE_BYTES)
}

fn write_changes_with_limit(
  writer: &mut impl Write,
  records: &[RelayRecord],
  frame_limit: usize,
) -> Result<(), String> {
  let mut seen = HashSet::new();
  let mut sessions = Vec::new();
  let empty_bytes = serde_json::to_vec(&SessionChanges { sessions: Vec::new() })
    .map_err(|e| e.to_string())?
    .len();
  let mut batch_bytes = empty_bytes;
  for record in records {
    if seen.insert((record.session.provider, &record.path, &record.session.session_id)) {
      let hint = SessionHint {
        provider: record.session.provider,
        path: record.path.clone(),
        session_id: record.session.session_id.clone(),
      };
      let hint_bytes = serde_json::to_vec(&hint).map_err(|e| e.to_string())?.len();
      if empty_bytes + hint_bytes >= frame_limit {
        eprintln!(
          "Relay skipped oversized session hint in {} (pipe frame limit: {frame_limit} bytes)",
          hint.path.display()
        );
        continue;
      }
      if batch_bytes + usize::from(!sessions.is_empty()) + hint_bytes >= frame_limit {
        write_line(
          writer,
          &SessionChanges {
            sessions: std::mem::take(&mut sessions),
          },
        )?;
        batch_bytes = empty_bytes;
      }
      batch_bytes += hint_bytes + usize::from(!sessions.is_empty());
      sessions.push(hint);
      if sessions.len() == MAX_BATCH_SESSIONS {
        write_line(
          writer,
          &SessionChanges {
            sessions: std::mem::take(&mut sessions),
          },
        )?;
        batch_bytes = empty_bytes;
      }
    }
  }
  if !sessions.is_empty() {
    write_line(writer, &SessionChanges { sessions })?;
  }
  Ok(())
}

fn write_bytes_line(writer: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
  writer
    .write_all(bytes)
    .and_then(|_| writer.write_all(b"\n"))
    .and_then(|_| writer.flush())
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn readiness_precedes_provider_initialization() {
    struct StopAfterReady(Vec<u8>);
    impl Write for StopAfterReady {
      fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
      }
      fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("stop after readiness"))
      }
    }
    let mut output = StopAfterReady(Vec::new());
    assert!(initialize(&mut output, RelayConfig::new(Vec::new())).await.is_err());
    assert_eq!(
      serde_json::from_slice::<serde_json::Value>(&output.0).unwrap(),
      serde_json::json!({"type":"ready", "version":VERSION})
    );
  }

  #[test]
  fn managed_feed_uses_coarse_full_scan_recovery() {
    let config = managed_config().unwrap();
    assert_eq!(config.poll_interval, MANAGED_POLL_INTERVAL);
    assert!(!config.include_native);
  }

  #[test]
  fn managed_changes_coalesce_records_and_omit_even_oversized_payloads() {
    let mut output = Vec::new();
    let oversized: RelayRecord = serde_json::from_value(serde_json::json!({
      "path": "/tmp/active.jsonl", "topic": "codex.active", "operation": "upsert", "record_id": "jsonl:0",
      "session": {"provider": "codex", "session_id": "active"},
      "events": [], "native": {"text": "x".repeat(MAX_LINE_BYTES)}
    }))
    .unwrap();
    let mut another = oversized.clone();
    another.record.record_id = "jsonl:100".into();
    another.record.native = None;
    let mut other_session = another.clone();
    other_session.session.session_id = "other".into();
    write_changes(&mut output, &[oversized, another, other_session]).unwrap();
    let changes: SessionChanges = serde_json::from_slice(&output).unwrap();
    assert_eq!(changes.sessions.len(), 2);
    assert_eq!(changes.sessions[0].session_id, "active");
    assert_eq!(changes.sessions[1].session_id, "other");
    assert!(
      output.len() < 256,
      "wire bytes must depend on identities, not transcript size"
    );
  }

  fn fixture_record(id: &str, text: &str) -> RelayRecord {
    serde_json::from_value(serde_json::json!({
      "path": "/tmp/active.jsonl", "topic": "pi.active", "operation": "upsert", "record_id": "jsonl:0",
      "session": {"provider": "pi", "session_id": id},
      "events": [{"type": "message", "provider": "pi", "session_id": id, "role": "assistant", "delivery": "final", "phase": "finished", "text": text}]
    })).unwrap()
  }

  #[test]
  fn byte_budget_splits_batches_and_skips_only_an_oversized_identity() {
    let mut records: Vec<_> = (0..8)
      .map(|id| fixture_record(&format!("{id}{}", "x".repeat(80)), "hello"))
      .collect();
    records.insert(2, fixture_record(&"x".repeat(512), "oversized identity"));
    let mut output = Vec::new();
    write_changes_with_limit(&mut output, &records, 512).unwrap();
    let mut delivered = Vec::new();
    for frame in output.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
      assert!(frame.len() < 512);
      let changes: SessionChanges = serde_json::from_slice(frame).unwrap();
      delivered.extend(changes.sessions);
    }
    assert_eq!(delivered.len(), 8);
    assert!(delivered.iter().all(|hint| hint.session_id.len() == 81));
  }

  #[test]
  fn managed_message_burst_reduces_wire_bytes() {
    let records: Vec<_> = (0..128)
      .map(|id| {
        let mut record = fixture_record("active", &"x".repeat(128));
        record.record.record_id = format!("jsonl:{id}");
        record
      })
      .collect();
    let full_bytes: usize = records
      .iter()
      .map(|record| serde_json::to_vec(record).unwrap().len() + 1)
      .sum();
    let mut output = Vec::new();
    write_changes(&mut output, &records).unwrap();
    let changes: SessionChanges = serde_json::from_slice(&output).unwrap();
    assert_eq!(changes.sessions.len(), 1);
    assert!(output.len() * 100 < full_bytes);
    eprintln!(
      "128-record managed burst: {full_bytes} full-record bytes -> {} hint bytes",
      output.len()
    );
  }
}
