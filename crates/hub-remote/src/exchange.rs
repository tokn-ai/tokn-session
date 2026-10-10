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
  protocol,
  secure::{InnerMessage, MAX_CHUNK, MAX_RECORD, NoiseIdentity, NoiseInitiator, SecureChannel},
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
  tokio::time::timeout(Duration::from_secs(90), async {
    loop {
      match socket.next().await {
        Some(Ok(Message::Binary(record))) if record.len() <= MAX_RECORD => return Ok(record.to_vec()),
        Some(Ok(Message::Ping(record))) => socket
          .send(Message::Pong(record))
          .await
          .map_err(|_| "Relay disconnected")?,
        Some(Ok(Message::Pong(_))) => {}
        _ => return Err("Encrypted connection ended unexpectedly".into()),
      }
    }
  })
  .await
  .map_err(|_| "Encrypted response timed out; requests are not retried")?
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
    receive(&mut self.0).await.map(Some)
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

pub(crate) struct Exchange {
  pub transport: BoxTransport,
  pub channel: SecureChannel,
  pub status: u16,
  pub content_type: Option<String>,
}

impl Exchange {
  pub async fn open(
    mut transport: BoxTransport,
    identity: &NoiseIdentity,
    host_key: &str,
    method: &str,
    path: &str,
    body: &[u8],
  ) -> Result<Self, String> {
    let mut initiator = NoiseInitiator::new(identity, host_key)?;
    transport.send(initiator.start()?).await?;
    let mut channel = initiator.finish(&read_record(&mut transport).await?)?;
    transport
      .send(channel.encrypt(&InnerMessage::DeviceRequest {
        method: method.into(),
        path: path.into(),
      })?)
      .await?;
    for chunk in body.chunks(MAX_CHUNK) {
      transport
        .send(channel.encrypt(&InnerMessage::RequestBody {
          data: protocol::encode(chunk),
        })?)
        .await?;
    }
    transport.send(channel.encrypt(&InnerMessage::RequestEnd {})?).await?;
    match channel.decrypt(&read_record(&mut transport).await?)? {
      InnerMessage::Response { status, content_type } if (200..=599).contains(&status) => {
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
          transport,
          channel,
          status,
          content_type,
        })
      }
      InnerMessage::Error { message } => Err(message),
      _ => Err("Expected authenticated host response".into()),
    }
  }

  pub async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
    match self.channel.decrypt(&read_record(&mut self.transport).await?)? {
      InnerMessage::Chunk { data } => Ok(Some(protocol::decode(&data, MAX_CHUNK)?)),
      InnerMessage::End {} => Ok(None),
      InnerMessage::Error { message } => Err(message),
      _ => Err("Unexpected encrypted response record".into()),
    }
  }

  pub async fn acknowledge(&mut self) -> Result<(), String> {
    self
      .transport
      .send(self.channel.encrypt(&InnerMessage::Window { credits: 1 })?)
      .await
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
