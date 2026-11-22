use crate::session::{Session, Stream};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{
  MaybeTlsStream, WebSocketStream, connect_async_with_config,
  tungstenite::{Message, protocol::WebSocketConfig},
};
use tokn_hub_client_core::{
  exchange::{RequestFrames, ResponseState},
  secure::MAX_RECORD,
};
use tokn_hub_transport::{BoxTransport, RecordTransport};
use url::Url;

pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) async fn connect(endpoint: &Url) -> Result<Socket, String> {
  let config = WebSocketConfig::default()
    .max_message_size(Some(MAX_RECORD))
    .max_frame_size(Some(MAX_RECORD));
  let (socket, _) = tokio::time::timeout(
    Duration::from_secs(15),
    connect_async_with_config(endpoint.as_str(), Some(config), false),
  )
  .await
  .map_err(|_| "Hub connection timed out")?
  .map_err(|_| "Could not reach this machine through the Hub")?;
  Ok(socket)
}

pub(crate) async fn send(socket: &mut Socket, record: Vec<u8>) -> Result<(), String> {
  tokio::time::timeout(Duration::from_secs(10), socket.send(Message::Binary(record.into())))
    .await
    .map_err(|_| "Encrypted write timed out; delivery may be uncertain")?
    .map_err(|_| "Encrypted connection closed; delivery may be uncertain".into())
}

pub(crate) async fn receive(socket: &mut Socket) -> Result<Vec<u8>, String> {
  // Relay pings never extend the authenticated-data timeout.
  tokio::time::timeout(Duration::from_secs(90), receive_from_relay(socket))
    .await
    .map_err(|_| "Encrypted response timed out; requests are not retried")?
}

async fn receive_from_relay(socket: &mut Socket) -> Result<Vec<u8>, String> {
  loop {
    match socket.next().await {
      Some(Ok(Message::Binary(record))) if record.len() <= MAX_RECORD => return Ok(record.to_vec()),
      Some(Ok(Message::Ping(record))) => {
        tokio::time::timeout(Duration::from_secs(10), socket.send(Message::Pong(record)))
          .await
          .map_err(|_| "Relay pong timed out")?
          .map_err(|_| "Relay disconnected")?
      }
      Some(Ok(Message::Pong(_))) => {}
      _ => return Err("Encrypted connection ended unexpectedly".into()),
    }
  }
}

/// A carrier only transports ordered encrypted records. Noise owns identity,
/// authorization and message framing independently of the physical path.
struct RelayTransport(Socket);

#[async_trait]
impl RecordTransport for RelayTransport {
  async fn send(&mut self, record: Vec<u8>) -> Result<(), String> {
    send(&mut self.0, record).await
  }
  async fn receive(&mut self) -> Result<Option<Vec<u8>>, String> {
    receive_from_relay(&mut self.0).await.map(Some)
  }
  async fn close(&mut self) {
    let _ = self.0.close(None).await;
  }
}

pub(crate) async fn relay_transport(endpoint: &Url) -> Result<BoxTransport, String> {
  Ok(Box::new(RelayTransport(connect(endpoint).await?)))
}

pub(crate) async fn read_record(transport: &mut BoxTransport) -> Result<Vec<u8>, String> {
  tokio::time::timeout(Duration::from_secs(90), transport.receive())
    .await
    .map_err(|_| "Encrypted response timed out; requests are not retried")??
    .filter(|record| record.len() <= MAX_RECORD)
    .ok_or_else(|| "Encrypted connection ended unexpectedly".into())
}

pub struct Exchange {
  stream: Stream,
  pub status: u16,
  pub content_type: Option<String>,
  response: ResponseState,
}

impl Exchange {
  pub(crate) async fn open(session: &Session, method: &str, path: &str, body: &[u8]) -> Result<Self, String> {
    let mut frames = RequestFrames::new(method, path, body)?;
    let mut stream = session
      .open(frames.next().expect("request starts with its header"))
      .await?;
    for frame in frames {
      stream.send(frame).await?;
    }
    let response = ResponseState::from_header(stream.receive().await?, None)?;
    Ok(Self {
      status: response.status,
      content_type: response.content_type.clone(),
      stream,
      response,
    })
  }

  pub async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
    self.response.next(self.stream.receive().await?)
  }

  pub async fn acknowledge(&mut self) -> Result<(), String> {
    self.stream.send(self.response.acknowledge()?).await
  }

  pub async fn json(mut self) -> Result<Value, String> {
    if self
      .content_type
      .as_deref()
      .is_some_and(|value| !value.starts_with("application/json"))
    {
      return Err("Expected a JSON response".into());
    }
    tokio::time::timeout(Duration::from_secs(120), async {
      let mut body = Vec::new();
      while let Some(chunk) = self.next().await? {
        if body.len() + chunk.len() > 128 * 1024 * 1024 {
          return Err("Host response exceeds size limit".into());
        }
        body.extend(chunk);
        self.acknowledge().await?;
      }
      let value = if self.status == 204 {
        Value::Null
      } else {
        serde_json::from_slice::<Value>(&body).map_err(|_| "Host returned invalid JSON")?
      };
      if self.status >= 400 {
        return Err(
          value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Machine request failed")
            .to_owned(),
        );
      }
      Ok(value)
    })
    .await
    .map_err(|_| "Host request timed out; requests are not retried")?
  }
}
