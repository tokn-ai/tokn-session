//! Process-wide admission for disposable history journals, separate from
//! response buffers and source-record parsing limits.
use std::sync::{Arc, Mutex, OnceLock};

pub(super) use crate::service_protocol::MAX_NORMALIZED_RECORD_BYTES as MAX_RECORD_BYTES;

#[derive(Clone, Copy)]
pub(super) struct Limits {
  pub disk_bytes: usize,
  pub index_bytes: usize,
  pub records: usize,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Usage {
  pub disk_bytes: usize,
  pub index_bytes: usize,
  pub records: usize,
}

struct State {
  limits: Limits,
  usage: Usage,
}

#[derive(Clone)]
pub(super) struct Budget(Arc<Mutex<State>>);

impl Budget {
  pub fn shared() -> Self {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    BUDGET
      .get_or_init(|| {
        Self::new(Limits {
          disk_bytes: 2 * 1024 * 1024 * 1024,
          index_bytes: 64 * 1024 * 1024,
          records: 1_000_000,
        })
      })
      .clone()
  }

  pub fn new(limits: Limits) -> Self {
    Self(Arc::new(Mutex::new(State {
      limits,
      usage: Usage::default(),
    })))
  }

  pub fn reserve(&self, usage: Usage) -> Result<Charge, String> {
    let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
    let next = Usage {
      disk_bytes: state
        .usage
        .disk_bytes
        .checked_add(usage.disk_bytes)
        .ok_or("History scratch disk budget exhausted")?,
      index_bytes: state
        .usage
        .index_bytes
        .checked_add(usage.index_bytes)
        .ok_or("History index memory budget exhausted")?,
      records: state
        .usage
        .records
        .checked_add(usage.records)
        .ok_or("History index record budget exhausted")?,
    };
    if next.disk_bytes > state.limits.disk_bytes {
      return Err("History scratch disk budget exhausted".into());
    }
    if next.index_bytes > state.limits.index_bytes {
      return Err("History index memory budget exhausted".into());
    }
    if next.records > state.limits.records {
      return Err("History index record budget exhausted".into());
    }
    state.usage = next;
    Ok(Charge {
      budget: self.clone(),
      usage,
    })
  }

  #[cfg(test)]
  pub fn usage(&self) -> Usage {
    self.0.lock().unwrap_or_else(|error| error.into_inner()).usage
  }
}

/// Charges belong to physical journals, so snapshot clones neither charge
/// twice nor release storage while another snapshot still owns the file.
pub(super) struct Charge {
  budget: Budget,
  usage: Usage,
}

impl Charge {
  pub fn merge(&mut self, mut other: Self) {
    assert!(Arc::ptr_eq(&self.budget.0, &other.budget.0));
    self.usage.disk_bytes += other.usage.disk_bytes;
    self.usage.index_bytes += other.usage.index_bytes;
    self.usage.records += other.usage.records;
    other.usage = Usage::default();
  }
}

impl Drop for Charge {
  fn drop(&mut self) {
    if self.usage.disk_bytes == 0 && self.usage.index_bytes == 0 && self.usage.records == 0 {
      return;
    }
    let mut state = self.budget.0.lock().unwrap_or_else(|error| error.into_inner());
    state.usage.disk_bytes -= self.usage.disk_bytes;
    state.usage.index_bytes -= self.usage.index_bytes;
    state.usage.records -= self.usage.records;
  }
}
