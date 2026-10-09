//! Managed live feed: stdout is protocol-only, stderr carries diagnostics,
//! and stdin EOF ends the child even if its async runtime stalls.
use crate::{PROVIDERS, RelayConfig, SessionRelay, provider_roots};
use std::io::{Read, Write};
use std::time::Duration;

pub const CHILD_FLAG: &str = "--tokn-viewer-relay-child";
pub const VERSION: u32 = 1;
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Managed viewers already have native index watches and their own durable
/// recovery pass. Keep Relay's whole-history scan as a coarse safety net so
/// an idle embedded child does not traverse every provider twice a minute.
const MANAGED_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

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
  let native = match (args.next(), args.next()) {
    (None, None) => false,
    (Some(flag), None) if flag == "--native" => true,
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
    .and_then(|runtime| runtime.block_on(run(native)));
  if let Err(error) = result {
    eprintln!("Relay stopped: {error}");
    std::process::exit(1);
  }
  std::process::exit(0);
}

async fn run(native: bool) -> Result<(), String> {
  let mut stdout = std::io::stdout().lock();
  let mut relay = initialize(&mut stdout, managed_config(native)?).await?;
  loop {
    let update = relay.next_update().await?;
    for warning in update.warnings {
      eprintln!("{warning}");
    }
    for record in update.records {
      if !write_record(&mut stdout, &record)? {
        eprintln!(
          "Relay skipped oversized record {} in {} (pipe frame limit: {MAX_LINE_BYTES} bytes)",
          record.record.record_id,
          record.path.display()
        );
      }
    }
  }
}

fn managed_config(native: bool) -> Result<RelayConfig, String> {
  let mut config = default_config(native)?;
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

/// A single oversized live hint must not stop the entire managed feed. The
/// viewer retains authoritative snapshot readers and its own recovery scan.
fn write_record(writer: &mut impl Write, record: &impl serde::Serialize) -> Result<bool, String> {
  let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
  if bytes.len() >= MAX_LINE_BYTES {
    return Ok(false);
  }
  write_bytes_line(writer, &bytes)?;
  Ok(true)
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
    assert_eq!(managed_config(false).unwrap().poll_interval, MANAGED_POLL_INTERVAL);
  }

  #[test]
  fn oversized_record_does_not_prevent_later_records() {
    let mut output = Vec::new();
    let oversized = serde_json::json!({"payload": "x".repeat(MAX_LINE_BYTES)});
    let following = serde_json::json!({"record_id": "later"});

    assert!(!write_record(&mut output, &oversized).unwrap());
    assert!(write_record(&mut output, &following).unwrap());
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&output).unwrap(), following);
  }
}
