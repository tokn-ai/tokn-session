//! Leased, metadata-only windows over the session sidebar. The query runs on
//! open and on source changes. Renewing a window never touches the index.
use crate::{
  ViewerService,
  model::{DEFAULT_PAGE_LIMIT, ListSessionsResponse, SessionQuery, SessionSummary, SourceError, ViewerProvider},
  updates::SessionSubscriptionRenewal,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, digest::Output};
use std::{
  collections::{BTreeMap, HashMap, HashSet},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};
use tokio::sync::Notify;

const MAX_WINDOWS: usize = 64;
const MAX_WINDOW_ROWS: usize = 4096;
const MAX_BASELINE_BYTES: usize = 4 * 1024 * 1024;
const LEASE: Duration = Duration::from_secs(90);
pub const SESSION_LIST_CAPACITY_ERROR: &str = "Live session list capacity exceeded";

#[derive(Clone, Debug, Deserialize)]
pub struct OpenSessionListRequest {
  pub subscription_id: String,
  #[serde(default)]
  pub query: SessionQuery,
  pub parent_session_key: Option<String>,
  #[serde(default)]
  pub offset: usize,
  pub limit: Option<usize>,
  pub generation: Option<String>,
  pub revision: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionListSubscriptionRequest {
  pub subscription_id: String,
}

#[derive(Debug, Serialize)]
pub struct SessionListRow {
  pub session: SessionSummary,
  /// SHA-256 of this scoped row's content. Independent of its page or position.
  pub revision: String,
}

#[derive(Debug, Serialize)]
pub struct SessionListUpdate {
  pub subscription_id: String,
  pub generation: String,
  /// A null base replaces the window. Clients choose the display order.
  pub base_revision: Option<String>,
  pub revision: String,
  pub rows: Vec<SessionListRow>,
  pub removed_keys: Vec<String>,
  pub next_cursor: Option<String>,
  pub pending_providers: Vec<ViewerProvider>,
  pub source_errors: Vec<SourceError>,
}

#[derive(Default)]
pub(crate) struct SessionLists {
  windows: Mutex<HashMap<String, LeasedWindow>>,
  pub(crate) changed: Notify,
}

type Scope = Option<Arc<HashSet<String>>>;

struct LeasedWindow {
  expires: Instant,
  window: Arc<Window>,
}

struct Window {
  id: String,
  scope: Scope,
  query: SessionQuery,
  parent: Option<String>,
  offset: usize,
  generation: String,
  dirty: AtomicBool,
  replica: Mutex<Replica>,
}

impl SessionLists {
  /// Directory changes can affect ranking across an entire query. For known
  /// row changes, only a grant or provider that can see those rows needs work.
  pub(crate) fn invalidate(&self, keys: Option<&[String]>) {
    let Ok(windows) = self.windows.lock() else { return };
    if windows.is_empty() {
      return;
    }
    let providers: HashSet<_> = keys
      .into_iter()
      .flatten()
      .filter_map(|key| {
        crate::model::decode_session_key(key)
          .ok()
          .map(|locator| locator.provider)
      })
      .collect();
    let now = Instant::now();
    let mut wake = false;
    for entry in windows.values().filter(|entry| entry.expires > now) {
      let window = &entry.window;
      if let Some(keys) = keys {
        if window
          .scope
          .as_ref()
          .is_some_and(|scope| !keys.iter().any(|key| scope.contains(key)))
        {
          continue;
        }
        if !providers.is_empty()
          && !window.query.providers.is_empty()
          && window.parent.is_none()
          && !window
            .query
            .providers
            .iter()
            .any(|provider| providers.contains(provider))
        {
          continue;
        }
      }
      window.dirty.store(true, Ordering::Release);
      wake = true;
    }
    if wake {
      self.changed.notify_one();
    }
  }
}

#[derive(Default)]
struct Replica {
  limit: usize,
  revision: u64,
  baseline: Option<Baseline>,
}

// Retain IDs and row fingerprints. Titles, previews and other row bodies live
// only in the index and in the outgoing changed rows.
#[derive(Eq, PartialEq)]
struct Baseline {
  rows: BTreeMap<String, Output<Sha256>>,
  next_cursor: Option<String>,
  pending_providers: Vec<ViewerProvider>,
  source_errors: Vec<SourceError>,
}

impl Baseline {
  fn new(page: &ListSessionsResponse) -> Result<Self, String> {
    let size = page
      .sessions
      .iter()
      .map(|row| row.session_key.len() + 64)
      .sum::<usize>();
    if size > MAX_BASELINE_BYTES {
      return Err(SESSION_LIST_CAPACITY_ERROR.into());
    }
    let mut rows = BTreeMap::new();
    for row in &page.sessions {
      let mut hash = Sha256::new();
      serde_json::to_writer(&mut hash, row).map_err(|error| error.to_string())?;
      rows.insert(row.session_key.clone(), hash.finalize());
    }
    Ok(Self {
      rows,
      next_cursor: page.next_cursor.clone(),
      pending_providers: page.pending_providers.clone(),
      source_errors: page.source_errors.clone(),
    })
  }
}

impl Window {
  fn read(&self, service: &ViewerService, limit: usize) -> Result<ListSessionsResponse, String> {
    let mut reader = service.clone();
    reader.session_scope = self.scope.clone();
    if let Some(parent) = &self.parent {
      let page = reader.list_session_child_window(parent, self.offset, limit, true)?;
      return Ok(ListSessionsResponse {
        sessions: page.sessions,
        next_cursor: page.next_cursor,
        pending_providers: Vec::new(),
        source_errors: Vec::new(),
      });
    }
    reader.list_session_window(&self.query, self.offset, limit)
  }

  fn capture(
    &self,
    service: &ViewerService,
    replica: &mut Replica,
    limit: usize,
    snapshot: bool,
    acknowledge: bool,
  ) -> Result<Option<SessionListUpdate>, String> {
    let page = self.read(service, limit)?;
    let current = Baseline::new(&page)?;
    let previous = replica.baseline.as_ref();
    let changed = previous != Some(&current);
    replica.limit = limit;
    if !changed && !acknowledge {
      return Ok(None);
    }
    let base_revision = (!snapshot).then(|| replica.revision.to_string());
    let base = previous.filter(|_| !snapshot);
    let removed_keys = base
      .into_iter()
      .flat_map(|base| base.rows.keys())
      .filter(|key| !current.rows.contains_key(*key))
      .cloned()
      .collect();
    let rows = page
      .sessions
      .into_iter()
      .filter_map(|session| {
        let digest = &current.rows[&session.session_key];
        let previous_digest = base.and_then(|base| base.rows.get(&session.session_key));
        (Some(digest) != previous_digest).then(|| SessionListRow {
          session,
          revision: format!("{digest:x}"),
        })
      })
      .collect();
    if changed {
      replica.revision += 1;
      replica.baseline = Some(current);
    }
    Ok(Some(SessionListUpdate {
      subscription_id: self.id.clone(),
      generation: self.generation.clone(),
      base_revision,
      revision: replica.revision.to_string(),
      rows,
      removed_keys,
      next_cursor: page.next_cursor,
      pending_providers: page.pending_providers,
      source_errors: page.source_errors,
    }))
  }
}

impl ViewerService {
  pub fn open_session_list(&self, request: OpenSessionListRequest) -> Result<SessionListUpdate, String> {
    let limit = request.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    if !self.valid_subscription_id(&request.subscription_id) {
      return Err("Invalid session list identity".into());
    }
    if !(1..=MAX_WINDOW_ROWS).contains(&limit) {
      return Err(format!(
        "Live session lists support 1 to {MAX_WINDOW_ROWS} rows per window"
      ));
    }
    if let Some(parent) = &request.parent_session_key {
      self.check_session_scope(parent)?;
      crate::model::decode_session_key(parent)?;
    }
    let window = {
      let mut windows = self
        .session_lists
        .windows
        .lock()
        .map_err(|_| "Session list store unavailable")?;
      let now = Instant::now();
      windows.retain(|_, entry| entry.expires > now);
      if let Some(entry) = windows.get_mut(&request.subscription_id) {
        let window = &entry.window;
        if window.scope != self.session_scope
          || window.query != request.query
          || window.parent != request.parent_session_key
          || window.offset != request.offset
        {
          return Err("Session list identity is already in use".into());
        }
        entry.expires = now + LEASE;
        window.clone()
      } else {
        if windows.len() >= MAX_WINDOWS {
          return Err(SESSION_LIST_CAPACITY_ERROR.into());
        }
        let window = Arc::new(Window {
          id: request.subscription_id.clone(),
          scope: self.session_scope.clone(),
          query: request.query,
          parent: request.parent_session_key,
          offset: request.offset,
          generation: uuid::Uuid::new_v4().to_string(),
          dirty: AtomicBool::new(false),
          replica: Mutex::default(),
        });
        windows.insert(
          request.subscription_id,
          LeasedWindow {
            expires: now + LEASE,
            window: window.clone(),
          },
        );
        window
      }
    };
    let mut replica = window.replica.lock().map_err(|_| "Session list unavailable")?;
    let matches = replica.baseline.is_some()
      && request.generation.as_ref() == Some(&window.generation)
      && request.revision.as_ref() == Some(&replica.revision.to_string());
    let result = window.capture(self, &mut replica, limit, !matches, true);
    if result
      .as_ref()
      .is_err_and(|error| replica.baseline.is_none() || error == SESSION_LIST_CAPACITY_ERROR)
    {
      self.forget_session_list(&window);
    }
    result.map(|update| update.expect("an open always acknowledges its revision"))
  }

  pub fn renew_session_lists(&self, ids: &[String]) -> Result<SessionSubscriptionRenewal, String> {
    if ids.len() > MAX_WINDOWS || ids.iter().any(|id| !self.valid_subscription_id(id)) {
      return Err("Invalid session list renewal".into());
    }
    let mut windows = self
      .session_lists
      .windows
      .lock()
      .map_err(|_| "Session list store unavailable")?;
    let now = Instant::now();
    windows.retain(|_, entry| entry.expires > now);
    let mut renewal = SessionSubscriptionRenewal::default();
    let mut seen = HashSet::new();
    for id in ids.iter().filter(|id| seen.insert(*id)) {
      if let Some(entry) = windows
        .get_mut(id)
        .filter(|entry| entry.window.scope == self.session_scope)
      {
        entry.expires = now + LEASE;
        renewal.renewed_ids.push(id.clone());
      } else {
        renewal.missing_ids.push(id.clone());
      }
    }
    Ok(renewal)
  }

  pub fn release_session_list(&self, request: SessionListSubscriptionRequest) -> Result<bool, String> {
    if !self.valid_subscription_id(&request.subscription_id) {
      return Err("Invalid session list identity".into());
    }
    let mut windows = self
      .session_lists
      .windows
      .lock()
      .map_err(|_| "Session list store unavailable")?;
    let owned = windows
      .get(&request.subscription_id)
      .is_some_and(|e| e.window.scope == self.session_scope);
    if owned {
      windows.remove(&request.subscription_id);
    }
    Ok(owned)
  }

  fn forget_session_list(&self, window: &Arc<Window>) {
    if let Ok(mut windows) = self.session_lists.windows.lock()
      && windows
        .get(&window.id)
        .is_some_and(|entry| Arc::ptr_eq(&entry.window, window))
    {
      windows.remove(&window.id);
    }
  }

  pub(crate) fn publish_session_lists(&self, mut publish: impl FnMut(SessionListUpdate)) {
    let windows: Vec<_> = {
      let Ok(mut windows) = self.session_lists.windows.lock() else {
        return;
      };
      let now = Instant::now();
      windows.retain(|_, entry| entry.expires > now);
      windows
        .values()
        .filter(|entry| entry.window.dirty.swap(false, Ordering::AcqRel))
        .map(|entry| entry.window.clone())
        .collect()
    };
    for window in windows {
      let Ok(mut replica) = window.replica.lock() else {
        continue;
      };
      if replica.baseline.is_none() {
        continue;
      }
      let limit = replica.limit;
      match window.capture(self, &mut replica, limit, false, false) {
        Ok(Some(update)) => publish(update),
        Ok(None) => {}
        // Preserve the displayed rows. The next renewal will request a new
        // baseline instead of acknowledging a subscription which missed data.
        Err(_) => self.forget_session_list(&window),
      }
    }
  }
}
