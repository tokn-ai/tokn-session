//! Bounded incremental SSE decoding, including CRLF and split UTF-8 chunks.
use crate::api::Request;

pub struct Decoder {
  line: Vec<u8>,
  data: Vec<u8>,
  limit: usize,
}

impl Decoder {
  pub fn new(max_frame_bytes: usize) -> Self {
    Self {
      line: Vec::new(),
      data: Vec::new(),
      limit: max_frame_bytes,
    }
  }

  pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Request>, String> {
    let mut frames = Vec::new();
    for byte in bytes {
      if *byte != b'\n' {
        if self.line.len() + self.data.len() >= self.limit {
          return Err("DSH SSE frame exceeds the byte limit".into());
        }
        self.line.push(*byte);
        continue;
      }
      if self.line.last() == Some(&b'\r') {
        self.line.pop();
      }
      if self.line.is_empty() && !self.data.is_empty() {
        let frame: Request =
          serde_json::from_slice(&self.data).map_err(|error| format!("invalid DSH SSE frame: {error}"))?;
        if frame.native_type != "server-request" {
          return Err("DSH SSE frame is not a server-request".into());
        }
        frames.push(frame);
        self.data.clear();
      } else if let Some(data) = self.line.strip_prefix(b"data:") {
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if !self.data.is_empty() {
          self.data.push(b'\n');
        }
        self.data.extend_from_slice(data);
      }
      self.line.clear();
    }
    Ok(frames)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn handles_comments_crlf_and_every_byte_split() {
    let wire = ": connected\r\n\r\ndata: {\"type\":\"server-request\",\"rpcId\":\"q\",\"method\":\"question/requested\",\"payload\":{\"title\":\"中文\"}}\r\n\r\n";
    let mut decoder = Decoder::new(1024);
    let mut frames = Vec::new();
    for byte in wire.as_bytes() {
      frames.extend(decoder.push(&[*byte]).unwrap());
    }
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].payload["title"], "中文");
  }

  #[test]
  fn bounds_unterminated_and_multiline_frames() {
    assert!(Decoder::new(8).push(b"123456789").is_err());
    assert!(Decoder::new(12).push(b"data: 12345\ndata: 67890\n").is_err());
  }
}
