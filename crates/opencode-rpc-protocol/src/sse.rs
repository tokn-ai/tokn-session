use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SseError {
  TooLarge,
  InvalidUtf8,
}

impl fmt::Display for SseError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::TooLarge => "OpenCode event exceeds the configured byte limit",
      Self::InvalidUtf8 => "OpenCode event is not valid UTF-8",
    })
  }
}
impl std::error::Error for SseError {}

/// Incremental, bounded SSE data decoder. SSE control fields are not replay cursors.
pub struct SseDecoder {
  limit: usize,
  line: Vec<u8>,
  data: String,
  has_data: bool,
}

impl SseDecoder {
  pub fn new(limit: usize) -> Self {
    Self {
      limit,
      line: Vec::new(),
      data: String::new(),
      has_data: false,
    }
  }

  pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, SseError> {
    let mut events = Vec::new();
    for &byte in bytes {
      if byte == b'\n' {
        self.line(&mut events)?;
      } else {
        if self.line.len().saturating_add(self.data.len()) >= self.limit {
          return Err(SseError::TooLarge);
        }
        self.line.push(byte);
      }
    }
    Ok(events)
  }

  pub fn finish(&mut self) -> Result<Vec<String>, SseError> {
    let mut events = Vec::new();
    if !self.line.is_empty() {
      self.line(&mut events)?;
    }
    self.dispatch(&mut events);
    Ok(events)
  }

  fn line(&mut self, events: &mut Vec<String>) -> Result<(), SseError> {
    if self.line.last() == Some(&b'\r') {
      self.line.pop();
    }
    if self.line.is_empty() {
      self.dispatch(events);
      return Ok(());
    }
    let line = std::str::from_utf8(&self.line).map_err(|_| SseError::InvalidUtf8)?;
    if let Some(value) = line.strip_prefix("data:") {
      let value = value.strip_prefix(' ').unwrap_or(value);
      if self.has_data {
        self.data.push('\n');
      }
      self.data.push_str(value);
      self.has_data = true;
    }
    self.line.clear();
    Ok(())
  }

  fn dispatch(&mut self, events: &mut Vec<String>) {
    if self.has_data {
      events.push(std::mem::take(&mut self.data));
      self.has_data = false;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fragmented_utf8_crlf_multiline_and_comments() {
    let data = ": heartbeat\r\ndata: 你好\r\ndata: world\r\nid: ignored\r\n\r\n";
    let mut parser = SseDecoder::new(1024);
    let mut events = Vec::new();
    for byte in data.as_bytes() {
      events.extend(parser.push(&[*byte]).unwrap());
    }
    assert_eq!(events, ["你好\nworld"]);
    assert!(parser.finish().unwrap().is_empty());
  }

  #[test]
  fn final_event_and_multiple_events() {
    let mut parser = SseDecoder::new(1024);
    assert_eq!(parser.push(b"data: one\n\ndata: two").unwrap(), ["one"]);
    assert_eq!(parser.finish().unwrap(), ["two"]);
  }

  #[test]
  fn limits_partial_lines_and_accumulated_events() {
    let mut parser = SseDecoder::new(20);
    assert!(parser.push(b"data: abcdefghij\n").is_ok());
    assert_eq!(parser.push(b"data: abcdefghij\n"), Err(SseError::TooLarge));
    let mut parser = SseDecoder::new(4);
    assert_eq!(parser.push(b": huge"), Err(SseError::TooLarge));
  }

  #[test]
  fn rejects_invalid_utf8() {
    assert_eq!(SseDecoder::new(100).push(b"data: \xff\n\n"), Err(SseError::InvalidUtf8));
  }
}
