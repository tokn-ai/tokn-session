//! Ordered, bounded record carriers. Authentication and encryption belong to
//! the caller, so new tunnels can implement this interface without changing Noise.

use async_trait::async_trait;

mod rtc;

pub use rtc::{WebRtcPeer, validate_stun_urls};

pub const MAX_RECORD: usize = 65_535;
pub const RECORD_CHANNEL_LABEL: &str = "tokn-record-v1";
pub type TransportResult<T> = Result<T, String>;
pub type BoxTransport = Box<dyn RecordTransport>;

/// One independently ordered exchange, with one encrypted record per message.
/// A failed send must never be retried on another carrier: delivery is uncertain.
#[async_trait]
pub trait RecordTransport: Send {
  async fn send(&mut self, record: Vec<u8>) -> TransportResult<()>;
  async fn receive(&mut self) -> TransportResult<Option<Vec<u8>>>;
  async fn close(&mut self);
}

pub fn validate_record(record: &[u8]) -> TransportResult<()> {
  if record.is_empty() || record.len() > MAX_RECORD {
    return Err("transport record size is invalid".into());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn records_have_explicit_bounds() {
    assert!(validate_record(&[]).is_err());
    assert!(validate_record(&vec![0; MAX_RECORD + 1]).is_err());
    assert!(validate_record(&vec![0; MAX_RECORD]).is_ok());
  }
}
