//! Bounded UTF-8 SSE parsing shared across native and applet clients.
use serde_json::Value;

pub const MAX_EVENT: usize = 2 * 1024 * 1024;
pub const MAX_SESSION_UPDATE: usize = 64 * 1024 * 1024;

pub struct EventParser {
  buffer: Vec<u8>,
  max_event: usize,
  max_session_update: usize,
}

impl Default for EventParser {
  fn default() -> Self {
    Self {
      buffer: Vec::new(),
      max_event: MAX_EVENT,
      max_session_update: MAX_SESSION_UPDATE,
    }
  }
}

impl EventParser {
  pub fn with_limits(max_event: usize, max_session_update: usize) -> Result<Self, String> {
    if max_event == 0
      || max_session_update == 0
      || max_event > MAX_SESSION_UPDATE
      || max_session_update > MAX_SESSION_UPDATE
    {
      return Err("Invalid host event limits".into());
    }
    Ok(Self {
      buffer: Vec::new(),
      max_event,
      max_session_update,
    })
  }
  pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<(String, Value)>, String> {
    // Input records are at most MAX_CHUNK. Check growing partial frames before
    // extending the buffer so a hostile caller cannot force an unbounded copy.
    let mut events = Vec::new();
    for byte in bytes.iter().copied().filter(|byte| *byte != b'\r') {
      self.buffer.push(byte);
      if self.buffer.ends_with(b"\n\n") {
        self.check_limit(self.buffer.len() - 2)?;
        let frame =
          std::str::from_utf8(&self.buffer[..self.buffer.len() - 2]).map_err(|_| "Invalid UTF-8 event stream")?;
        let mut name = "message".to_owned();
        let mut data = String::new();
        let mut has_data = false;
        for line in frame.lines() {
          if let Some(value) = line.strip_prefix("event:") {
            name = value.trim().to_owned();
          }
          if let Some(value) = line.strip_prefix("data:") {
            if has_data {
              data.push('\n');
            }
            has_data = true;
            data.push_str(value.trim_start());
          }
        }
        if has_data {
          events.push((
            name,
            serde_json::from_str(&data).map_err(|_| "Host sent malformed live event JSON")?,
          ));
        }
        self.buffer.clear();
      } else {
        self.check_limit(self.buffer.len().saturating_sub(usize::from(byte == b'\n')))?;
      }
    }
    Ok(events)
  }
  fn check_limit(&self, length: usize) -> Result<(), String> {
    let maximum =
      if self.buffer.starts_with(b"event: session-updated\n") || self.buffer.starts_with(b"event:session-updated\n") {
        self.max_session_update
      } else {
        self.max_event
      };
    if length > maximum {
      Err("Host event exceeds size limit".into())
    } else {
      Ok(())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  #[test]
  fn split_unicode_crlf_and_multi_frame_stream_are_lossless() {
    let mut parser = EventParser::default();
    let bytes = "event: ready\r\ndata: {\"text\":\"你好\"}\r\n\r\nevent:relay-changed\ndata: {}\n\n".as_bytes();
    let split = bytes.iter().position(|byte| *byte == 0xe4).unwrap() + 1;
    assert!(parser.feed(&bytes[..split]).unwrap().is_empty());
    assert_eq!(
      parser.feed(&bytes[split..]).unwrap(),
      vec![
        ("ready".into(), json!({"text":"你好"})),
        ("relay-changed".into(), json!({}))
      ]
    );
  }
  #[test]
  fn guest_limits_apply_to_partial_frames_and_invalid_json() {
    assert!(EventParser::with_limits(32, 64).unwrap().feed(&[b'x'; 33]).is_err());
    assert!(EventParser::default().feed(b"event:ready\ndata:not-json\n\n").is_err());
    assert!(EventParser::with_limits(0, 64).is_err());
  }
}
