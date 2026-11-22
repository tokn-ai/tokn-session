//! Shared versioned Hub wire messages, bounds, encoding, and route policy.
pub use tokn_hub_client_core::protocol::*;

use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Queue slots cover all logical streams. A shared byte budget bounds the
/// actual heap usage across slow recipients, independently of record counts.
pub(crate) const RESPONSE_RECORD_SLOTS: usize = (RESPONSE_WINDOW + 2) * crate::secure::MAX_STREAMS + 2;
pub(crate) const REQUEST_RECORD_SLOTS: usize = (MAX_BODY.div_ceil(CHUNK_SIZE) + 2) * crate::secure::MAX_STREAMS + 2;

pub(crate) fn budget_record(bytes: Vec<u8>, budget: &Arc<Semaphore>) -> Option<Bytes> {
  struct Record {
    bytes: Vec<u8>,
    _budget: OwnedSemaphorePermit,
  }
  impl AsRef<[u8]> for Record {
    fn as_ref(&self) -> &[u8] {
      &self.bytes
    }
  }
  let permit = budget
    .clone()
    .try_acquire_many_owned(u32::try_from(bytes.capacity()).ok()?)
    .ok()?;
  Some(Bytes::from_owner(Record { bytes, _budget: permit }))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn queued_ciphertext_releases_its_budget_when_the_last_clone_drops() {
    let budget = Arc::new(Semaphore::new(96));
    let held = budget_record(vec![0; 64], &budget).unwrap();
    assert!(budget_record(vec![0; 33], &budget).is_none());
    let copy = held.clone();
    drop(held);
    assert_eq!(budget.available_permits(), 32);
    drop(copy);
    assert!(budget_record(vec![0; 96], &budget).is_some());
  }
}
