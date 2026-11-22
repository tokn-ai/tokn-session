//! Read-only native Segment storage. A snapshot never imports or repairs logs.
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
  collections::HashSet,
  io::Read,
  path::{Path, PathBuf},
  time::Duration,
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_SAFE: u64 = 9_007_199_254_740_991;
const MAX_TRACES: usize = 100_000;
const MAX_SEGMENTS: usize = 4096;

pub(crate) struct Snapshot {
  pub header: Value,
  pub rows: Vec<Value>,
  pub inherited_count: u64,
}

pub(crate) fn is_database(path: &Path) -> bool {
  path.extension().is_some_and(|extension| extension == "sqlite")
}

pub(crate) fn is_marker(path: &Path) -> bool {
  path
    .parent()
    .and_then(Path::file_name)
    .is_some_and(|name| name == ".segments")
    && path.extension().is_some_and(|extension| extension == "json")
}

/// Marker database paths are evidence to verify, never arbitrary files to open.
fn resolve(path: &Path, expected: Option<&str>) -> Result<(PathBuf, Option<String>), String> {
  if !is_marker(path) {
    return Ok((path.to_owned(), expected.map(str::to_owned)));
  }
  let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
  if !metadata.is_file() || metadata.len() > MAX_HEADER_BYTES as u64 {
    return Err("Invalid DSH SQLite routing marker size".into());
  }
  let mut bytes = Vec::new();
  std::fs::File::open(path)
    .map_err(|error| error.to_string())?
    .take(MAX_HEADER_BYTES as u64 + 1)
    .read_to_end(&mut bytes)
    .map_err(|error| error.to_string())?;
  if bytes.len() > MAX_HEADER_BYTES {
    return Err("DSH SQLite routing marker exceeds its size limit".into());
  }
  let value: Value =
    serde_json::from_slice(&bytes).map_err(|error| format!("Invalid DSH SQLite routing marker: {error}"))?;
  let id = value["id"]
    .as_str()
    .filter(|id| valid_id(id))
    .ok_or("Invalid DSH routing identity")?;
  let filename = format!("{:x}.json", Sha256::digest(id.as_bytes()));
  if path.file_name().and_then(|name| name.to_str()) != Some(filename.as_str())
    || expected.is_some_and(|expected| expected != id)
    || value.as_object().is_none_or(|object| object.len() != 2)
  {
    return Err("DSH routing marker has a different session identity".into());
  }
  let database = path.parent().unwrap().join("sessions.sqlite");
  if std::fs::symlink_metadata(&database)
    .map_err(|error| error.to_string())?
    .file_type()
    .is_symlink()
  {
    return Err("DSH routing database must not be a symbolic link".into());
  }
  let recorded = Path::new(value["database"].as_str().ok_or("Invalid DSH routing database")?);
  if !recorded.is_absolute()
    || std::fs::canonicalize(recorded).map_err(|error| error.to_string())?
      != std::fs::canonicalize(&database).map_err(|error| error.to_string())?
  {
    return Err("DSH routing marker names an unexpected database".into());
  }
  Ok((database, Some(id.to_owned())))
}

pub fn storage_dependency_paths(path: &Path) -> Result<Vec<PathBuf>, String> {
  if !is_database(path) && !is_marker(path) {
    return Ok(vec![path.to_owned()]);
  }
  let (database, _) = resolve(path, None)?;
  let mut paths = vec![database.clone()];
  let mut wal = database.as_os_str().to_owned();
  wal.push("-wal");
  paths.push(PathBuf::from(wal));
  if path != database {
    paths.push(path.to_owned());
  }
  Ok(paths)
}

fn open(path: &Path) -> Result<Connection, String> {
  if !std::fs::metadata(path).map_err(|error| error.to_string())?.is_file() {
    return Err("DSH SQLite storage must be a regular file".into());
  }
  let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
    .map_err(|error| format!("Could not read DSH SQLite storage: {error}"))?;
  db.busy_timeout(Duration::from_secs(5))
    .map_err(|error| error.to_string())?;
  db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")
    .map_err(|error| error.to_string())?;
  let application: i64 = db
    .pragma_query_value(None, "application_id", |row| row.get(0))
    .map_err(|error| error.to_string())?;
  let version: i64 = db
    .pragma_query_value(None, "user_version", |row| row.get(0))
    .map_err(|error| error.to_string())?;
  if application != 0x44535043 || version != 1 {
    return Err("Unsupported DSH SQLite storage schema".into());
  }
  for table in ["traces", "segments", "frames"] {
    let kind: Option<String> = db
      .query_row("SELECT type FROM sqlite_schema WHERE name=?1", [table], |row| {
        row.get(0)
      })
      .optional()
      .map_err(|error| error.to_string())?;
    if kind.as_deref() != Some("table") {
      return Err("Invalid DSH SQLite storage tables".into());
    }
  }
  Ok(db)
}

pub(crate) fn headers(path: &Path) -> Result<Vec<Value>, String> {
  let db = open(path)?;
  let mut statement = db
    .prepare(
      "SELECT CASE WHEN length(CAST(trace_uuid AS BLOB))<=512 THEN trace_uuid END, CASE WHEN length(CAST(header AS BLOB))<=65536 THEN header END FROM traces ORDER BY trace_uuid LIMIT 100001",
    )
    .map_err(|error| error.to_string())?;
  let mut rows = statement.query([]).map_err(|error| error.to_string())?;
  let mut headers = Vec::new();
  let mut total_bytes = 0usize;
  while let Some(row) = rows.next().map_err(|error| error.to_string())? {
    if headers.len() >= MAX_TRACES {
      return Err("DSH trace catalog exceeds its size limit".into());
    }
    let id: String = row.get(0).map_err(|error| error.to_string())?;
    let text: String = row.get(1).map_err(|_| "DSH SQLite header exceeds its size limit")?;
    total_bytes = total_bytes.saturating_add(text.len()).saturating_add(id.len());
    if total_bytes > 128 * 1024 * 1024 {
      return Err("DSH header catalog exceeds its size limit".into());
    }
    headers.push(decode_header(&id, &text)?);
  }
  Ok(headers)
}

pub(crate) fn header(path: &Path, expected: Option<&str>) -> Result<Value, String> {
  let (database, id) = resolve(path, expected)?;
  let db = open(&database)?;
  let id = select_id(&db, id)?;
  Ok(trace(&db, &id)?.0)
}

fn select_id(db: &Connection, id: Option<String>) -> Result<String, String> {
  if let Some(id) = id {
    return valid_id(&id).then_some(id).ok_or("Invalid DSH session identity".into());
  }
  let mut statement = db
    .prepare("SELECT CASE WHEN length(CAST(trace_uuid AS BLOB))<=512 THEN trace_uuid END FROM traces LIMIT 2")
    .map_err(|error| error.to_string())?;
  let ids = statement
    .query_map([], |row| row.get::<_, String>(0))
    .map_err(|error| error.to_string())?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| error.to_string())?;
  match ids.as_slice() {
    [id] if valid_id(id) => Ok(id.clone()),
    [] => Err("No DSH session is stored in this database".into()),
    _ => Err("DSH database contains multiple sessions; provide an exact session ID".into()),
  }
}

fn trace(db: &Connection, id: &str) -> Result<(Value, String, u64), String> {
  let row = db.query_row("SELECT CASE WHEN length(CAST(header AS BLOB))<=65536 THEN header END, CASE WHEN length(CAST(tail_segment_uuid AS BLOB))<=512 THEN tail_segment_uuid END, inherited_count FROM traces WHERE trace_uuid=?1", [id], |row| {
    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, count(row, 2)?))
  }).optional().map_err(|error| error.to_string())?.ok_or("DSH history has a different session identity")?;
  if row.2 > MAX_SAFE {
    return Err("Invalid DSH inherited event count".into());
  }
  Ok((decode_header(id, &row.0)?, row.1, row.2))
}

fn decode_header(id: &str, text: &str) -> Result<Value, String> {
  if text.len() > MAX_HEADER_BYTES {
    return Err("DSH SQLite header exceeds its size limit".into());
  }
  let mut header: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
  if !valid_id(id) || header["id"] != id || header["version"] != 4 || !header["isSeeded"].is_boolean() {
    return Err("Invalid DSH SQLite trace identity or header version".into());
  }
  header["type"] = Value::String("session".into());
  Ok(header)
}

struct Segment {
  id: String,
  parent: Option<String>,
  parent_end: Option<u64>,
  base: u64,
  count: u64,
}

fn segment(db: &Connection, id: &str) -> Result<Segment, String> {
  db.query_row("SELECT CASE WHEN length(CAST(parent_segment_uuid AS BLOB))>512 THEN 0 ELSE parent_segment_uuid END, parent_end_seq, base_seq, COALESCE((SELECT MAX(end_seq) FROM frames WHERE segment_uuid=?1),0) FROM segments WHERE segment_uuid=?1", [id], |row| {
    Ok(Segment { id: id.into(), parent: row.get(0)?, parent_end: row.get::<_, Option<i64>>(1)?.map(|value| sql_count(value, 1)).transpose()?, base: count(row, 2)?, count: count(row, 3)? })
  }).optional().map_err(|error| error.to_string())?.ok_or_else(|| "DSH trace references a missing segment".into())
}

pub(crate) fn snapshot(path: &Path, expected: Option<&str>, max_bytes: usize) -> Result<Snapshot, String> {
  let (database, id) = resolve(path, expected)?;
  let db = open(&database)?;
  let tx = db.unchecked_transaction().map_err(|error| error.to_string())?;
  let id = select_id(&tx, id)?;
  let (header, tail, inherited_count) = trace(&tx, &id)?;
  let mut current = segment(&tx, &tail)?;
  let mut end = current.count;
  let mut seen = HashSet::new();
  let mut ranges = Vec::new();
  loop {
    if ranges.len() >= MAX_SEGMENTS
      || !seen.insert(current.id.clone())
      || end > current.count
      || current.base.checked_add(end).is_none_or(|value| value > MAX_SAFE)
    {
      return Err("Invalid DSH trace segment ancestry".into());
    }
    let parent = current.parent.clone();
    let parent_end = current.parent_end;
    let base = current.base;
    ranges.push((current, end));
    match (parent, parent_end) {
      (None, None) if base == 0 => break,
      (Some(parent), Some(parent_end)) => {
        current = segment(&tx, &parent)?;
        if current.base.checked_add(parent_end) != Some(base) {
          return Err("Invalid DSH segment offset".into());
        }
        end = parent_end;
      }
      _ => return Err("Invalid DSH segment parent".into()),
    }
  }
  let mut rows = Vec::new();
  let mut remaining = max_bytes
    .checked_sub(serde_json::to_vec(&header).map_err(|error| error.to_string())?.len())
    .ok_or("DSH snapshot exceeds its size limit")?;
  let mut select = tx.prepare("SELECT start_seq,end_seq,codec,CASE WHEN length(payload)<=?3 THEN payload END FROM frames WHERE segment_uuid=?1 AND start_seq<?2 ORDER BY start_seq").map_err(|error| error.to_string())?;
  for (segment, finish) in ranges.into_iter().rev() {
    let mut frames = select
      .query(rusqlite::params![
        segment.id,
        finish as i64,
        i64::try_from(max_bytes).unwrap_or(i64::MAX)
      ])
      .map_err(|error| error.to_string())?;
    let mut expected = 0;
    while let Some(frame) = frames.next().map_err(|error| error.to_string())? {
      let start = count(frame, 0).map_err(|error| error.to_string())?;
      let end = count(frame, 1).map_err(|error| error.to_string())?;
      let codec: i64 = frame.get(2).map_err(|error| error.to_string())?;
      let payload: Vec<u8> = frame.get(3).map_err(|_| "DSH SQLite frame exceeds its size limit")?;
      if start != expected || end <= start || end > MAX_SAFE || end - start > 1_000_000 {
        return Err("DSH SQLite segment has a gap or overlap".into());
      }
      let decoded = match codec {
        0 => payload,
        1 => {
          let mut decoder = zstd::stream::read::Decoder::new(payload.as_slice()).map_err(|error| error.to_string())?;
          decoder.window_log_max(23).map_err(|error| error.to_string())?;
          let mut bytes = Vec::new();
          decoder
            .take(remaining as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
          bytes
        }
        _ => return Err("Unknown DSH SQLite frame codec".into()),
      };
      if decoded.len() > remaining {
        return Err("DSH decoded source exceeds the snapshot size limit".into());
      }
      remaining -= decoded.len();
      let text = std::str::from_utf8(&decoded).map_err(|error| error.to_string())?;
      let members: Vec<_> = text.split('\n').collect();
      if members.len() as u64 != end - start {
        return Err("DSH SQLite frame length does not match its event range".into());
      }
      for (offset, text) in members
        .into_iter()
        .enumerate()
        .take((finish - start).min(end - start) as usize)
      {
        let event: Value = serde_json::from_str(text).map_err(|error| format!("Invalid DSH SQLite event: {error}"))?;
        let seq = segment.base + start + offset as u64;
        if event["seq"].as_u64() != Some(seq) || seq != rows.len() as u64 {
          return Err("DSH SQLite event sequence does not match its segment".into());
        }
        rows.push(event);
      }
      expected = end.min(finish);
    }
    if expected != finish {
      return Err("DSH SQLite segment is incomplete".into());
    }
  }
  drop(select);
  tx.commit().map_err(|error| error.to_string())?;
  Ok(Snapshot {
    header,
    rows,
    inherited_count,
  })
}

fn valid_id(id: &str) -> bool {
  !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}

fn count(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
  sql_count(row.get(index)?, index)
}

fn sql_count(value: i64, index: usize) -> rusqlite::Result<u64> {
  u64::try_from(value)
    .ok()
    .filter(|value| *value <= MAX_SAFE)
    .ok_or_else(|| {
      rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Integer,
        Box::new(std::io::Error::other("Invalid DSH nonnegative safe integer")),
      )
    })
}
