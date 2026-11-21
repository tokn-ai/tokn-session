use serde_json::Value;
use std::{
  fs::File,
  io::{BufRead, BufReader},
  path::{Path, PathBuf},
};
use tokn_session_core::FileSessionCatalog;

pub(crate) const MAX_SOURCE_BYTES: usize = 128 * 1024 * 1024;

/// Qoder's documented configuration override contains a `projects` directory.
pub fn default_session_root() -> Result<PathBuf, String> {
  let config = std::env::var_os("QODER_CONFIG_DIR")
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .or_else(|| dirs::home_dir().map(|home| home.join(".qoder")))
    .ok_or("could not resolve Qoder's configuration directory")?;
  Ok(config.join("projects"))
}

/// Native Qoder 1.1.94 project-directory encoding, including its long-path hash.
pub fn project_identifier(directory: &str) -> String {
  let encoded: String = directory
    .encode_utf16()
    .map(|unit| {
      if unit <= 127 && (unit as u8).is_ascii_alphanumeric() {
        unit as u8 as char
      } else {
        '-'
      }
    })
    .collect();
  if encoded.len() <= 200 {
    return encoded;
  }
  // JavaScript's bitwise XOR truncates the native accumulator to signed i32.
  let hash = directory
    .encode_utf16()
    .fold(5381_i32, |hash, unit| hash.wrapping_mul(33) ^ i32::from(unit));
  let mut number = i64::from(hash).unsigned_abs();
  let mut suffix = Vec::new();
  loop {
    let digit = (number % 36) as u8;
    suffix.push(if digit < 10 { b'0' + digit } else { b'a' + digit - 10 });
    number /= 36;
    if number == 0 {
      break;
    }
  }
  suffix.reverse();
  format!("{}-{}", &encoded[..200], String::from_utf8(suffix).expect("base36"))
}

pub(crate) fn catalog(root: &Path) -> FileSessionCatalog {
  let mut output = FileSessionCatalog {
    paths: Vec::new(),
    complete: true,
  };
  collect(root, 0, &mut output);
  output.paths.sort();
  output
}

fn collect(root: &Path, depth: usize, output: &mut FileSessionCatalog) {
  let entries = match std::fs::read_dir(root) {
    Ok(entries) => entries,
    Err(_) => {
      output.complete = false;
      return;
    }
  };
  for entry in entries {
    let Ok(entry) = entry else {
      output.complete = false;
      continue;
    };
    let Ok(kind) = entry.file_type() else {
      output.complete = false;
      continue;
    };
    let path = entry.path();
    if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
      // Native per-run logs, workflow journals and sidecar metadata are not transcripts.
      let name = entry.file_name();
      let name = name.to_string_lossy();
      let subagent = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "subagents");
      if !name.starts_with("agent-") || subagent {
        output.paths.push(path);
      }
    } else if kind.is_dir() && depth == 0 {
      collect(&path, 1, output);
    } else if kind.is_dir() && depth == 1 {
      if entry.file_name() == "subagents" {
        collect(&path, 2, output);
      } else {
        let subagents = path.join("subagents");
        match std::fs::symlink_metadata(&subagents) {
          Ok(metadata) if metadata.file_type().is_dir() => collect(&subagents, 2, output),
          Ok(_) => {}
          Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
          Err(_) => output.complete = false,
        }
      }
    }
  }
}

pub(crate) fn first_identity(path: &Path) -> Result<Value, String> {
  let file = File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
  // Native transcripts have no dedicated header: their first user record may
  // contain a large image. Serde skips unknown message bodies without retaining
  // their strings, rather than allocating an entire image-bearing JSON row.
  #[derive(serde::Deserialize)]
  #[serde(rename_all = "camelCase")]
  struct Identity {
    #[serde(rename = "type")]
    kind: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<Value>,
    agent_id: Option<String>,
    is_sidechain: Option<bool>,
  }
  let reader = BufReader::new(file).take(MAX_SOURCE_BYTES as u64);
  for row in serde_json::Deserializer::from_reader(reader).into_iter::<Identity>() {
    let row = row.map_err(|err| format!("invalid Qoder transcript header: {err}"))?;
    if matches!(
      row.kind.as_deref(),
      Some("user" | "assistant" | "system" | "attachment")
    ) && row.session_id.as_deref().is_some_and(valid_id)
      && row.cwd.as_deref().is_some_and(native_absolute_path)
    {
      return Ok(
        serde_json::json!({"type":row.kind,"sessionId":row.session_id,"cwd":row.cwd,"timestamp":row.timestamp,"agentId":row.agent_id,"isSidechain":row.is_sidechain}),
      );
    }
  }
  Err("Qoder transcript has no bounded, valid session identity".into())
}

pub(crate) fn valid_id(value: &str) -> bool {
  !value.is_empty()
    && value.len() <= 256
    && !value.contains("..")
    && value
      .bytes()
      .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_' | b'.'))
}

/// Historical records can come from a different operating system. This is
/// lexical native-path validation, never permission to access a host path.
pub(crate) fn native_absolute_path(value: &str) -> bool {
  if value.contains('\0') {
    return false;
  }
  if value.starts_with('/') {
    return true;
  }
  let bytes = value.as_bytes();
  if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\') {
    return true;
  }
  if let Some(unc) = value.strip_prefix("\\\\") {
    let mut parts = unc.split('\\');
    return parts.next().is_some_and(|part| !part.is_empty()) && parts.next().is_some_and(|part| !part.is_empty());
  }
  false
}

/// Metadata-only streaming pass for exact bind verification. Large message
/// rows are drained without allocating/parsing their bodies. A relocated
/// transcript retains its original message cwd, so its latest native relocation
/// record must be applied before a host verifies the current project.
pub(crate) fn effective_cwd(path: &Path, session_id: &str, initial: String) -> Result<String, String> {
  let file = File::open(path).map_err(|err| format!("failed to open Qoder metadata: {err}"))?;
  if file.metadata().map_err(|err| err.to_string())?.len() > MAX_SOURCE_BYTES as u64 {
    return Err("Qoder source exceeds the metadata verification size limit".into());
  }
  let mut reader = BufReader::new(file);
  let mut cwd = initial;
  let mut line = Vec::new();
  let mut oversized = false;
  let mut observed = 0_usize;
  loop {
    let buffer = reader
      .fill_buf()
      .map_err(|err| format!("failed to read Qoder metadata: {err}"))?;
    if buffer.is_empty() {
      break;
    }
    let size = buffer
      .iter()
      .position(|byte| *byte == b'\n')
      .map_or(buffer.len(), |index| index + 1);
    observed = observed.saturating_add(size);
    if observed > MAX_SOURCE_BYTES {
      return Err("Qoder source changed beyond the metadata verification size limit".into());
    }
    let completed = buffer[size - 1] == b'\n';
    if !oversized && line.len() + size <= 64 * 1024 {
      line.extend_from_slice(&buffer[..size]);
    } else {
      oversized = true;
      line.clear();
    }
    reader.consume(size);
    if completed {
      if !oversized
        && let Ok(row) = serde_json::from_slice::<Value>(&line)
        && row["type"] == "relocated"
        && row["sessionId"] == session_id
        && let Some(next) = row["relocatedCwd"].as_str().filter(|cwd| native_absolute_path(cwd))
      {
        cwd = next.to_owned();
      }
      line.clear();
      oversized = false;
    }
  }
  Ok(cwd)
}

use std::io::Read;
