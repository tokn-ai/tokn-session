//! Local, versioned snapshot/follow protocol. Each JSON frame is prefixed by
//! a big-endian u32 length. Snapshots commit atomically after their last frame.
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokn_session_core::{Provider, SessionHeader};

use crate::RelayRecord;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_SERVICE_ENDPOINT: &str = "tcp://127.0.0.1:5557";
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogEntry {
  pub key: String,
  pub provider: Provider,
  pub header: SessionHeader,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Request {
  pub version: u32,
  #[serde(flatten)]
  pub action: Action,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
  Catalog,
  Follow {
    session_key: String,
  },
  FollowWindow {
    session_key: String,
    retain_from: Option<usize>,
    before_event: Option<usize>,
  },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
  Hello {
    version: u32,
    native: bool,
    providers: Vec<Provider>,
  },
  Header {
    entry: CatalogEntry,
  },
  CatalogEnd {
    warnings: Vec<String>,
  },
  Begin {
    generation: String,
    revision: String,
    reset: bool,
    header: SessionHeader,
  },
  Window {
    /// Generation-scoped anchor; positive values need not imply older history.
    event_offset: usize,
    has_earlier: bool,
  },
  Record {
    record: Box<RelayRecord>,
  },
  Commit {
    generation: String,
    revision: String,
  },
  Heartbeat,
  Error {
    message: String,
  },
}

/// An unauthenticated local service must never accidentally expose sessions
/// on a LAN. Remote access and authentication are deliberately not inferred.
pub fn local_endpoint(endpoint: &str) -> Result<SocketAddr, String> {
  let address: SocketAddr = endpoint
    .strip_prefix("tcp://")
    .ok_or("Relay endpoint must use tcp:// with a numeric loopback address")?
    .parse()
    .map_err(|_| "Invalid Relay endpoint; use tcp://127.0.0.1:5557")?;
  if !address.ip().is_loopback() {
    return Err("Relay snapshot service only permits loopback addresses".into());
  }
  Ok(address)
}

pub async fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut (impl AsyncRead + Unpin)) -> Result<T, String> {
  let length = reader.read_u32().await.map_err(|err| err.to_string())? as usize;
  if length == 0 || length > MAX_FRAME_BYTES {
    return Err("Relay frame exceeds the protocol size limit".into());
  }
  let mut bytes = vec![0; length];
  reader.read_exact(&mut bytes).await.map_err(|err| err.to_string())?;
  serde_json::from_slice(&bytes).map_err(|err| format!("Invalid Relay frame: {err}"))
}

pub async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), value: &impl Serialize) -> Result<(), String> {
  let bytes = serde_json::to_vec(value).map_err(|err| err.to_string())?;
  if bytes.len() > MAX_FRAME_BYTES {
    return Err("Relay record exceeds the protocol size limit".into());
  }
  writer
    .write_u32(bytes.len() as u32)
    .await
    .map_err(|err| err.to_string())?;
  writer.write_all(&bytes).await.map_err(|err| err.to_string())?;
  writer.flush().await.map_err(|err| err.to_string())
}
