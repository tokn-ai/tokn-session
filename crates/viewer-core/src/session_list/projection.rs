//! One retained index of the latest visible summaries for a particular page.
//! Updating it also records departures from that page, including sessions
//! displaced at a ranking boundary or excluded by a changed search match.
use super::{SESSION_LIST_CAPACITY_ERROR, Scope, SessionListEntry, SessionListUpdate};
use crate::{
  ViewerService,
  model::{ListSessionsResponse, SessionQuery, SourceError, ViewerProvider},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
  collections::{BTreeMap, HashSet},
  io::{self, Write},
  sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
  },
};

const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOMBSTONES: usize = 4096;

#[derive(Eq, PartialEq)]
pub(super) struct PageSpec {
  pub scope: Scope,
  query: SessionQuery,
  parent: Option<String>,
  offset: usize,
  pub limit: usize,
}

impl PageSpec {
  pub fn new(scope: Scope, mut query: SessionQuery, parent: Option<String>, offset: usize, limit: usize) -> Self {
    if parent.is_some() {
      // The child query is determined entirely by its parent.
      query = SessionQuery::default();
    } else {
      query.providers = crate::service::selected_providers(&query.providers);
      query.search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    }
    Self {
      scope,
      query,
      parent,
      offset,
      limit,
    }
  }

  pub fn same_query(&self, other: &Self) -> bool {
    self.scope == other.scope && self.query == other.query && self.parent == other.parent && self.offset == other.offset
  }

  pub fn affected(&self, keys: Option<&[String]>, providers: &HashSet<ViewerProvider>) -> bool {
    let Some(keys) = keys else { return true };
    if self
      .scope
      .as_ref()
      .is_some_and(|scope| !keys.iter().any(|key| scope.contains(key)))
    {
      return false;
    }
    self.parent.is_some() || providers.is_empty() || self.query.providers.iter().any(|p| providers.contains(p))
  }

  fn read(&self, service: &ViewerService) -> Result<ListSessionsResponse, String> {
    let mut reader = service.clone();
    reader.session_scope = self.scope.clone();
    if let Some(parent) = &self.parent {
      let page = reader.list_session_child_window(parent, self.offset, self.limit, true)?;
      return Ok(ListSessionsResponse {
        sessions: page.sessions,
        next_cursor: page.next_cursor,
        pending_providers: Vec::new(),
        source_errors: Vec::new(),
      });
    }
    reader.list_session_window(&self.query, self.offset, self.limit)
  }
}

pub(super) struct Projection {
  pub identity: uuid::Uuid,
  pub spec: PageSpec,
  target_epoch: AtomicU64,
  page: Mutex<Page>,
}

impl Projection {
  pub fn new(spec: PageSpec, epoch: u64) -> Self {
    let identity = uuid::Uuid::new_v4();
    Self {
      identity,
      spec,
      target_epoch: AtomicU64::new(epoch),
      page: Mutex::new(Page {
        identity: identity.to_string(),
        ..Page::default()
      }),
    }
  }

  pub fn invalidate(&self, epoch: u64) {
    self.target_epoch.store(epoch, Ordering::Release);
  }

  pub fn saved_digests(&self, cursor: Option<&str>) -> Option<(String, BTreeMap<String, String>)> {
    let page = self.page.lock().ok()?;
    let cursor = cursor?;
    if page.head == 0 || page.sequence(cursor)? != page.head {
      return None;
    }
    Some((
      cursor.to_owned(),
      page
        .sessions
        .iter()
        .map(|(key, member)| (key.clone(), member.entry.revision.clone()))
        .collect(),
    ))
  }

  pub fn capture<T>(&self, service: &ViewerService, reply: impl FnOnce(&Page) -> T) -> Result<T, String> {
    // Invalidations never need this lock. If another index commit races the
    // read, its newer target remains pending for the next capture.
    let mut page = self.page.lock().map_err(|_| "Session list projection unavailable")?;
    let epoch = self.target_epoch.load(Ordering::Acquire);
    if page.checked_epoch != epoch {
      #[cfg(test)]
      service.session_lists.reads.fetch_add(1, Ordering::Relaxed);
      let current = self.spec.read(service)?;
      // Do not expose the owner's clock gaps (including hidden sessions'
      // changes) to a guest. Advance only for visible changes to its page.
      let stamp = if self.spec.scope.is_some() {
        page.head.checked_add(1).expect("session list clock overflow")
      } else {
        epoch
      };
      #[cfg(test)]
      let max_tombstones = service
        .session_lists
        .tombstone_limit
        .get()
        .copied()
        .unwrap_or(MAX_TOMBSTONES);
      #[cfg(not(test))]
      let max_tombstones = MAX_TOMBSTONES;
      page.reconcile(current, stamp, max_tombstones)?;
      page.checked_epoch = epoch;
    }
    Ok(reply(&page))
  }
}

struct Member {
  changed_at: u64,
  entry: SessionListEntry,
}

#[derive(Clone, Default)]
pub(super) struct Page {
  identity: String,
  checked_epoch: u64,
  pub head: u64,
  floor: u64,
  // A publisher can hold this immutable version while encoding for many
  // clients. New opens and commits then proceed on the projection lock.
  sessions: Arc<BTreeMap<String, Member>>,
  removed: Arc<BTreeMap<String, u64>>,
  next_cursor: Option<String>,
  pending_providers: Vec<ViewerProvider>,
  source_errors: Vec<SourceError>,
}

// Count serialized storage while hashing, without an extra copy of a large
// summary. Keys and fixed per-entry bookkeeping are budgeted separately.
#[derive(Default)]
struct SizedDigest {
  hash: Sha256,
  bytes: usize,
}

impl Write for SizedDigest {
  fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    self.bytes += bytes.len();
    self.hash.update(bytes);
    Ok(bytes.len())
  }
  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

impl SizedDigest {
  fn of(value: &impl Serialize) -> Result<Self, String> {
    let mut digest = Self::default();
    serde_json::to_writer(&mut digest, value).map_err(|err| err.to_string())?;
    Ok(digest)
  }
}

impl Page {
  fn reconcile(&mut self, current: ListSessionsResponse, stamp: u64, max_tombstones: usize) -> Result<(), String> {
    let mut changed = self.head == 0
      || self.sessions.len() != current.sessions.len()
      || self.next_cursor != current.next_cursor
      || self.pending_providers != current.pending_providers
      || self.source_errors != current.source_errors;
    let mut bytes = SizedDigest::of(&(&current.next_cursor, &current.pending_providers, &current.source_errors))?.bytes;
    if bytes > MAX_PAGE_BYTES {
      return Err(SESSION_LIST_CAPACITY_ERROR.into());
    }
    let mut sessions = BTreeMap::new();
    for session in current.sessions {
      let digest = SizedDigest::of(&session)?;
      bytes += digest.bytes + session.session_key.len() + 128;
      if bytes > MAX_PAGE_BYTES {
        return Err(SESSION_LIST_CAPACITY_ERROR.into());
      }
      let revision = format!("{:x}", digest.hash.finalize());
      let old = self.sessions.get(&session.session_key);
      let changed_at = match old {
        Some(old) if old.entry.revision == revision => old.changed_at,
        _ => {
          changed = true;
          stamp
        }
      };
      sessions.insert(
        session.session_key.clone(),
        Member {
          changed_at,
          entry: SessionListEntry { session, revision },
        },
      );
    }
    if !changed {
      return Ok(());
    }
    if self.head == 0 {
      self.floor = stamp;
    }
    let removed = Arc::make_mut(&mut self.removed);
    for key in self.sessions.keys().filter(|k| !sessions.contains_key(*k)) {
      removed.insert(key.clone(), stamp);
    }
    removed.retain(|k, _| !sessions.contains_key(k));
    self.trim_tombstones(max_tombstones);
    self.head = stamp;
    self.sessions = Arc::new(sessions);
    self.next_cursor = current.next_cursor;
    self.pending_providers = current.pending_providers;
    self.source_errors = current.source_errors;
    Ok(())
  }

  fn trim_tombstones(&mut self, limit: usize) {
    let mut bytes: usize = self.removed.keys().map(|key| key.len() + 64).sum();
    if self.removed.len() <= limit && bytes <= MAX_PAGE_BYTES {
      return;
    }
    let removed = Arc::make_mut(&mut self.removed);
    let mut by_age: Vec<_> = removed.iter().map(|(key, at)| (*at, key.clone())).collect();
    by_age.sort_unstable();
    for (at, key) in by_age {
      if removed.len() <= limit && bytes <= MAX_PAGE_BYTES {
        break;
      }
      self.floor = self.floor.max(at);
      bytes -= key.len() + 64;
      removed.remove(&key);
    }
  }

  fn sequence(&self, cursor: &str) -> Option<u64> {
    let (identity, sequence) = cursor.split_once(':')?;
    if identity != self.identity {
      return None;
    }
    let number = sequence.parse::<u64>().ok()?;
    (sequence == number.to_string()).then_some(number)
  }

  fn cursor(&self, seq: u64) -> String {
    format!("{}:{seq}", self.identity)
  }

  pub fn update(&self, id: String, since: Option<&str>) -> SessionListUpdate {
    self.delta_or_snapshot(id, since.and_then(|cursor| self.sequence(cursor)))
  }

  pub fn delta_or_snapshot(&self, id: String, since: Option<u64>) -> SessionListUpdate {
    let base = since.filter(|s| *s >= self.floor && *s <= self.head);
    if base == Some(self.head) {
      return self.response(id, Some(self.cursor(self.head)), Vec::new(), Vec::new());
    }
    let sessions = self
      .sessions
      .values()
      .filter(|m| base.is_none_or(|s| m.changed_at > s))
      .map(|m| m.entry.clone())
      .collect();
    let removed_keys = self
      .removed
      .iter()
      .filter(|(_, at)| base.is_some_and(|s| **at > s))
      .map(|(k, _)| k.clone())
      .collect();
    self.response(id, base.map(|s| self.cursor(s)), sessions, removed_keys)
  }

  pub fn resized(&self, id: String, base: String, previous: &BTreeMap<String, String>) -> SessionListUpdate {
    let sessions = self
      .sessions
      .iter()
      .filter(|(k, m)| previous.get(*k) != Some(&m.entry.revision))
      .map(|(_, m)| m.entry.clone())
      .collect();
    let removed_keys = previous
      .keys()
      .filter(|k| !self.sessions.contains_key(*k))
      .cloned()
      .collect();
    self.response(id, Some(base), sessions, removed_keys)
  }

  fn response(
    &self,
    subscription_id: String,
    base_cursor: Option<String>,
    sessions: Vec<SessionListEntry>,
    removed_keys: Vec<String>,
  ) -> SessionListUpdate {
    SessionListUpdate {
      subscription_id,
      base_cursor,
      synced_through: self.cursor(self.head),
      sessions,
      removed_keys,
      next_cursor: self.next_cursor.clone(),
      pending_providers: self.pending_providers.clone(),
      source_errors: self.source_errors.clone(),
    }
  }
}
