//! Bounded verification of a persisted Pi session's native identity.
use std::{
  fs::File,
  io::{BufRead, BufReader, Read},
  path::{Path, PathBuf},
};
use tokn_pi_protocol::PiSessionLine;

pub const MAX_SESSION_HEADER_BYTES: usize = 64 * 1024;

/// Native identity observed in a complete, matching persisted session header.
///
/// Both paths are canonical. This describes the file at verification time;
/// callers must verify again before relying on a later read or delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedSessionIdentity {
  pub session_file: PathBuf,
  pub session_id: String,
  pub cwd: PathBuf,
}

/// Verify only the first JSONL row, without decoding conversation history.
///
/// `expected_cwd` must already be canonical. Missing files and incomplete first
/// rows return `None`, allowing callers to wait for Pi's initial persistence.
/// A planned path or an unterminated header never establishes identity.
pub fn verify_session_identity(
  path: &Path,
  expected_session_id: &str,
  expected_cwd: &Path,
) -> Result<Option<VerifiedSessionIdentity>, String> {
  let file = match File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(format!("Could not inspect Pi history: {error}")),
  };
  if !file.metadata().map_err(|error| error.to_string())?.is_file() {
    return Err("Pi session history must be a regular file".into());
  }
  let mut line = String::new();
  BufReader::new(file.take(MAX_SESSION_HEADER_BYTES as u64 + 1))
    .read_line(&mut line)
    .map_err(|error| error.to_string())?;
  if line.len() > MAX_SESSION_HEADER_BYTES {
    return Err("Pi session header exceeds its size limit".into());
  }
  if !line.ends_with('\n') {
    return Ok(None);
  }
  let line: PiSessionLine = serde_json::from_str(&line).map_err(|_| "Pi persisted an invalid session header")?;
  // Ownership depends only on native identity. The tolerant historical view
  // can become Unknown when unrelated optional fields change shape.
  let header = line.native();
  if header.get("type").and_then(serde_json::Value::as_str) != Some("session")
    || header.get("id").and_then(serde_json::Value::as_str) != Some(expected_session_id)
  {
    return Err("Pi persisted a different session identity".into());
  }
  let cwd = header
    .get("cwd")
    .and_then(serde_json::Value::as_str)
    .ok_or("Pi session header has no working directory")?;
  let cwd = std::fs::canonicalize(cwd).map_err(|error| error.to_string())?;
  if cwd != expected_cwd || std::fs::canonicalize(expected_cwd).map_err(|error| error.to_string())? != expected_cwd {
    return Err("Pi session working directory differs from its expected canonical directory".into());
  }
  Ok(Some(VerifiedSessionIdentity {
    session_file: std::fs::canonicalize(path).map_err(|error| error.to_string())?,
    session_id: expected_session_id.to_owned(),
    cwd,
  }))
}
