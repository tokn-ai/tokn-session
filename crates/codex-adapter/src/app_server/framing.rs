use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};

// A 24 MiB image batch becomes 32 MiB of base64. Reserve bounded room for
// accompanying text and the JSON envelope, independently of line buffering.
pub(super) const MAX_FRAME_BYTES: usize = 34 * 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// Keep one reader for the connection lifetime: an LF read can buffer bytes
/// belonging to subsequent messages.
pub(super) struct RpcReader<R> {
  reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> RpcReader<R> {
  pub(super) fn new(reader: R) -> Self {
    Self {
      reader: BufReader::new(reader),
    }
  }

  pub(super) async fn read_frame(&mut self) -> Result<Option<Value>, String> {
    read_frame(&mut self.reader).await
  }
}

async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<Value>, String> {
  let mut bytes = Vec::new();
  loop {
    let (consumed, complete) = {
      let available = reader
        .fill_buf()
        .await
        .map_err(|error| format!("Could not read Codex RPC stdout: {error}"))?;
      if available.is_empty() {
        return if bytes.is_empty() {
          Ok(None)
        } else {
          Err("Codex RPC stdout ended before the LF frame delimiter".into())
        };
      }
      let newline = available.iter().position(|byte| *byte == b'\n');
      let length = newline.unwrap_or(available.len());
      if length > MAX_FRAME_BYTES - bytes.len() {
        return Err("Codex RPC frame exceeds the 34 MiB limit".into());
      }
      bytes.extend_from_slice(&available[..length]);
      (length + usize::from(newline.is_some()), newline.is_some())
    };
    reader.consume(consumed);
    if complete {
      if bytes.last() == Some(&b'\r') {
        bytes.pop();
      }
      return serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Invalid Codex RPC JSON frame: {error}"));
    }
  }
}

/// Bound diagnostic bytes even when stderr has no newlines or splits UTF-8.
#[derive(Default)]
pub(super) struct StderrTail {
  bytes: Vec<u8>,
}

impl StderrTail {
  pub(super) fn push(&mut self, bytes: &[u8]) {
    if bytes.len() >= STDERR_TAIL_BYTES {
      self.bytes.clear();
      self.bytes.extend_from_slice(&bytes[bytes.len() - STDERR_TAIL_BYTES..]);
    } else {
      let overflow = (self.bytes.len() + bytes.len()).saturating_sub(STDERR_TAIL_BYTES);
      self.bytes.drain(..overflow);
      self.bytes.extend_from_slice(bytes);
    }
  }

  pub(super) fn text(&self) -> String {
    String::from_utf8_lossy(&self.bytes).into_owned()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[tokio::test]
  async fn keeps_buffered_frames_and_accepts_crlf() {
    let mut reader = RpcReader::new(&b"{\"a\":1}\r\n{\"b\":2}\n"[..]);
    assert_eq!(reader.read_frame().await.unwrap(), Some(json!({"a":1})));
    assert_eq!(reader.read_frame().await.unwrap(), Some(json!({"b":2})));
    assert!(reader.read_frame().await.unwrap().is_none());
  }

  #[tokio::test]
  async fn rejects_truncated_and_invalid_json_frames() {
    assert!(RpcReader::new(&b"{\"a\":1}"[..]).read_frame().await.is_err());
    assert!(RpcReader::new(&b"not json\n"[..]).read_frame().await.is_err());
    assert!(RpcReader::new(&b"\n"[..]).read_frame().await.is_err());
  }

  #[test]
  fn diagnostics_keep_only_bounded_tail() {
    let mut tail = StderrTail::default();
    tail.push(&vec![b'x'; STDERR_TAIL_BYTES + 1]);
    tail.push(b"end");
    assert_eq!(tail.text().len(), STDERR_TAIL_BYTES);
    assert!(tail.text().ends_with("end"));
  }
}
