//! Transport-independent encrypted HTTP framing shared by native and guest clients.
use crate::{
  protocol,
  secure::{InnerMessage, MAX_CHUNK, MAX_REQUEST_BODY},
};

#[derive(PartialEq, Eq)]
enum Expected {
  Header,
  Body,
  Discovery,
  DirectConfig,
  DirectAnswer,
  Ended,
}

/// Validate response order and credits as records arrive, including while a
/// consumer is paused. Each logical stream has its own instance.
pub struct StreamResponse {
  expected: Expected,
  unacknowledged: usize,
}

impl StreamResponse {
  pub fn new(first: &InnerMessage) -> Result<Self, String> {
    let expected = match first {
      InnerMessage::DeviceRequest { .. } => Expected::Header,
      InnerMessage::DiscoveryRequest {} => Expected::Discovery,
      InnerMessage::DirectConfigRequest {} => Expected::DirectConfig,
      InnerMessage::DirectOffer { .. } => Expected::DirectAnswer,
      _ => return Err("Invalid encrypted stream request".into()),
    };
    Ok(Self {
      expected,
      unacknowledged: 0,
    })
  }

  /// Returns true when the stream has received its terminal record.
  pub fn accept(&mut self, frame: &InnerMessage) -> Result<bool, String> {
    self.expected = match (&self.expected, frame) {
      (Expected::Ended, _) => return Err("Encrypted stream already ended".into()),
      (_, InnerMessage::Error { .. })
      | (Expected::Discovery, InnerMessage::DiscoveryResponse { .. })
      | (Expected::DirectConfig, InnerMessage::DirectConfig { .. })
      | (Expected::DirectAnswer, InnerMessage::DirectAnswer { .. })
      | (Expected::Body, InnerMessage::End {}) => Expected::Ended,
      (Expected::Header, InnerMessage::Response { .. }) => Expected::Body,
      (Expected::Body, InnerMessage::Chunk { .. }) => {
        if self.unacknowledged >= protocol::RESPONSE_WINDOW {
          return Err("Machine exceeded its encrypted stream credit".into());
        }
        self.unacknowledged += 1;
        Expected::Body
      }
      _ => return Err("Unexpected encrypted stream response".into()),
    };
    Ok(self.expected == Expected::Ended)
  }

  pub fn acknowledge(&mut self, credits: usize) -> Result<(), String> {
    self.unacknowledged = self
      .unacknowledged
      .checked_sub(credits)
      .ok_or("Invalid encrypted stream credit")?;
    Ok(())
  }
}

/// Borrow a bounded request body and emit one protocol message at a time. The
/// caller encrypts/writes each message without allocating a second body copy.
pub struct RequestFrames<'a> {
  method: String,
  path: String,
  body: &'a [u8],
  offset: usize,
  started: bool,
  ended: bool,
}

impl<'a> RequestFrames<'a> {
  pub fn new(method: &str, path: &str, body: &'a [u8]) -> Result<Self, String> {
    InnerMessage::DeviceRequest {
      method: method.into(),
      path: path.into(),
    }
    .validate()?;
    if body.len() > MAX_REQUEST_BODY {
      return Err("Host request exceeds size limit".into());
    }
    Ok(Self {
      method: method.into(),
      path: path.into(),
      body,
      offset: 0,
      started: false,
      ended: false,
    })
  }
}

impl Iterator for RequestFrames<'_> {
  type Item = InnerMessage;
  fn next(&mut self) -> Option<Self::Item> {
    if !self.started {
      self.started = true;
      return Some(InnerMessage::DeviceRequest {
        method: self.method.clone(),
        path: self.path.clone(),
      });
    }
    if self.offset < self.body.len() {
      let end = self.body.len().min(self.offset + MAX_CHUNK);
      let data = protocol::encode(&self.body[self.offset..end]);
      self.offset = end;
      return Some(InnerMessage::RequestBody { data });
    }
    if self.ended {
      return None;
    }
    self.ended = true;
    Some(InnerMessage::RequestEnd {})
  }
}

/// A response permits only its declared header, bounded chunks, and one end.
/// Optional aggregate limits suit JSON; live event streams stay bounded by
/// per-record/parser limits rather than the duration of the connection.
pub struct ResponseState {
  pub status: u16,
  pub content_type: Option<String>,
  maximum: Option<usize>,
  received: usize,
  unacknowledged: usize,
  ended: bool,
}

impl ResponseState {
  pub fn from_header(message: InnerMessage, max_body_bytes: Option<usize>) -> Result<Self, String> {
    message.validate()?;
    let InnerMessage::Response { status, content_type } = message else {
      return Err(match message {
        InnerMessage::Error { message } => message,
        _ => "Expected authenticated host response".into(),
      });
    };
    if !(200..=599).contains(&status) {
      return Err("Expected authenticated host response".into());
    }
    if !matches!(
      content_type
        .as_deref()
        .map(|value| value.split(';').next().unwrap_or_default().trim()),
      Some("application/json" | "text/event-stream")
    ) && !(status == 204 && content_type.is_none())
    {
      return Err("Host returned an unsupported response type".into());
    }
    Ok(Self {
      status,
      content_type,
      maximum: max_body_bytes,
      received: 0,
      unacknowledged: 0,
      ended: false,
    })
  }

  pub fn next(&mut self, message: InnerMessage) -> Result<Option<Vec<u8>>, String> {
    if self.ended {
      return Err("Encrypted response already ended".into());
    }
    message.validate()?;
    match message {
      InnerMessage::Chunk { data } => {
        if self.unacknowledged >= protocol::RESPONSE_WINDOW {
          return Err("Host exceeded the encrypted response window".into());
        }
        let bytes = protocol::decode(&data, MAX_CHUNK)?;
        let total = self
          .received
          .checked_add(bytes.len())
          .ok_or("Host response exceeds size limit")?;
        if self.maximum.is_some_and(|maximum| total > maximum) {
          return Err("Host response exceeds size limit".into());
        }
        self.received = total;
        self.unacknowledged += 1;
        Ok(Some(bytes))
      }
      InnerMessage::End {} => {
        self.ended = true;
        Ok(None)
      }
      InnerMessage::Error { message } => {
        self.ended = true;
        Err(message)
      }
      _ => Err("Unexpected encrypted response record".into()),
    }
  }

  /// Issue credit only after consuming a chunk. There is no automatic retry or
  /// input replay at this framing boundary.
  pub fn acknowledge(&mut self) -> Result<InnerMessage, String> {
    if self.unacknowledged == 0 || self.ended {
      return Err("No encrypted response chunk awaits acknowledgement".into());
    }
    self.unacknowledged -= 1;
    Ok(InnerMessage::Window { credits: 1 })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  fn response(maximum: Option<usize>) -> ResponseState {
    ResponseState::from_header(
      InnerMessage::Response {
        status: 200,
        content_type: Some("application/json".into()),
      },
      maximum,
    )
    .unwrap()
  }
  fn chunk() -> InnerMessage {
    InnerMessage::Chunk {
      data: protocol::encode(b"data"),
    }
  }

  #[test]
  fn request_chunks_preserve_body_and_end_once() {
    let body = vec![7; MAX_CHUNK + 1];
    let frames = RequestFrames::new("POST", "/api/v1/list_sessions", &body)
      .unwrap()
      .collect::<Vec<_>>();
    assert_eq!(frames.len(), 4);
    assert!(matches!(&frames[0], InnerMessage::DeviceRequest {method,..} if method == "POST"));
    let restored = frames
      .iter()
      .filter_map(|frame| match frame {
        InnerMessage::RequestBody { data } => Some(protocol::decode(data, MAX_CHUNK).unwrap()),
        _ => None,
      })
      .flatten()
      .collect::<Vec<_>>();
    assert_eq!(restored, body);
    assert!(matches!(frames.last(), Some(InnerMessage::RequestEnd {})));
    assert!(RequestFrames::new("DELETE", "/api/v1/list_sessions", &[]).is_err());
    assert!(RequestFrames::new("POST", "/api/v1/list_sessions", &vec![0; MAX_REQUEST_BODY + 1]).is_err());
  }

  #[test]
  fn response_bounds_credits_and_terminal_order() {
    let mut bounded = response(Some(4));
    assert_eq!(bounded.next(chunk()).unwrap().unwrap(), b"data");
    assert!(bounded.next(chunk()).is_err());
    assert!(matches!(
      bounded.acknowledge().unwrap(),
      InnerMessage::Window { credits: 1 }
    ));
    assert!(bounded.acknowledge().is_err());
    assert!(bounded.next(InnerMessage::End {}).unwrap().is_none());
    assert!(bounded.next(chunk()).is_err());
    let mut window = response(None);
    for _ in 0..protocol::RESPONSE_WINDOW {
      window.next(chunk()).unwrap();
    }
    assert!(window.next(chunk()).is_err());
    window.acknowledge().unwrap();
    window.next(chunk()).unwrap();
  }

  #[test]
  fn concurrent_streams_have_independent_response_types_and_credit() {
    let open = InnerMessage::DeviceRequest {
      method: "GET".into(),
      path: "/api/v1/health".into(),
    };
    let mut http = StreamResponse::new(&open).unwrap();
    let mut discovery = StreamResponse::new(&InnerMessage::DiscoveryRequest {}).unwrap();
    let header = InnerMessage::Response {
      status: 200,
      content_type: Some("application/json".into()),
    };
    assert!(http.accept(&chunk()).is_err());
    assert!(!http.accept(&header).unwrap());
    for _ in 0..protocol::RESPONSE_WINDOW {
      assert!(!http.accept(&chunk()).unwrap());
    }
    assert!(http.accept(&chunk()).is_err());
    assert!(discovery.accept(&header).is_err());
    assert!(
      discovery
        .accept(&InnerMessage::DiscoveryResponse { group: None })
        .unwrap()
    );
    assert!(discovery.accept(&InnerMessage::End {}).is_err());
    http.acknowledge(1).unwrap();
    assert!(!http.accept(&chunk()).unwrap());
    assert!(http.accept(&InnerMessage::End {}).unwrap());
    assert!(open.clone().on_stream(0).validate().is_err());
    assert!(open.on_stream(1).on_stream(2).validate().is_err());
  }
}
