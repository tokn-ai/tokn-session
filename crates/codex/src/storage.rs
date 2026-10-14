//! Read-only access to native rollout representations. All cursors and history
//! cutoffs address decoded JSONL bytes, never compressed filesystem offsets.
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub(crate) const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;

struct DecodedLimit<R> {
  input: R,
  remaining: u64,
}

impl<R: Read> Read for DecodedLimit<R> {
  fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
    if output.is_empty() {
      return Ok(0);
    }
    if self.remaining == 0 {
      let mut extra = [0];
      return match self.input.read(&mut extra)? {
        0 => Ok(0),
        _ => Err(io::Error::other("Codex decoded source exceeds the snapshot size limit")),
      };
    }
    let limit = output.len().min(self.remaining as usize);
    let read = self.input.read(&mut output[..limit])?;
    self.remaining -= read as u64;
    Ok(read)
  }
}

pub(crate) fn is_compressed(path: &Path) -> bool {
  path
    .file_name()
    .is_some_and(|name| name.to_string_lossy().ends_with(".jsonl.zst"))
}

pub(crate) fn is_rollout(path: &Path) -> bool {
  path.extension().is_some_and(|extension| extension == "jsonl") || is_compressed(path)
}

pub(crate) fn logical_path(path: &Path) -> PathBuf {
  if is_compressed(path) {
    path.with_extension("")
  } else {
    path.to_path_buf()
  }
}

pub(crate) fn resolve_path(path: &Path) -> PathBuf {
  let plain = logical_path(path);
  if plain.is_file() {
    return plain;
  }
  if plain.extension().is_some_and(|extension| extension == "jsonl") {
    let compressed = plain.with_extension("jsonl.zst");
    if compressed.is_file() {
      return compressed;
    }
  }
  path.to_path_buf()
}

pub(crate) fn collect_rollouts(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
  if !root.exists() {
    return Ok(());
  }
  for entry in std::fs::read_dir(root).map_err(|err| format!("failed to read {}: {err}", root.display()))? {
    let entry = entry.map_err(|err| err.to_string())?;
    let kind = entry.file_type().map_err(|err| err.to_string())?;
    let path = entry.path();
    if kind.is_dir() {
      collect_rollouts(&path, paths)?;
    } else if kind.is_file() && is_rollout(&path) && (!is_compressed(&path) || !logical_path(&path).is_file()) {
      paths.push(path);
    }
  }
  Ok(())
}

/// Pin the physical EOF, including across concatenated compressed frames, so
/// a historical read cannot follow a concurrently growing source indefinitely.
pub(crate) fn reader(path: &Path) -> Result<Box<dyn BufRead>, String> {
  let path = resolve_path(path);
  let file = File::open(&path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  let length = file.metadata().map_err(|err| err.to_string())?.len();
  let input = file.take(length);
  if is_compressed(&path) {
    let decoder =
      zstd::stream::read::Decoder::new(input).map_err(|err| format!("failed to decode {}: {err}", path.display()))?;
    Ok(Box::new(BufReader::new(DecodedLimit {
      input: decoder,
      remaining: MAX_DECODED_BYTES,
    })))
  } else {
    Ok(Box::new(BufReader::new(input)))
  }
}

/// Seek plain files directly; advance a compressed stream without retaining its
/// skipped body. Prefix discovery needs only the final row and boundary row.
pub(crate) fn reader_at(path: &Path, offset: u64) -> Result<Box<dyn BufRead>, String> {
  let path = resolve_path(path);
  if !is_compressed(&path) {
    let mut file = File::open(&path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let length = file.metadata().map_err(|err| err.to_string())?.len();
    file.seek(SeekFrom::Start(offset)).map_err(|err| err.to_string())?;
    return Ok(Box::new(BufReader::new(file.take(length.saturating_sub(offset)))));
  }
  if offset > MAX_DECODED_BYTES {
    return Err("Codex decoded source exceeds the snapshot size limit".into());
  }
  let mut input = reader(&path)?;
  io::copy(&mut Read::by_ref(&mut input).take(offset), &mut io::sink())
    .map_err(|err| format!("failed to decode {}: {err}", path.display()))?;
  Ok(input)
}

/// Backward window discovery needs seeking. Decode once per rebuild to an
/// anonymous temporary file, shared by boundary scanning and normalization.
/// Inherited sources decode only their retained prefix; excluded suffixes need
/// neither disk space nor normalization. No source files are materialized.
pub(crate) fn seekable(path: &Path, end: Option<u64>) -> Result<File, String> {
  let path = resolve_path(path);
  if !is_compressed(&path) {
    return File::open(&path).map_err(|err| format!("failed to open {}: {err}", path.display()));
  }
  let limit = end.unwrap_or(MAX_DECODED_BYTES);
  if limit > MAX_DECODED_BYTES {
    return Err("Codex decoded source exceeds the snapshot size limit".into());
  }
  let mut decoded =
    tempfile::tempfile().map_err(|err| format!("failed to create Codex history scratch file: {err}"))?;
  let length = io::copy(&mut reader(&path)?.take(limit + u64::from(end.is_none())), &mut decoded)
    .map_err(|err| format!("failed to decode {}: {err}", path.display()))?;
  if end.is_some() && length != limit {
    return Err("Codex history changed while reading its prefix".into());
  }
  if length > MAX_DECODED_BYTES {
    return Err("Codex decoded source exceeds the snapshot size limit".into());
  }
  decoded.rewind().map_err(|err| err.to_string())?;
  Ok(decoded)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn decoded_limit_reports_excess_instead_of_returning_a_partial_stream() {
    for bytes in [b"123".as_slice(), b"1234".as_slice()] {
      let mut reader = DecodedLimit {
        input: bytes,
        remaining: 3,
      };
      let mut output = Vec::new();
      let result = reader.read_to_end(&mut output);
      assert_eq!(output, b"123");
      if bytes.len() == 3 {
        assert!(result.is_ok());
      } else {
        assert!(result.unwrap_err().to_string().contains("size limit"));
      }
    }
  }
}
