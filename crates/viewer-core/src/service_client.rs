use std::{
  pin::Pin,
  sync::Arc,
  task::{Context, Poll},
  time::Duration,
};

use tokio::net::TcpStream;
use tokn_session_core::{AgentEvent, LoadedSession, Provider, SessionHistoryStatus, SessionRef};

use crate::service_protocol::*;

pub struct RelayCatalog {
  pub entries: Vec<CatalogEntry>,
  pub providers: Vec<Provider>,
  pub native: bool,
  pub warnings: Vec<String>,
}

trait SessionIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> SessionIo for T {}

/// An embedded subscription owns its server handler, including queued initial
/// work. Dropping the client releases its handler and any pending follow lease.
struct EmbeddedIo {
  stream: tokio::io::DuplexStream,
  handler: tokio::task::AbortHandle,
}

impl Drop for EmbeddedIo {
  fn drop(&mut self) {
    self.handler.abort();
  }
}

impl tokio::io::AsyncRead for EmbeddedIo {
  fn poll_read(
    mut self: Pin<&mut Self>,
    context: &mut Context<'_>,
    buffer: &mut tokio::io::ReadBuf<'_>,
  ) -> Poll<std::io::Result<()>> {
    Pin::new(&mut self.stream).poll_read(context, buffer)
  }
}

impl tokio::io::AsyncWrite for EmbeddedIo {
  fn poll_write(mut self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &[u8]) -> Poll<std::io::Result<usize>> {
    Pin::new(&mut self.stream).poll_write(context, buffer)
  }

  fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    Pin::new(&mut self.stream).poll_flush(context)
  }

  fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
    Pin::new(&mut self.stream).poll_shutdown(context)
  }
}

#[derive(Clone)]
pub enum Connection {
  Tcp(String),
  Embedded(Arc<crate::service_server::Service>),
}

async fn connect(connection: &Connection, action: Action) -> Result<(Box<dyn SessionIo>, Vec<Provider>, bool), String> {
  let mut stream: Box<dyn SessionIo> = match connection {
    Connection::Tcp(endpoint) => Box::new(
      tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(local_endpoint(endpoint)?))
        .await
        .map_err(|_| "Relay connection timed out")?
        .map_err(|e| e.to_string())?,
    ),
    Connection::Embedded(service) => {
      let (client, mut server) = tokio::io::duplex(64 * 1024);
      let service = service.clone();
      let handler = tokio::spawn(async move {
        if let Err(message) = service.handle(&mut server).await {
          let _ = write_frame(&mut server, &Frame::Error { message }).await;
        }
      });
      Box::new(EmbeddedIo {
        stream: client,
        handler: handler.abort_handle(),
      })
    }
  };
  write_frame(
    &mut stream,
    &Request {
      version: PROTOCOL_VERSION,
      action,
    },
  )
  .await?;
  match receive(&mut stream).await? {
    Frame::Hello {
      version: PROTOCOL_VERSION,
      native,
      providers,
    } => Ok((stream, providers, native)),
    _ => Err("Unsupported Relay handshake".into()),
  }
}

pub async fn load_catalog(endpoint: &str) -> Result<RelayCatalog, String> {
  load_catalog_from(&Connection::Tcp(endpoint.into())).await
}

pub async fn load_catalog_from(connection: &Connection) -> Result<RelayCatalog, String> {
  let (mut stream, providers, native) = connect(connection, Action::Catalog).await?;
  let mut entries = Vec::new();
  let mut bytes = 0;
  loop {
    match receive(&mut stream).await? {
      Frame::Header { entry } => {
        bytes += serde_json::to_vec(&entry).map_err(|e| e.to_string())?.len();
        if bytes > MAX_SNAPSHOT_BYTES {
          return Err("Relay catalog exceeds memory limit".into());
        }
        entries.push(entry);
      }
      Frame::CatalogEnd { warnings } => {
        return Ok(RelayCatalog {
          entries,
          providers,
          native,
          warnings,
        });
      }
      _ => return Err("Unexpected Relay catalog frame".into()),
    }
  }
}

pub struct RelaySubscription {
  windowed: bool,
  stream: Box<dyn SessionIo>,
  generation: String,
  revision: u64,
  events: Vec<AgentEvent>,
  native: Vec<Option<Arc<serde_json::Value>>>,
  event_offset: usize,
  has_earlier: bool,
  bytes: usize,
}

#[derive(Debug)]
pub struct SessionSnapshot {
  pub generation: String,
  pub revision: String,
  pub event_offset: usize,
  pub has_earlier: bool,
  pub estimated_bytes: usize,
  pub loaded: LoadedSession,
  /// Native record for each normalized event, used by the viewer Inspector.
  pub native: Vec<Option<Arc<serde_json::Value>>>,
}

impl RelaySubscription {
  pub async fn connect(endpoint: &str, session_key: &str) -> Result<Self, String> {
    Self::connect_from(&Connection::Tcp(endpoint.into()), session_key).await
  }

  pub async fn connect_from(connection: &Connection, session_key: &str) -> Result<Self, String> {
    Self::connect_action(
      connection,
      Action::Follow {
        session_key: session_key.into(),
      },
    )
    .await
  }

  pub async fn connect_window_from(
    connection: &Connection,
    session_key: &str,
    retain_from: Option<usize>,
    before_event: Option<usize>,
  ) -> Result<Self, String> {
    Self::connect_action(
      connection,
      Action::FollowWindow {
        session_key: session_key.into(),
        retain_from,
        before_event,
      },
    )
    .await
  }

  async fn connect_action(connection: &Connection, action: Action) -> Result<Self, String> {
    let windowed = matches!(action, Action::FollowWindow { .. });
    let (stream, _, _) = connect(connection, action).await?;
    Ok(Self {
      windowed,
      stream,
      generation: String::new(),
      revision: 0,
      events: Vec::new(),
      native: Vec::new(),
      event_offset: 0,
      has_earlier: false,
      bytes: 0,
    })
  }

  /// No partial snapshot escapes this method. Interrupted transactions leave
  /// the caller's last committed snapshot intact and require reconnection.
  pub async fn next_snapshot(&mut self) -> Result<SessionSnapshot, String> {
    let (generation, revision, reset, header) = loop {
      match receive(&mut self.stream).await? {
        Frame::Begin {
          generation,
          revision,
          reset,
          header,
        } => break (generation, revision, reset, header),
        Frame::Heartbeat => continue,
        _ => return Err("Expected Relay snapshot boundary".into()),
      }
    };
    // Window generations become part of every viewer event identity. Bound
    // this token separately from the frame limit to avoid per-event expansion.
    if generation.is_empty() || generation.len() > 128 {
      return Err("Invalid Relay snapshot generation".into());
    }
    let sequence: u64 = revision.parse().map_err(|_| "Invalid Relay revision")?;
    if !reset && (generation != self.generation || sequence <= self.revision) {
      return Err("Relay cursor mismatch; resnapshot required".into());
    }
    let mut pending_events = Vec::new();
    let mut pending_native = Vec::new();
    let mut event_offset = if reset { 0 } else { self.event_offset };
    let mut has_earlier = !reset && self.has_earlier;
    let mut saw_window = false;
    let mut saw_record = false;
    let mut bytes = if reset { 0 } else { self.bytes };
    loop {
      match receive(&mut self.stream).await? {
        Frame::Window {
          event_offset: offset,
          has_earlier: earlier,
        } => {
          if !self.windowed || saw_window || saw_record {
            return Err("Unexpected Relay history window metadata".into());
          }
          if earlier != (offset > 0) {
            return Err("Invalid Relay history window boundary".into());
          }
          if !reset && offset != self.event_offset {
            return Err("Relay history window changed without reset".into());
          }
          saw_window = true;
          event_offset = offset;
          has_earlier = earlier;
        }
        Frame::Record { record } => {
          if self.windowed && !saw_window {
            return Err("Missing Relay history window metadata".into());
          }
          saw_record = true;
          if record.path != header.path || record.session.session_id != header.id {
            return Err("Relay snapshot contains a record outside its session".into());
          }
          bytes += serde_json::to_vec(&record).map_err(|e| e.to_string())?.len();
          if bytes > MAX_SNAPSHOT_BYTES {
            return Err("Relay snapshot exceeds memory limit".into());
          }
          let redacted = record
            .record
            .events
            .iter()
            .any(|event| matches!(event, AgentEvent::Reasoning(reasoning) if reasoning.redacted == Some(true)));
          let source = if redacted {
            None
          } else {
            record.record.native.map(Arc::new)
          };
          for event in record.record.events {
            pending_events.push(event);
            pending_native.push(source.clone());
          }
        }
        Frame::Commit {
          generation: end_generation,
          revision: end_revision,
        } if end_generation == generation && end_revision == revision => {
          if self.windowed && !saw_window {
            return Err("Missing Relay history window metadata".into());
          }
          break;
        }
        _ => return Err("Invalid Relay snapshot transaction".into()),
      }
    }
    let retained_events = if reset { 0 } else { self.events.len() };
    let event_count = retained_events
      .checked_add(pending_events.len())
      .ok_or("Relay history window size overflow")?;
    event_offset
      .checked_add(event_count)
      .ok_or("Relay history window position overflow")?;
    if reset {
      self.events.clear();
      self.native.clear();
    }
    self.events.extend(pending_events);
    self.native.extend(pending_native);
    self.event_offset = event_offset;
    self.has_earlier = has_earlier;
    self.bytes = bytes;
    self.generation = generation.clone();
    self.revision = sequence;
    let events = self.events.clone();
    let native = self.native.clone();
    let message_count = events
      .iter()
      .filter(|e| matches!(e, tokn_session_core::AgentEvent::Message(_)))
      .count();
    Ok(SessionSnapshot {
      generation,
      revision,
      event_offset,
      has_earlier,
      estimated_bytes: bytes,
      native,
      loaded: LoadedSession {
        reference: SessionRef {
          id: header.id,
          parent_session_id: header.parent_session_id,
          agent_path: header.agent_path,
          agent_nickname: header.agent_nickname,
          agent_role: header.agent_role,
          title: header.title,
          preview: header.preview,
          path: header.path,
          cwd: header.cwd,
          timestamp: header.timestamp,
          message_count,
        },
        events,
        history_status: SessionHistoryStatus::Complete,
      },
    })
  }
}

async fn receive(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Frame, String> {
  let frame = tokio::time::timeout(Duration::from_secs(10), read_frame(stream))
    .await
    .map_err(|_| "Relay response timed out")??;
  if let Frame::Error { message } = frame {
    Err(message)
  } else {
    Ok(frame)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::RelayRecord;
  use serde_json::json;

  fn fixture_header() -> tokn_session_core::SessionHeader {
    serde_json::from_value(json!({ "id": "fixture", "path": "/tmp/fixture.jsonl" })).unwrap()
  }

  fn fixture_record() -> Frame {
    Frame::Record {
      record: Box::new(
        serde_json::from_value(json!({
          "path": "/tmp/fixture.jsonl", "topic": "pi.fixture", "operation": "upsert", "record_id": "jsonl:0",
          "session": { "provider": "pi", "session_id": "fixture" },
          "events": [{ "type": "session_started", "provider": "pi", "session_id": "fixture" }]
        }))
        .unwrap(),
      ),
    }
  }

  fn scripted_subscription(windowed: bool, frames: Vec<Frame>) -> RelaySubscription {
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
      for frame in frames {
        if write_frame(&mut server, &frame).await.is_err() {
          break;
        }
      }
    });
    RelaySubscription {
      windowed,
      stream: Box::new(client),
      generation: String::new(),
      revision: 0,
      events: Vec::new(),
      native: Vec::new(),
      event_offset: 0,
      has_earlier: false,
      bytes: 0,
    }
  }

  #[tokio::test]
  async fn malformed_window_transactions_preserve_the_last_committed_snapshot() {
    let begin = |reset| Frame::Begin {
      generation: if reset { "replacement" } else { "first" }.into(),
      revision: "2".into(),
      reset,
      header: fixture_header(),
    };
    let commit = |reset| Frame::Commit {
      generation: if reset { "replacement" } else { "first" }.into(),
      revision: "2".into(),
    };
    let window = |event_offset, has_earlier| Frame::Window {
      event_offset,
      has_earlier,
    };
    let cases = [
      (
        "reset missing window",
        vec![begin(true), fixture_record(), commit(true)],
      ),
      ("empty reset missing window", vec![begin(true), commit(true)]),
      (
        "append missing window",
        vec![begin(false), fixture_record(), commit(false)],
      ),
      (
        "duplicate window",
        vec![begin(true), window(0, false), window(0, false), commit(true)],
      ),
      (
        "window after record",
        vec![
          begin(true),
          window(0, false),
          fixture_record(),
          window(0, false),
          commit(true),
        ],
      ),
      ("earlier at zero", vec![begin(true), window(0, true), commit(true)]),
      (
        "no earlier above zero",
        vec![begin(true), window(4, false), commit(true)],
      ),
      (
        "offset overflow",
        vec![begin(true), window(usize::MAX, true), fixture_record(), commit(true)],
      ),
      (
        "shift without reset",
        vec![begin(false), window(3, true), fixture_record(), commit(false)],
      ),
      (
        "incomplete reset",
        vec![begin(true), window(0, false), fixture_record()],
      ),
    ];
    for (name, invalid) in cases {
      let mut frames = vec![
        Frame::Begin {
          generation: "first".into(),
          revision: "1".into(),
          reset: true,
          header: fixture_header(),
        },
        window(5, true),
        fixture_record(),
        Frame::Commit {
          generation: "first".into(),
          revision: "1".into(),
        },
      ];
      frames.extend(invalid);
      let mut subscription = scripted_subscription(true, frames);
      let committed = subscription.next_snapshot().await.unwrap();
      let previous_events = serde_json::to_value(&subscription.events).unwrap();
      assert!(subscription.next_snapshot().await.is_err(), "{name} must be rejected");
      assert_eq!(subscription.generation, "first", "{name}");
      assert_eq!(subscription.revision, 1, "{name}");
      assert_eq!(subscription.event_offset, 5, "{name}");
      assert!(subscription.has_earlier, "{name}");
      assert_eq!(subscription.bytes, committed.estimated_bytes, "{name}");
      assert_eq!(
        serde_json::to_value(&subscription.events).unwrap(),
        previous_events,
        "{name}"
      );
    }
  }

  #[tokio::test]
  async fn generation_tokens_are_nonempty_and_bounded_before_event_keys_expand_them() {
    for generation in [String::new(), "x".repeat(129), "界".repeat(43)] {
      let mut subscription = scripted_subscription(
        true,
        vec![Frame::Begin {
          generation,
          revision: "1".into(),
          reset: true,
          header: fixture_header(),
        }],
      );
      assert!(
        subscription
          .next_snapshot()
          .await
          .unwrap_err()
          .contains("Invalid Relay snapshot generation")
      );
      assert!(subscription.events.is_empty());
      assert_eq!(subscription.revision, 0);
    }
    let generation = "x".repeat(128);
    let mut subscription = scripted_subscription(
      true,
      vec![
        Frame::Begin {
          generation: generation.clone(),
          revision: "1".into(),
          reset: true,
          header: fixture_header(),
        },
        Frame::Window {
          event_offset: 0,
          has_earlier: false,
        },
        fixture_record(),
        Frame::Commit {
          generation: generation.clone(),
          revision: "1".into(),
        },
      ],
    );
    assert_eq!(subscription.next_snapshot().await.unwrap().generation, generation);
  }

  #[tokio::test]
  async fn legacy_follow_rejects_window_metadata_and_window_appends_check_absolute_end() {
    let mut legacy = scripted_subscription(
      false,
      vec![
        Frame::Begin {
          generation: "first".into(),
          revision: "1".into(),
          reset: true,
          header: fixture_header(),
        },
        Frame::Window {
          event_offset: 0,
          has_earlier: false,
        },
        Frame::Commit {
          generation: "first".into(),
          revision: "1".into(),
        },
      ],
    );
    assert!(
      legacy
        .next_snapshot()
        .await
        .unwrap_err()
        .contains("Unexpected Relay history window")
    );

    let mut subscription = scripted_subscription(
      true,
      vec![
        Frame::Begin {
          generation: "first".into(),
          revision: "1".into(),
          reset: true,
          header: fixture_header(),
        },
        Frame::Window {
          event_offset: usize::MAX - 1,
          has_earlier: true,
        },
        fixture_record(),
        Frame::Commit {
          generation: "first".into(),
          revision: "1".into(),
        },
        Frame::Begin {
          generation: "first".into(),
          revision: "2".into(),
          reset: false,
          header: fixture_header(),
        },
        Frame::Window {
          event_offset: usize::MAX - 1,
          has_earlier: true,
        },
        fixture_record(),
        Frame::Commit {
          generation: "first".into(),
          revision: "2".into(),
        },
      ],
    );
    subscription.next_snapshot().await.unwrap();
    assert!(
      subscription
        .next_snapshot()
        .await
        .unwrap_err()
        .contains("position overflow")
    );
    assert_eq!(subscription.events.len(), 1);
    assert_eq!(subscription.revision, 1);
  }

  #[tokio::test]
  async fn window_follow_opens_one_turn_expands_preserves_appends_and_resets() {
    use std::io::Write;
    use tokn_session_relay::{ProviderRoot, RelayConfig};
    let root = tempfile::TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    let header = "{\"type\":\"session\",\"id\":\"fixture\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n";
    let message = |id: usize| {
      format!(
        "{}\n",
        json!({"type":"message", "id":format!("user-{id}"),
      "message":{"role":"user", "content":format!("prompt {id}")}})
      )
    };
    std::fs::write(&path, format!("{header}{}", (0..8).map(message).collect::<String>())).unwrap();
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, root.path().into())]);
    config.include_native = true;
    config.poll_interval = Duration::from_millis(10);
    let service = crate::service_server::Service::new(config).unwrap();
    let connection = Connection::Embedded(service.clone());
    let key = load_catalog_from(&connection).await.unwrap().entries.remove(0).key;
    let mut client = RelaySubscription::connect_window_from(&connection, &key, None, None)
      .await
      .unwrap();
    let first = client.next_snapshot().await.unwrap();
    assert_eq!(first.event_offset, 8);
    assert!(first.has_earlier);
    assert_eq!(first.loaded.events.len(), 1);
    assert!(first.native.iter().all(Option::is_some));
    let mut earlier =
      RelaySubscription::connect_window_from(&connection, &key, Some(first.event_offset), Some(first.event_offset))
        .await
        .unwrap();
    let expanded = earlier.next_snapshot().await.unwrap();
    assert_eq!(expanded.event_offset, 5);
    assert_eq!(expanded.loaded.events.len(), 4);
    assert_eq!(expanded.generation, first.generation);
    assert_eq!(
      serde_json::to_value(&expanded.loaded.events[3..]).unwrap(),
      serde_json::to_value(&first.loaded.events).unwrap()
    );
    drop(client);
    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(message(8).as_bytes())
      .unwrap();
    service.invalidate().await;
    let appended = tokio::time::timeout(Duration::from_secs(3), async {
      loop {
        let next = earlier.next_snapshot().await.unwrap();
        if next.loaded.events.len() == 5 {
          break next;
        }
      }
    })
    .await
    .unwrap();
    assert_eq!(appended.event_offset, expanded.event_offset);
    assert_eq!(appended.generation, expanded.generation);
    let mut reconnect = RelaySubscription::connect_window_from(&connection, &key, Some(appended.event_offset), None)
      .await
      .unwrap();
    let retained = reconnect.next_snapshot().await.unwrap();
    assert_eq!(retained.loaded.events.len(), 5);
    assert_eq!(retained.event_offset, 5);
    let replacement = root.path().join("replacement.jsonl");
    std::fs::write(
      &replacement,
      format!("{header}{}", (100..110).map(message).collect::<String>()),
    )
    .unwrap();
    std::fs::rename(replacement, &path).unwrap();
    service.invalidate().await;
    let rewritten = tokio::time::timeout(Duration::from_secs(3), async {
      loop {
        let next = reconnect.next_snapshot().await.unwrap();
        if next.generation != retained.generation {
          break next;
        }
      }
    })
    .await
    .unwrap();
    assert_eq!(rewritten.event_offset, 6);
    assert_eq!(
      rewritten.loaded.events.len(),
      5,
      "rewrite retains expanded/live-grown turn count"
    );
    // A rewrite shorter than the retained range chooses a useful fresh window.
    std::fs::write(&path, format!("{header}{}", message(99))).unwrap();
    service.invalidate().await;
    let reset = tokio::time::timeout(Duration::from_secs(3), async {
      loop {
        let next = reconnect.next_snapshot().await.unwrap();
        if next.generation != rewritten.generation {
          break next;
        }
      }
    })
    .await
    .unwrap();
    assert_eq!(reset.event_offset, 0);
    assert!(!reset.has_earlier);
    assert_eq!(reset.loaded.events.len(), 2);
    assert!(
      reset
        .loaded
        .events
        .iter()
        .any(|event| matches!(event, AgentEvent::Message(message) if message.text == "prompt 99"))
    );
  }

  #[tokio::test]
  async fn redacted_sibling_native_is_withheld_and_incomplete_reset_does_not_commit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let header: tokn_session_core::SessionHeader =
      serde_json::from_value(json!({ "id": "fixture", "path": "/tmp/fixture.jsonl" })).unwrap();
    let record: RelayRecord = serde_json::from_value(json!({
      "path": "/tmp/fixture.jsonl", "topic": "pi.fixture", "operation": "upsert", "record_id": "jsonl:0",
      "session": { "provider": "pi", "session_id": "fixture" }, "native": { "private": "redacted source" },
      "events": [
        { "type": "reasoning", "provider": "pi", "phase": "finished", "redacted": true },
        { "type": "session_started", "provider": "pi", "session_id": "fixture" }
      ]
    }))
    .unwrap();
    let server = tokio::spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let _: Request = read_frame(&mut stream).await.unwrap();
      for frame in [
        Frame::Hello {
          version: PROTOCOL_VERSION,
          providers: vec![Provider::Pi],
          native: true,
        },
        Frame::Begin {
          generation: "first".into(),
          revision: "1".into(),
          reset: true,
          header: header.clone(),
        },
        Frame::Record {
          record: Box::new(record.clone()),
        },
        Frame::Commit {
          generation: "first".into(),
          revision: "1".into(),
        },
        Frame::Begin {
          generation: "replacement".into(),
          revision: "2".into(),
          reset: true,
          header,
        },
        Frame::Record {
          record: Box::new(record),
        },
      ] {
        write_frame(&mut stream, &frame).await.unwrap();
      }
      // Drop the stream mid-transaction, before commit.
    });
    let mut subscription = RelaySubscription::connect(&endpoint, "fixture").await.unwrap();
    let first = subscription.next_snapshot().await.unwrap();
    assert_eq!(first.loaded.events.len(), 2);
    assert!(first.native.iter().all(Option::is_none));
    let bytes = subscription.bytes;
    assert!(subscription.next_snapshot().await.is_err());
    assert_eq!(subscription.generation, "first");
    assert_eq!(subscription.revision, 1);
    assert_eq!(subscription.events.len(), 2);
    assert_eq!(subscription.bytes, bytes);
    server.await.unwrap();
  }
}
