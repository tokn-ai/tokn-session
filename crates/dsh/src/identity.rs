//! Bounded ownership checks for exact native histories. Later records cannot
//! replace the identity in the first row and are never parsed by this reader.
use crate::{session_source, storage};
use std::{
  fs::File,
  io::{BufRead, BufReader, Read},
  path::Path,
  time::UNIX_EPOCH,
};
use tokn_session_core::SessionHeader;

const MAX_HEADER_BYTES: u64 = 64 * 1024;
const MAX_COMPRESSED_INPUT_BYTES: u64 = 1024 * 1024;

/// Read and verify the owning version-zero DSH session row at this exact path.
///
/// Plain JSONL and concatenated Zstandard frames are supported, including frame
/// boundaries within the header. The first row must be complete (newline
/// terminated). Input, decoded header, and decompression window sizes are
/// bounded. This does not hydrate title/preview or inspect conversation records.
/// `cwd` is returned as recorded, for the caller's canonical-directory check.
pub fn read_session_header_exact(path: &Path, expected_session_id: &str) -> Result<SessionHeader, String> {
  if expected_session_id.is_empty()
    || expected_session_id.len() > 512
    || expected_session_id.trim() != expected_session_id
    || expected_session_id.chars().any(char::is_control)
  {
    return Err("Invalid expected DSH session identity".into());
  }
  let file = File::open(path).map_err(|error| format!("Could not inspect DSH history {}: {error}", path.display()))?;
  let metadata = file.metadata().map_err(|error| error.to_string())?;
  if !metadata.is_file() {
    return Err("DSH session history must be a regular file".into());
  }
  let compressed = path
    .extension()
    .is_some_and(|extension| extension == "zstd" || extension == "zst");
  let mut input: Box<dyn BufRead> = if compressed {
    // DSH appends independent frames; decoding must continue across them to
    // finish a header split between writes, without reading an unbounded prefix.
    let mut decoder = zstd::stream::read::Decoder::new(file.take(metadata.len().min(MAX_COMPRESSED_INPUT_BYTES)))
      .map_err(|error| format!("Could not decode DSH history: {error}"))?;
    decoder.window_log_max(23).map_err(|error| error.to_string())?;
    Box::new(BufReader::new(decoder))
  } else {
    Box::new(BufReader::new(file.take(metadata.len())))
  };
  let mut line = String::new();
  input
    .by_ref()
    .take(MAX_HEADER_BYTES + 1)
    .read_line(&mut line)
    .map_err(|error| format!("Could not read DSH session header: {error}"))?;
  if line.len() as u64 > MAX_HEADER_BYTES {
    return Err("DSH session header exceeds its size limit".into());
  }
  if !line.ends_with('\n') {
    return Err("DSH session header is not complete".into());
  }
  let native = session_source::header(storage::parse(&line, path, 1)?, path)?;
  if native.id != expected_session_id {
    return Err("DSH history has a different session identity".into());
  }
  let reference = session_source::reference(path, &native);
  let updated_at_ms = metadata
    .modified()
    .ok()
    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
    .and_then(|duration| i64::try_from(duration.as_millis()).ok());
  Ok(SessionHeader {
    id: reference.id,
    parent_session_id: reference.parent_session_id,
    agent_path: reference.agent_path,
    agent_nickname: reference.agent_nickname,
    agent_role: reference.agent_role,
    title: None,
    preview: None,
    path: reference.path,
    cwd: reference.cwd,
    timestamp: reference.timestamp,
    updated_at: updated_at_ms.map(|time| time.to_string()),
    updated_at_ms,
  })
}
