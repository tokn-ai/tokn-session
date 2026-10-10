use crate::{Connection, exchange::Exchange};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::oneshot;

const MAX_EVENT: usize = 2 * 1024 * 1024;
const MAX_SESSION_UPDATE: usize = 64 * 1024 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) async fn pump(connection: Arc<Connection>, ready: oneshot::Sender<Result<(), String>>) {
  pump_with_startup_timeout(connection, ready, STARTUP_TIMEOUT).await;
}

async fn pump_with_startup_timeout(
  connection: Arc<Connection>,
  ready: oneshot::Sender<Result<(), String>>,
  startup_timeout: Duration,
) {
  let mut ready = Some(ready);
  let mut connected = false;
  let mut attempted = false;
  let deadline = tokio::time::Instant::now() + startup_timeout;
  let mut failure = "Machine disconnected".to_owned();
  while !connection.cancellation.is_cancelled() {
    connection.state(if attempted { "reconnecting" } else { "connecting" });
    attempted = true;
    let result = tokio::select! {
      biased;
      _ = connection.cancellation.cancelled() => {failure = "Machine disconnected".into();break;},
      result = async {
        let opening = Exchange::open(&connection.endpoint,&connection.identity,&connection.info.host_public_key,"GET","/api/v1/events",&[]);
        let mut exchange = if connected {opening.await?} else {
          tokio::time::timeout_at(deadline, opening).await.map_err(|_|"Live connection timed out")??
        };
        if exchange.status != 200 || !exchange.content_type.as_deref().is_some_and(|value|value.starts_with("text/event-stream")) {
          return Err("Host refused live event stream".into());
        }
        let mut parser = EventParser::default();
        loop {
          let chunk = if connected {exchange.next().await?} else {
            tokio::time::timeout_at(deadline, exchange.next()).await.map_err(|_|"Live connection timed out")??
          };
          let Some(chunk) = chunk else {break;};
          for (event,payload) in parser.feed(&chunk)? {
            if event == "ready" {
              connection.state("connected");
              if connected {
                connection.event("transport-reconnected",json!({}));
                connection.event("relay-changed",json!({"session_key":null,"reset":true}));
              }
              connected = true;
              if let Some(ready) = ready.take() {let _ = ready.send(Ok(()));}
            } else {connection.event(&event,payload);}
          }
          if connected {exchange.acknowledge().await?;} else {
            tokio::time::timeout_at(deadline, exchange.acknowledge()).await.map_err(|_|"Live connection timed out")??;
          }
        }
        Err::<(),String>("Host closed live event stream".into())
      } => result,
    };
    // The stream is read-only, including before its first ready frame. Retry
    // transient startup failures within the same bounded readiness deadline.
    if let Err(error) = result {
      failure = error;
    }
    tokio::select! {
      biased;
      _ = connection.cancellation.cancelled()=>{failure = "Machine disconnected".into();break;},
      _ = tokio::time::sleep_until(deadline), if !connected=>{failure = "Live connection timed out".into();break;},
      _ = tokio::time::sleep(Duration::from_secs(1))=>{},
    }
  }
  connection.listening.store(false, std::sync::atomic::Ordering::Release);
  if let Some(ready) = ready {
    let _ = ready.send(Err(failure));
  }
}

#[derive(Default)]
struct EventParser {
  buffer: Vec<u8>,
}

impl EventParser {
  fn feed(&mut self, bytes: &[u8]) -> Result<Vec<(String, Value)>, String> {
    // Viewer API emits LF delimiters; remove CR to tolerate CRLF on the wire.
    self.buffer.extend(bytes.iter().copied().filter(|byte| *byte != b'\r'));
    let mut events = Vec::new();
    while let Some(end) = self.buffer.windows(2).position(|pair| pair == b"\n\n") {
      self.check_limit(end)?;
      let frame = std::str::from_utf8(&self.buffer[..end]).map_err(|_| "Invalid UTF-8 event stream")?;
      let mut event = "message";
      let mut data = Vec::new();
      for line in frame.lines() {
        if let Some(value) = line.strip_prefix("event:") {
          event = value.trim();
        }
        if let Some(value) = line.strip_prefix("data:") {
          data.push(value.trim_start());
        }
      }
      if !data.is_empty() {
        let payload = serde_json::from_str(&data.join("\n")).map_err(|_| "Host sent malformed live event JSON")?;
        events.push((event.to_owned(), payload));
      }
      self.buffer.drain(..end + 2);
    }
    self.check_limit(self.buffer.len())?;
    Ok(events)
  }

  fn check_limit(&self, length: usize) -> Result<(), String> {
    let max =
      if self.buffer.starts_with(b"event: session-updated\n") || self.buffer.starts_with(b"event:session-updated\n") {
        MAX_SESSION_UPDATE
      } else {
        MAX_EVENT
      };
    if length > max {
      return Err("Host event exceeds size limit".into());
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ConnectionInfo;
  use std::sync::atomic::{AtomicBool, Ordering};
  use tokio::sync::Semaphore;
  use tokio_util::sync::CancellationToken;
  use tokn_hub_client_core::secure::NoiseIdentity;
  use url::Url;

  fn connection(endpoint: Url) -> Arc<Connection> {
    let identity = Arc::new(NoiseIdentity::generate().unwrap());
    Arc::new(Connection {
      info: ConnectionInfo {
        connection_id: "test-connection".into(),
        endpoint: endpoint.to_string(),
        host_id: uuid::Uuid::new_v4().to_string(),
        host_public_key: identity.public_key(),
      },
      endpoint,
      identity,
      cancellation: CancellationToken::new(),
      requests: Arc::new(Semaphore::new(1)),
      listening: AtomicBool::new(true),
      sink: Arc::new(|_, _| {}),
    })
  }

  #[tokio::test]
  async fn initial_stream_deadline_closes_hanging_attempt_and_resets_listener_state() {
    // Keep the TCP listener open without upgrading the socket: even a relay
    // that accepts TCP but never completes WebSocket cannot extend readiness.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let connection = connection(Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap());
    let (ready, result) = oneshot::channel();
    pump_with_startup_timeout(connection.clone(), ready, Duration::from_millis(20)).await;
    assert!(result.await.unwrap().unwrap_err().contains("timed out"));
    assert!(!connection.listening.load(Ordering::Acquire));
  }

  #[tokio::test]
  async fn closing_machine_cancels_initial_stream_wait_without_waiting_for_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let connection = connection(Url::parse(&format!("ws://{}", listener.local_addr().unwrap())).unwrap());
    let (ready, result) = oneshot::channel();
    let pump = tokio::spawn(pump_with_startup_timeout(
      connection.clone(),
      ready,
      Duration::from_secs(30),
    ));
    tokio::task::yield_now().await;
    connection.cancellation.cancel();
    assert_eq!(
      tokio::time::timeout(Duration::from_secs(1), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err(),
      "Machine disconnected"
    );
    pump.await.unwrap();
    assert!(!connection.listening.load(Ordering::Acquire));
  }

  #[test]
  fn event_parser_preserves_split_unicode_and_handles_multiple_frames() {
    let mut parser = EventParser::default();
    let bytes = "event: ready\ndata: {\"text\":\"你好\"}\n\nevent: relay-changed\ndata: {}\n\n".as_bytes();
    let split = bytes.iter().position(|byte| *byte == 0xe4).unwrap() + 1;
    assert!(parser.feed(&bytes[..split]).unwrap().is_empty());
    let events = parser.feed(&bytes[split..]).unwrap();
    assert_eq!(events[0], ("ready".into(), json!({"text":"你好"})));
    assert_eq!(events[1], ("relay-changed".into(), json!({})));
  }

  #[test]
  fn event_parser_rejects_oversize_and_invalid_json() {
    assert!(EventParser::default().feed(&vec![b'x'; MAX_EVENT + 1]).is_err());
    assert!(EventParser::default().feed(b"event:ready\ndata:not-json\n\n").is_err());
  }
}
