//! Read-only access to native rollout representations. All cursors and history
//! cutoffs address decoded JSONL bytes, never compressed filesystem offsets.
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{
  Arc, OnceLock,
  atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

const DEFAULT_SCRATCH_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_READ_DURATION: Duration = Duration::from_secs(120);

#[derive(Debug)]
struct ScratchUsage {
  limit: u64,
  used: AtomicU64,
}

/// Shared quota for temporary decoded Codex files, charged until their last
/// owning handle closes. It does not limit source or retained history size.
#[derive(Clone, Debug)]
pub struct CodexScratchBudget {
  usage: Arc<ScratchUsage>,
}

impl CodexScratchBudget {
  pub fn new(limit: u64) -> Self {
    Self {
      usage: Arc::new(ScratchUsage {
        limit,
        used: AtomicU64::new(0),
      }),
    }
  }

  pub fn used_bytes(&self) -> u64 {
    self.usage.used.load(Ordering::Acquire)
  }

  fn reserve(&self, bytes: u64) -> Result<(), String> {
    self
      .usage
      .used
      .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
        used.checked_add(bytes).filter(|next| *next <= self.usage.limit)
      })
      .map(|_| ())
      .map_err(|_| {
        format!(
          "Codex history scratch disk budget exhausted ({} bytes)",
          self.usage.limit
        )
      })
  }

  fn release(&self, bytes: u64) {
    self.usage.used.fetch_sub(bytes, Ordering::AcqRel);
  }
}

impl Default for CodexScratchBudget {
  fn default() -> Self {
    static SHARED: OnceLock<CodexScratchBudget> = OnceLock::new();
    SHARED.get_or_init(|| Self::new(DEFAULT_SCRATCH_BYTES)).clone()
  }
}

/// One cooperative deadline shared by all source reads in an operation.
#[derive(Clone)]
pub(crate) struct ReadWorkBudget {
  deadline: Instant,
}

impl ReadWorkBudget {
  pub(crate) fn new(duration: Duration) -> Self {
    let now = Instant::now();
    Self {
      deadline: now.checked_add(duration).unwrap_or(now),
    }
  }

  pub(crate) fn check(&self) -> Result<(), String> {
    if Instant::now() >= self.deadline {
      Err("Codex history read exceeded its work deadline".into())
    } else {
      Ok(())
    }
  }
}

impl Default for ReadWorkBudget {
  fn default() -> Self {
    Self::new(DEFAULT_READ_DURATION)
  }
}

struct BudgetReader<R> {
  input: R,
  work: ReadWorkBudget,
}

impl<R: Read> Read for BudgetReader<R> {
  fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
    if output.is_empty() {
      return Ok(0);
    }
    self.work.check().map_err(io::Error::other)?;
    let read = self.input.read(output)?;
    self.work.check().map_err(io::Error::other)?;
    Ok(read)
  }
}

/// A seekable source; decoded temporary files retain their quota reservation.
pub(crate) struct ScratchFile {
  file: Option<File>,
  scratch: CodexScratchBudget,
  charged: u64,
}

impl Deref for ScratchFile {
  type Target = File;

  fn deref(&self) -> &File {
    self.file.as_ref().unwrap()
  }
}

impl DerefMut for ScratchFile {
  fn deref_mut(&mut self) -> &mut File {
    self.file.as_mut().unwrap()
  }
}

impl Drop for ScratchFile {
  fn drop(&mut self) {
    // Close the anonymous temporary file before making its quota available.
    drop(self.file.take());
    self.scratch.release(self.charged);
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
  collect_rollouts_with_budget(root, paths, &ReadWorkBudget::default())
}

pub(crate) fn collect_rollouts_with_budget(
  root: &Path,
  paths: &mut Vec<PathBuf>,
  work: &ReadWorkBudget,
) -> Result<(), String> {
  work.check()?;
  if !root.exists() {
    return Ok(());
  }
  for entry in std::fs::read_dir(root).map_err(|err| format!("failed to read {}: {err}", root.display()))? {
    work.check()?;
    let entry = entry.map_err(|err| err.to_string())?;
    let kind = entry.file_type().map_err(|err| err.to_string())?;
    let path = entry.path();
    if kind.is_dir() {
      collect_rollouts_with_budget(&path, paths, work)?;
    } else if kind.is_file() && is_rollout(&path) && (!is_compressed(&path) || !logical_path(&path).is_file()) {
      paths.push(path);
    }
  }
  Ok(())
}

pub(crate) fn collect_rollout_catalog(
  root: &Path,
  catalog: &mut tokn_session_core::FileSessionCatalog,
  optional: bool,
) {
  let entries = match std::fs::read_dir(root) {
    Ok(entries) => entries,
    Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => return,
    Err(_) => {
      catalog.complete = false;
      return;
    }
  };
  for entry in entries {
    let Ok(entry) = entry else {
      catalog.complete = false;
      continue;
    };
    let Ok(kind) = entry.file_type() else {
      catalog.complete = false;
      continue;
    };
    let path = entry.path();
    if kind.is_dir() {
      collect_rollout_catalog(&path, catalog, false);
    } else if kind.is_file() && is_rollout(&path) {
      if is_compressed(&path) {
        match std::fs::metadata(logical_path(&path)) {
          Ok(metadata) if metadata.is_file() => continue,
          Ok(_) => {}
          Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
          Err(_) => catalog.complete = false,
        }
      }
      catalog.paths.push(path);
    }
  }
}

/// Pin the physical EOF, including across concatenated compressed frames, so
/// a historical read cannot follow a concurrently growing source indefinitely.
pub(crate) fn reader(path: &Path) -> Result<Box<dyn BufRead>, String> {
  reader_with_budget(path, &ReadWorkBudget::default())
}

pub(crate) fn reader_with_budget(path: &Path, work: &ReadWorkBudget) -> Result<Box<dyn BufRead>, String> {
  work.check()?;
  let path = resolve_path(path);
  let file = File::open(&path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  let length = file.metadata().map_err(|err| err.to_string())?.len();
  let input = BudgetReader {
    input: file.take(length),
    work: work.clone(),
  };
  if is_compressed(&path) {
    let decoder =
      zstd::stream::read::Decoder::new(input).map_err(|err| format!("failed to decode {}: {err}", path.display()))?;
    Ok(Box::new(BufReader::new(BudgetReader {
      input: decoder,
      work: work.clone(),
    })))
  } else {
    Ok(Box::new(BufReader::new(input)))
  }
}

/// Seek plain files directly; advance a compressed stream without retaining its
/// skipped body. Prefix discovery needs only the final row and boundary row.
#[cfg(test)]
pub(crate) fn reader_at(path: &Path, offset: u64) -> Result<Box<dyn BufRead>, String> {
  reader_at_with_budget(path, offset, &ReadWorkBudget::default())
}

#[cfg(test)]
pub(crate) fn reader_at_with_budget(
  path: &Path,
  offset: u64,
  work: &ReadWorkBudget,
) -> Result<Box<dyn BufRead>, String> {
  reader_at_if_present(path, offset, work)?.ok_or_else(|| "Codex history changed while reading its prefix".into())
}

pub(crate) fn reader_at_if_present(
  path: &Path,
  offset: u64,
  work: &ReadWorkBudget,
) -> Result<Option<Box<dyn BufRead>>, String> {
  work.check()?;
  let path = resolve_path(path);
  if !is_compressed(&path) {
    let mut file = File::open(&path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let length = file.metadata().map_err(|err| err.to_string())?.len();
    if offset > length {
      return Ok(None);
    }
    file.seek(SeekFrom::Start(offset)).map_err(|err| err.to_string())?;
    return Ok(Some(Box::new(BufReader::new(BudgetReader {
      input: file.take(length - offset),
      work: work.clone(),
    }))));
  }
  let mut input = reader_with_budget(&path, work)?;
  let skipped = io::copy(&mut Read::by_ref(&mut input).take(offset), &mut io::sink())
    .map_err(|err| format!("failed to decode {}: {err}", path.display()))?;
  if skipped != offset {
    return Ok(None);
  }
  Ok(Some(input))
}

/// Backward window discovery needs seeking. Decode once per rebuild to an
/// anonymous temporary file, shared by boundary scanning and normalization.
/// Inherited sources decode only their retained prefix; excluded suffixes need
/// neither disk space nor normalization. No source files are materialized.
pub(crate) fn seekable(
  path: &Path,
  end: Option<u64>,
  scratch: &CodexScratchBudget,
  work: &ReadWorkBudget,
) -> Result<ScratchFile, String> {
  work.check()?;
  let path = resolve_path(path);
  if !is_compressed(&path) {
    let file = File::open(&path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    if let Some(end) = end {
      if end > file.metadata().map_err(|err| err.to_string())?.len() {
        return Err("Codex history changed while reading its prefix".into());
      }
    }
    work.check()?;
    return Ok(ScratchFile {
      file: Some(file),
      scratch: scratch.clone(),
      charged: 0,
    });
  }
  let file = tempfile::tempfile().map_err(|err| format!("failed to create Codex history scratch file: {err}"))?;
  let mut decoded = ScratchFile {
    file: Some(file),
    scratch: scratch.clone(),
    charged: 0,
  };
  let mut input = reader_with_budget(&path, work)?;
  let mut chunk = [0; 64 * 1024];
  loop {
    work.check()?;
    let length = end.map_or(chunk.len(), |end| {
      (end - decoded.charged).min(chunk.len() as u64) as usize
    });
    if length == 0 {
      break;
    }
    let read = input
      .read(&mut chunk[..length])
      .map_err(|err| format!("failed to decode {}: {err}", path.display()))?;
    if read == 0 {
      if end.is_some() {
        return Err("Codex history changed while reading its prefix".into());
      }
      break;
    }
    scratch.reserve(read as u64)?;
    decoded.charged += read as u64;
    decoded
      .write_all(&chunk[..read])
      .map_err(|err| format!("failed to write Codex history scratch file: {err}"))?;
  }
  decoded.rewind().map_err(|err| err.to_string())?;
  work.check()?;
  Ok(decoded)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn compressed(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, zstd::stream::encode_all(bytes, 0).unwrap()).unwrap();
    path
  }

  fn read_bytes(mut input: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes).unwrap();
    bytes
  }

  #[test]
  fn scratch_quota_is_shared_until_files_close_and_failed_reads_release_it() {
    let root = tempfile::tempdir().unwrap();
    let path = compressed(root.path(), "first.jsonl.zst", b"12345678");
    let scratch = CodexScratchBudget::new(12);
    let work = ReadWorkBudget::default();
    let first = seekable(&path, None, &scratch, &work).unwrap();
    assert_eq!(scratch.used_bytes(), 8);
    let peer = scratch.clone();
    assert!(
      seekable(&path, None, &peer, &work)
        .err()
        .unwrap()
        .contains("scratch disk budget")
    );
    assert_eq!(scratch.used_bytes(), 8);
    drop(first);
    assert_eq!(scratch.used_bytes(), 0);
    let retry = seekable(&path, None, &peer, &work).unwrap();
    assert_eq!(peer.used_bytes(), 8);
    drop(retry);
    assert_eq!(peer.used_bytes(), 0);

    assert!(
      seekable(&path, Some(9), &scratch, &work)
        .err()
        .unwrap()
        .contains("prefix")
    );
    assert_eq!(scratch.used_bytes(), 0);
    let path = compressed(root.path(), "malformed.jsonl.zst", b"12345678");
    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(b"not a frame")
      .unwrap();
    assert!(seekable(&path, None, &scratch, &work).is_err());
    assert_eq!(scratch.used_bytes(), 0);
  }

  #[test]
  fn seekable_plain_and_compressed_bytes_match_and_only_scratch_is_charged() {
    let root = tempfile::tempdir().unwrap();
    let bytes = b"retained\nexcluded\n";
    let plain = root.path().join("plain.jsonl");
    std::fs::write(&plain, bytes).unwrap();
    let packed = compressed(root.path(), "packed.jsonl.zst", bytes);
    let work = ReadWorkBudget::default();
    let scratch = CodexScratchBudget::new(bytes.len() as u64);
    let mut plain_file = seekable(&plain, None, &scratch, &work).unwrap();
    assert_eq!(scratch.used_bytes(), 0);
    let mut decoded_file = seekable(&packed, None, &scratch, &work).unwrap();
    assert_eq!(scratch.used_bytes(), bytes.len() as u64);
    assert_eq!(read_bytes(&mut *plain_file), read_bytes(&mut *decoded_file));
    drop(decoded_file);
    assert_eq!(scratch.used_bytes(), 0);

    let mut prefix = seekable(&packed, Some(9), &scratch, &work).unwrap();
    assert_eq!(read_bytes(&mut *prefix), b"retained\n");
    assert_eq!(scratch.used_bytes(), 9);
    drop(prefix);
    let zero = CodexScratchBudget::new(0);
    assert!(seekable(&plain, None, &zero, &work).is_ok());
    assert!(seekable(&packed, None, &zero, &work).is_err());
  }

  #[test]
  fn source_streams_pin_eof_and_validate_exact_decoded_offsets() {
    let root = tempfile::tempdir().unwrap();
    for packed in [false, true] {
      let path = if packed {
        compressed(root.path(), "packed.jsonl.zst", b"1234")
      } else {
        let path = root.path().join("plain.jsonl");
        std::fs::write(&path, b"1234").unwrap();
        path
      };
      let input = reader(&path).unwrap();
      let appended = if packed {
        zstd::stream::encode_all(b"5678".as_slice(), 0).unwrap()
      } else {
        b"5678".to_vec()
      };
      std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&appended)
        .unwrap();
      assert_eq!(read_bytes(input), b"1234");
      assert_eq!(read_bytes(reader(&path).unwrap()), b"12345678");
      assert_eq!(read_bytes(reader_at(&path, 4).unwrap()), b"5678");
      assert_eq!(read_bytes(reader_at(&path, 8).unwrap()), b"");
      assert!(reader_at(&path, 9).err().unwrap().contains("prefix"));
    }
  }

  #[test]
  fn expired_work_budget_rejects_reads_without_consuming_scratch_quota() {
    let root = tempfile::tempdir().unwrap();
    let path = compressed(root.path(), "packed.jsonl.zst", b"data");
    let work = ReadWorkBudget::new(Duration::ZERO);
    let scratch = CodexScratchBudget::new(100);
    assert!(
      reader_with_budget(&path, &work)
        .err()
        .unwrap()
        .contains("work deadline")
    );
    assert!(
      reader_at_with_budget(&path, 1, &work)
        .err()
        .unwrap()
        .contains("work deadline")
    );
    assert!(
      seekable(&path, None, &scratch, &work)
        .err()
        .unwrap()
        .contains("work deadline")
    );
    assert_eq!(scratch.used_bytes(), 0);
    let mut input = BudgetReader {
      input: b"data".as_slice(),
      work,
    };
    assert!(input.read(&mut [0]).unwrap_err().to_string().contains("work deadline"));
    assert_eq!(input.input, b"data");
  }
}
