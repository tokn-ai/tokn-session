//! A list's change history is shared by all watchers with the same visibility,
//! filter and page. A lease holds only the routing identity and last push cursor.
mod projection;

use crate::{
  ViewerService,
  model::{DEFAULT_PAGE_LIMIT, SessionQuery, SessionSummary, SourceError, ViewerProvider},
  updates::SessionSubscriptionRenewal,
};
use projection::{PageSpec, Projection};
use serde::{Deserialize, Serialize};
use std::{
  collections::{BTreeMap, HashMap, HashSet, VecDeque},
  sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
  },
  time::{Duration, Instant},
};
use tokio::sync::Notify;

const MAX_VIEWS: usize = 64;
const MAX_LEASES: usize = 1024;
const MAX_RENEWALS: usize = 64;
const MAX_SESSIONS_PER_WINDOW: usize = 4096;
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
  pub since: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionListSubscriptionRequest {
  pub subscription_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionListEntry {
  pub session: SessionSummary,
  /// SHA-256 of the session summary visible in this scope.
  pub revision: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionListUpdate {
  pub subscription_id: String,
  /// A null base replaces the window. Clients store synced_through with that list.
  pub base_cursor: Option<String>,
  pub synced_through: String,
  pub sessions: Vec<SessionListEntry>,
  pub removed_keys: Vec<String>,
  pub next_cursor: Option<String>,
  pub pending_providers: Vec<ViewerProvider>,
  pub source_errors: Vec<SourceError>,
}

#[derive(Default)]
pub(crate) struct SessionLists {
  registry: Mutex<Registry>,
  pub(crate) changed: Notify,
  #[cfg(test)]
  reads: std::sync::atomic::AtomicUsize,
  #[cfg(test)]
  tombstone_limit: std::sync::OnceLock<usize>,
}

type Scope = Option<Arc<HashSet<String>>>;

#[derive(Default)]
struct Registry {
  // Ordered by most recent admission. Active views never get evicted.
  views: VecDeque<Arc<Projection>>,
  leases: HashMap<String, Lease>,
  epoch: u64,
}

struct Lease {
  expires: Instant,
  route: Arc<Route>,
}

struct Route {
  view: Arc<Projection>,
  local: bool,
  sent: AtomicU64,
}

struct Admission {
  route: Arc<Route>,
  resized_from: Option<Arc<Projection>>,
}

impl Registry {
  fn expire(&mut self, now: Instant) {
    self.leases.retain(|_, lease| lease.expires > now);
  }

  fn admit(&mut self, id: &str, spec: PageSpec, local: bool) -> Result<Admission, String> {
    let now = Instant::now();
    self.expire(now);
    let mut resized_from = None;
    if let Some(lease) = self.leases.get_mut(id) {
      if lease.route.local != local || !lease.route.view.spec.same_query(&spec) {
        return Err("Session list identity is already in use".into());
      }
      if lease.route.view.spec.limit == spec.limit {
        lease.expires = now + LEASE;
        return Ok(Admission {
          route: lease.route.clone(),
          resized_from: None,
        });
      }
      resized_from = Some(lease.route.view.clone());
    } else if self.leases.len() >= MAX_LEASES {
      return Err(SESSION_LIST_CAPACITY_ERROR.into());
    }

    let view = if let Some(index) = self.views.iter().position(|v| v.spec == spec) {
      self.views.remove(index).expect("matched existing view")
    } else {
      if self.views.len() >= MAX_VIEWS {
        // The current route can relinquish its old page when growing a window.
        let evict = self
          .views
          .iter()
          .position(|view| {
            !self
              .leases
              .iter()
              .any(|(other, lease)| other != id && Arc::ptr_eq(&lease.route.view, view))
          })
          .ok_or(SESSION_LIST_CAPACITY_ERROR)?;
        self.views.remove(evict);
      }
      // Epoch zero is reserved for a route that has not opened successfully.
      self.epoch = self.epoch.max(1);
      Arc::new(Projection::new(spec, self.epoch))
    };
    self.views.push_back(view.clone());
    let route = Arc::new(Route {
      view,
      local,
      sent: AtomicU64::new(0),
    });
    self.leases.insert(
      id.to_owned(),
      Lease {
        expires: now + LEASE,
        route: route.clone(),
      },
    );
    Ok(Admission { route, resized_from })
  }
}

impl SessionLists {
  #[cfg(test)]
  pub(crate) fn read_count(&self) -> usize {
    self.reads.load(Ordering::Relaxed)
  }

  #[cfg(test)]
  pub(crate) fn limit_tombstones(&self, count: usize) {
    self.tombstone_limit.set(count).unwrap();
  }

  pub(crate) fn invalidate(&self, keys: Option<&[String]>) {
    let Ok(mut registry) = self.registry.lock() else { return };
    registry.epoch = registry.epoch.checked_add(1).expect("session list clock overflow");
    if registry.views.is_empty() {
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
    // Mark retained views even while their last client is offline. No query runs
    // for them until someone opens the view again.
    let mut affected = false;
    for view in &registry.views {
      if view.spec.affected(keys, &providers) {
        view.invalidate(registry.epoch);
        affected = true;
      }
    }
    if affected && !registry.leases.is_empty() {
      self.changed.notify_one();
    }
  }

  fn forget(&self, id: &str, route: &Arc<Route>) {
    if let Ok(mut registry) = self.registry.lock()
      && registry
        .leases
        .get(id)
        .is_some_and(|lease| Arc::ptr_eq(&lease.route, route))
    {
      registry.leases.remove(id);
    }
  }
}

impl ViewerService {
  pub fn open_session_list(&self, request: OpenSessionListRequest) -> Result<SessionListUpdate, String> {
    self.open_list(request, false)
  }

  /// Native desktop events route directly to the webview. Embedded HTTP
  /// clients receive their updates over their own SSE connections.
  pub fn open_local_session_list(&self, request: OpenSessionListRequest) -> Result<SessionListUpdate, String> {
    self.open_list(request, true)
  }

  fn open_list(&self, request: OpenSessionListRequest, local: bool) -> Result<SessionListUpdate, String> {
    if !self.valid_subscription_id(&request.subscription_id) {
      return Err("Invalid session list identity".into());
    }
    let limit = request.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    if !(1..=MAX_SESSIONS_PER_WINDOW).contains(&limit) {
      return Err(format!(
        "Live session lists support 1 to {MAX_SESSIONS_PER_WINDOW} sessions per window"
      ));
    }
    if let Some(parent) = &request.parent_session_key {
      self.check_session_scope(parent)?;
      crate::model::decode_session_key(parent)?;
    }
    let spec = PageSpec::new(
      self.session_scope.clone(),
      request.query,
      request.parent_session_key,
      request.offset,
      limit,
    );
    let id = request.subscription_id;
    let admission = self
      .session_lists
      .registry
      .lock()
      .map_err(|_| "Session list store unavailable")?
      .admit(&id, spec, local)?;
    // Never hold the registry lock across a database read, or two projection
    // locks together when a route grows from one page to another.
    let previous = admission
      .resized_from
      .as_ref()
      .and_then(|old| old.saved_digests(request.since.as_deref()));
    let route = admission.route;
    let view = &route.view;
    let result = view.capture(self, |page| {
      let update = if let Some((since, previous)) = previous {
        page.resized(id.clone(), since, &previous)
      } else {
        page.update(id.clone(), request.since.as_deref())
      };
      route.sent.store(page.head, Ordering::Release);
      update
    });
    if result.is_err() {
      self.session_lists.forget(&id, &route);
    }
    result
  }

  pub fn renew_session_lists(&self, ids: &[String]) -> Result<SessionSubscriptionRenewal, String> {
    if ids.len() > MAX_RENEWALS || ids.iter().any(|id| !self.valid_subscription_id(id)) {
      return Err("Invalid session list renewal".into());
    }
    let mut registry = self
      .session_lists
      .registry
      .lock()
      .map_err(|_| "Session list store unavailable")?;
    let now = Instant::now();
    registry.expire(now);
    let mut renewal = SessionSubscriptionRenewal::default();
    let mut seen = HashSet::new();
    for id in ids.iter().filter(|id| seen.insert(*id)) {
      if let Some(lease) = registry
        .leases
        .get_mut(id)
        .filter(|lease| lease.route.view.spec.scope == self.session_scope)
      {
        lease.expires = now + LEASE;
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
    let mut registry = self
      .session_lists
      .registry
      .lock()
      .map_err(|_| "Session list store unavailable")?;
    let owned = registry
      .leases
      .get(&request.subscription_id)
      .is_some_and(|lease| lease.route.view.spec.scope == self.session_scope);
    if owned {
      registry.leases.remove(&request.subscription_id);
    }
    Ok(owned)
  }

  pub(crate) fn publish_session_list_batches(&self, mut publish: impl FnMut(SessionListUpdate, Vec<String>, bool)) {
    let views = {
      let Ok(mut registry) = self.session_lists.registry.lock() else {
        return;
      };
      registry.expire(Instant::now());
      let mut active = HashMap::<_, (Arc<Projection>, Vec<(String, Arc<Route>)>)>::new();
      for (id, lease) in &registry.leases {
        let view = &lease.route.view;
        let (_, routes) = active
          .entry(view.identity)
          .or_insert_with(|| (view.clone(), Vec::new()));
        routes.push((id.clone(), lease.route.clone()));
      }
      active
    };
    for (view, routes) in views.into_values() {
      let result = view.capture(self, |page| {
        // Clients at the same cutoff receive the same payload. The routing ID
        // is the only per-client part, including for independently opened tabs.
        let mut cohorts = BTreeMap::<(u64, bool), Vec<String>>::new();
        for (id, route) in &routes {
          let sent = route.sent.load(Ordering::Acquire);
          if sent != 0 && sent != page.head {
            cohorts.entry((sent, route.local)).or_default().push(id.clone());
            route.sent.store(page.head, Ordering::Release);
          }
        }
        (!cohorts.is_empty()).then(|| (page.clone(), cohorts))
      });
      let delivery = match result {
        Ok(delivery) => delivery,
        Err(_) => {
          for (id, route) in &routes {
            self.session_lists.forget(id, route);
          }
          continue;
        }
      };
      let Some((page, cohorts)) = delivery else { continue };
      // Hold only the immutable page here. Build one cohort's payload at a
      // time, and never call transport code under the projection lock.
      for ((since, local), ids) in cohorts {
        publish(page.delta_or_snapshot(String::new(), Some(since)), ids, local);
      }
    }
  }

  #[cfg(test)]
  pub(crate) fn publish_session_lists(&self, mut publish: impl FnMut(SessionListUpdate)) {
    self.publish_session_list_batches(|update, ids, _| {
      for subscription_id in ids {
        publish(SessionListUpdate {
          subscription_id,
          ..update.clone()
        });
      }
    });
  }
}
