use std::collections::{HashMap, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptError {
  Conflict,
  Full,
}

#[derive(Clone, Debug)]
pub enum ReceiptAdmission<R> {
  New,
  Replay(R),
}

struct Entry<R> {
  scope: String,
  fingerprint: [u8; 32],
  response: R,
  complete: bool,
  retained: bool,
}

/// A bounded receipt ledger shared by UI delivery and runtime interactions.
/// Pending and explicitly retained outcomes are never evicted. Other completed
/// outcomes remain replayable until normal capacity eviction. This is process-local
/// deduplication, not an exactly-once promise after host restarts.
pub struct ReceiptLedger<R> {
  capacity: usize,
  entries: HashMap<String, Entry<R>>,
  completed: VecDeque<String>,
}

impl<R: Clone> ReceiptLedger<R> {
  pub fn new(capacity: usize) -> Self {
    Self {
      capacity,
      entries: HashMap::new(),
      completed: VecDeque::new(),
    }
  }

  pub fn lookup(&self, request_id: &str, scope: &str, fingerprint: &[u8; 32]) -> Result<Option<R>, ReceiptError> {
    let Some(entry) = self.entries.get(request_id) else {
      return Ok(None);
    };
    if entry.scope != scope || &entry.fingerprint != fingerprint {
      return Err(ReceiptError::Conflict);
    }
    Ok(Some(entry.response.clone()))
  }

  pub fn admit(
    &mut self,
    request_id: String,
    scope: String,
    fingerprint: [u8; 32],
    pending: R,
  ) -> Result<ReceiptAdmission<R>, ReceiptError> {
    if let Some(response) = self.lookup(&request_id, &scope, &fingerprint)? {
      return Ok(ReceiptAdmission::Replay(response));
    }
    while self.entries.len() >= self.capacity {
      let Some(oldest) = self.completed.pop_front() else {
        return Err(ReceiptError::Full);
      };
      if self.entries.get(&oldest).is_some_and(|entry| !entry.retained) {
        self.entries.remove(&oldest);
      }
    }
    self.entries.insert(
      request_id,
      Entry {
        scope,
        fingerprint,
        response: pending,
        complete: false,
        retained: false,
      },
    );
    Ok(ReceiptAdmission::New)
  }

  pub fn complete(&mut self, request_id: &str, response: R) {
    if let Some(entry) = self.entries.get_mut(request_id) {
      entry.response = response;
      if !entry.complete {
        entry.complete = true;
        self.completed.push_back(request_id.to_owned());
      }
    }
  }

  /// Keep an uncertain result for the lifetime of this ledger. When retained or
  /// pending entries fill the bound, reject new admissions instead of resending.
  pub fn retain_outcome(&mut self, request_id: &str, response: R) {
    if let Some(entry) = self.entries.get_mut(request_id) {
      entry.response = response;
      entry.complete = true;
      entry.retained = true;
    }
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn retained_uncertain_outcomes_reject_capacity_pressure_instead_of_replaying() {
    let mut ledger = ReceiptLedger::new(1);
    ledger.admit("a".into(), "runtime".into(), [1; 32], "pending").unwrap();
    ledger.retain_outcome("a", "uncertain");
    assert!(matches!(
      ledger.admit("b".into(), "runtime".into(), [2; 32], "pending"),
      Err(ReceiptError::Full)
    ));
    assert_eq!(ledger.lookup("a", "runtime", &[1; 32]).unwrap(), Some("uncertain"));
  }

  #[test]
  fn pending_receipts_survive_capacity_pressure_and_results_are_identity_bound() {
    let mut ledger = ReceiptLedger::new(2);
    ledger
      .admit("a".into(), "runtime-1".into(), [1; 32], "pending")
      .unwrap();
    ledger
      .admit("b".into(), "runtime-1".into(), [2; 32], "pending")
      .unwrap();
    assert!(matches!(
      ledger.admit("c".into(), "runtime-1".into(), [3; 32], "pending"),
      Err(ReceiptError::Full)
    ));
    ledger.complete("b", "uncertain");
    assert_eq!(ledger.lookup("b", "runtime-1", &[2; 32]).unwrap(), Some("uncertain"));
    assert_eq!(ledger.lookup("b", "runtime-2", &[2; 32]), Err(ReceiptError::Conflict));
    assert_eq!(ledger.lookup("b", "runtime-1", &[3; 32]), Err(ReceiptError::Conflict));
    ledger
      .admit("c".into(), "runtime-1".into(), [3; 32], "pending")
      .unwrap();
    assert_eq!(ledger.lookup("a", "runtime-1", &[1; 32]).unwrap(), Some("pending"));
    assert_eq!(ledger.lookup("b", "runtime-1", &[2; 32]).unwrap(), None);
    assert_eq!(ledger.len(), 2);
  }
}
