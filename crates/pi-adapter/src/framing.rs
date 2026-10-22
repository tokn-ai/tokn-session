use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};

/// Maximum bytes before LF, including an optional CR. A 24 MiB image batch
/// becomes 32 MiB of base64; leave bounded room for its text and JSON envelope.
pub const MAX_FRAME_BYTES: usize = 34 * 1024 * 1024;
pub const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// Retain this reader for the process lifetime so bytes after an LF stay buffered.
pub struct RpcReader<R> {
  reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> RpcReader<R> {
  pub fn new(reader: R) -> Self {
    Self {
      reader: BufReader::new(reader),
    }
  }

  pub async fn read_frame(&mut self) -> Result<Option<Value>, String> {
    read_frame(&mut self.reader).await
  }
}

/// Clean EOF is distinct from a truncated frame. Invalid frames are terminal:
/// callers must stop the process rather than try to recover delimiter alignment.
pub async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<Value>, String> {
  let mut bytes = Vec::new();
  loop {
    let (consumed, complete) = {
      let available = reader
        .fill_buf()
        .await
        .map_err(|error| format!("failed to read Pi RPC stdout: {error}"))?;
      if available.is_empty() {
        return if bytes.is_empty() {
          Ok(None)
        } else {
          Err("Pi RPC stdout ended before the LF frame delimiter".to_string())
        };
      }
      let newline = available.iter().position(|byte| *byte == b'\n');
      let payload_length = newline.unwrap_or(available.len());
      if payload_length > MAX_FRAME_BYTES - bytes.len() {
        return Err("Pi RPC frame exceeds the 34 MiB limit".to_string());
      }
      bytes.extend_from_slice(&available[..payload_length]);
      (payload_length + usize::from(newline.is_some()), newline.is_some())
    };
    reader.consume(consumed);
    if complete {
      if bytes.last() == Some(&b'\r') {
        bytes.pop();
      }
      return serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("invalid Pi RPC JSON frame: {error}"));
    }
  }
}

/// Bounded diagnostic bytes, independent of stderr line lengths or UTF-8 splits.
#[derive(Default)]
pub struct StderrTail {
  bytes: Vec<u8>,
}

impl StderrTail {
  pub fn push(&mut self, bytes: &[u8]) {
    if bytes.len() >= STDERR_TAIL_BYTES {
      self.bytes.clear();
      self.bytes.extend_from_slice(&bytes[bytes.len() - STDERR_TAIL_BYTES..]);
    } else {
      let overflow = (self.bytes.len() + bytes.len()).saturating_sub(STDERR_TAIL_BYTES);
      self.bytes.drain(..overflow);
      self.bytes.extend_from_slice(bytes);
    }
  }

  pub fn text(&self) -> String {
    String::from_utf8_lossy(&self.bytes).into_owned()
  }
}
