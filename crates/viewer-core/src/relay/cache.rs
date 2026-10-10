//! Session residency is independent from filesystem activity. Only explicit
//! access advances the user LRU; live feed hints may occupy spare preload slots.
use super::{State, ViewerRelay};
use crate::model::{SessionLocator, ViewerProvider};
use std::{
  collections::{HashMap, HashSet},
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant, SystemTime},
};
use tokio_util::sync::CancellationToken;

const MAX_SESSIONS: usize = 8;
const MAX_BACKGROUND_SESSIONS: usize = 2;
const MEMORY_TARGET_BYTES: usize = 64 * 1024 * 1024;
const VIEW_LEASE: Duration = Duration::from_secs(90);
const PREFETCH_DEBOUNCE: Duration = Duration::from_millis(250);
// A third streaming session must not repeatedly evict and reparse the other
// two. Replace only settled, idle preloads and cool down attempts after eviction.
const PREFETCH_IDLE_PERIOD: Duration = Duration::from_secs(30);
const MAX_VIEWS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum SessionPriority {
  Background,
  Explicit,
}

struct ViewLease {
  revision: u64,
  selected: Option<SessionLocator>,
  candidates: HashSet<SessionLocator>,
  updated: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceVersion(Vec<Option<(u64, SystemTime)>>);

impl SourceVersion {
  fn read(locator: &SessionLocator) -> Self {
    // SQLite writes may change only the WAL. JSONL providers have just one
    // dependency here; authoritative readers validate their full lineage.
    let mut paths = vec![locator.source_path.clone()];
    if matches!(
      locator.provider,
      ViewerProvider::OpenCode | ViewerProvider::ZCode | ViewerProvider::WorkBuddy
    ) {
      let mut wal = locator.source_path.as_os_str().to_os_string();
      wal.push("-wal");
      paths.push(PathBuf::from(wal));
    }
    Self(
      paths
        .into_iter()
        .map(|path| {
          std::fs::metadata(path)
            .ok()
            .and_then(|meta| Some((meta.len(), meta.modified().ok()?)))
        })
        .collect(),
    )
  }
}

#[derive(Default)]
pub(super) struct CachePolicy {
  views: HashMap<String, ViewLease>,
  pending: HashMap<SessionLocator, Instant>,
  versions: HashMap<SessionLocator, SourceVersion>,
  attempts: HashMap<SessionLocator, Instant>,
}

impl CachePolicy {
  fn selected(&self, locator: &SessionLocator) -> bool {
    self.views.values().any(|view| view.selected.as_ref() == Some(locator))
  }

  pub(super) fn eligible(&self, locator: &SessionLocator) -> bool {
    self.views.values().any(|view| view.candidates.contains(locator))
  }

  fn expire(&mut self, now: Instant) {
    let previous_count = self.views.len();
    self
      .views
      .retain(|_, view| now.saturating_duration_since(view.updated) < VIEW_LEASE);
    if self.views.len() != previous_count {
      self.prune_candidates();
    }
  }

  fn prune_candidates(&mut self) {
    let eligible: HashSet<_> = self
      .views
      .values()
      .flat_map(|view| view.candidates.iter().cloned())
      .collect();
    self.pending.retain(|locator, _| eligible.contains(locator));
    self.versions.retain(|locator, _| eligible.contains(locator));
    self.attempts.retain(|locator, _| eligible.contains(locator));
  }

  pub(super) fn can_prefetch(&self, locator: &SessionLocator, now: Instant) -> bool {
    self
      .attempts
      .get(locator)
      .is_none_or(|attempted| now.saturating_duration_since(*attempted) >= PREFETCH_IDLE_PERIOD)
  }

  pub(super) fn record_prefetch(&mut self, locator: &SessionLocator, now: Instant) {
    self.attempts.insert(locator.clone(), now);
  }
}

impl State {
  pub(super) fn expire_views(&mut self, now: Instant) {
    self.cache.expire(now);
    let removed: Vec<_> = self
      .sessions
      .iter()
      .filter(|(key, session)| {
        session.priority == SessionPriority::Background && !self.cache.eligible(key) && !self.cache.selected(key)
      })
      .map(|(key, _)| key.clone())
      .collect();
    for locator in removed {
      self.evict(&locator);
    }
  }

  fn evict(&mut self, locator: &SessionLocator) {
    if let Some(session) = self.sessions.remove(locator) {
      session.cancel.cancel();
    }
  }

  fn victim(&self, protected: Option<&SessionLocator>) -> Option<SessionLocator> {
    self
      .sessions
      .iter()
      .filter(|(key, _)| protected != Some(*key) && !self.cache.selected(key))
      .min_by_key(|(_, session)| (session.priority, session.accessed))
      .map(|(key, _)| key.clone())
  }

  fn estimated_bytes(&self) -> usize {
    self
      .sessions
      .values()
      .map(|session| {
        let has_old_snapshot = session
          .loaded
          .as_ref()
          .zip(session.displayed.as_ref())
          .is_some_and(|(loaded, displayed)| !Arc::ptr_eq(loaded, displayed));
        session
          .estimated_bytes
          .saturating_mul(2)
          .saturating_add(if has_old_snapshot { session.displayed_bytes } else { 0 })
      })
      .fold(0, usize::saturating_add)
  }

  pub(super) fn enforce_budget(&mut self, protected: Option<&SessionLocator>) {
    while self.estimated_bytes() > MEMORY_TARGET_BYTES || self.sessions.len() > MAX_SESSIONS {
      let Some(victim) = self.victim(protected) else {
        break;
      };
      self.evict(&victim);
    }
  }

  pub(super) fn admit(&mut self, locator: &SessionLocator, priority: SessionPriority) -> bool {
    if priority == SessionPriority::Background {
      if self.estimated_bytes() >= MEMORY_TARGET_BYTES {
        return false;
      }
      let backgrounds = self
        .sessions
        .values()
        .filter(|s| s.priority == SessionPriority::Background)
        .count();
      if backgrounds >= MAX_BACKGROUND_SESSIONS || self.sessions.len() >= MAX_SESSIONS {
        let now = Instant::now();
        let victim = self
          .sessions
          .iter()
          .filter(|(key, session)| {
            *key != locator
              && session.priority == SessionPriority::Background
              && !self.cache.selected(key)
              && session.loaded.is_some()
              && now.saturating_duration_since(session.last_activity) >= PREFETCH_IDLE_PERIOD
          })
          .min_by_key(|(_, session)| session.last_activity)
          .map(|(key, _)| key.clone());
        let Some(victim) = victim else {
          return false;
        };
        self.evict(&victim);
      }
    } else if self.sessions.len() >= MAX_SESSIONS {
      let Some(victim) = self.victim(Some(locator)) else {
        return false;
      };
      self.evict(&victim);
    }
    true
  }
}

impl ViewerRelay {
  pub(crate) fn update_view(
    self: &Arc<Self>,
    view_id: &str,
    revision: u64,
    selected: Option<SessionLocator>,
    candidates: Vec<SessionLocator>,
  ) -> Result<(), String> {
    if view_id.is_empty() || view_id.len() > 128 || candidates.len() > 128 {
      return Err("Invalid session view lease".into());
    }
    let mut state = self.state.lock().unwrap();
    let now = Instant::now();
    state.expire_views(now);
    if let Some(view) = state.cache.views.get(view_id) {
      if revision <= view.revision {
        return Ok(());
      }
    } else if state.cache.views.len() >= MAX_VIEWS {
      return Err("Too many active viewer windows".into());
    }
    let newly_selected = selected
      .as_ref()
      .is_some_and(|locator| state.cache.views.get(view_id).and_then(|view| view.selected.as_ref()) != Some(locator));
    if newly_selected && let Some(session) = selected.as_ref().and_then(|locator| state.sessions.get_mut(locator)) {
      session.priority = SessionPriority::Explicit;
      session.accessed = now;
    }
    // Empty leases remain as short-lived tombstones so delayed heartbeats
    // cannot repin a view that was closed with a newer revision.
    state.cache.views.insert(
      view_id.to_owned(),
      ViewLease {
        revision,
        selected,
        candidates: candidates.into_iter().collect(),
        updated: now,
      },
    );
    state.cache.prune_candidates();
    state.expire_views(now);
    state.enforce_budget(None);
    drop(state);
    self.ready.notify_all();
    Ok(())
  }

  pub(crate) fn changed_source(&self, provider: ViewerProvider, path: &Path) {
    self.changed_session_source(provider, path, None);
  }

  pub(super) fn changed_session_source(&self, provider: ViewerProvider, path: &Path, session_id: Option<&str>) {
    let mut state = self.state.lock().unwrap();
    let now = Instant::now();
    state.expire_views(now);
    let candidates: HashSet<_> = state
      .cache
      .views
      .values()
      .flat_map(|view| view.candidates.iter())
      .filter(|locator| {
        locator.provider == provider
          && locator.source_path == path
          && session_id.is_none_or(|id| locator.session_id == id)
      })
      .cloned()
      .collect();
    for locator in candidates {
      if let Some(session) = state.sessions.get_mut(&locator) {
        session.last_activity = now;
      } else if state.cache.can_prefetch(&locator, now) {
        state.cache.pending.insert(locator, now);
      }
    }
    if !state.cache.pending.is_empty() {
      self.prefetch_wake.notify_one();
    }
  }

  pub(super) async fn cache_loop(self: Arc<Self>, epoch: u64, cancel: CancellationToken) {
    let mut maintenance = tokio::time::interval(Duration::from_secs(30));
    loop {
      tokio::select! {
        _ = cancel.cancelled() => return,
        _ = maintenance.tick() => {
          let mut state = self.state.lock().unwrap();
          if state.epoch != epoch { return; }
          state.expire_views(Instant::now());
          state.enforce_budget(None);
        }
        _ = self.prefetch_wake.notified() => {
          tokio::select! { _ = cancel.cancelled() => return, _ = tokio::time::sleep(PREFETCH_DEBOUNCE) => {} }
          let pending = {
            let mut state = self.state.lock().unwrap();
            if state.epoch != epoch { return; }
            state.expire_views(Instant::now());
            let mut pending: Vec<_> = state.cache.pending.drain().collect();
            pending.sort_by_key(|(_, changed)| std::cmp::Reverse(*changed));
            // Only the newest eligible changes can fill the two spare slots.
            pending.truncate(MAX_BACKGROUND_SESSIONS);
            pending.into_iter().map(|(locator, _)| locator).collect::<Vec<_>>()
          };
          for locator in pending {
            let manager = self.clone();
            tokio::task::spawn_blocking(move || manager.prefetch_changed(locator, epoch));
          }
        }
      }
    }
  }

  fn prefetch_changed(self: Arc<Self>, locator: SessionLocator, epoch: u64) {
    let version = SourceVersion::read(&locator);
    {
      let mut state = self.state.lock().unwrap();
      state.expire_views(Instant::now());
      if state.epoch != epoch
        || !state.cache.eligible(&locator)
        || state.sessions.contains_key(&locator)
        || !state.cache.can_prefetch(&locator, Instant::now())
        || state.cache.versions.get(&locator) == Some(&version)
      {
        return;
      }
    }
    if self.ensure_session(&locator, SessionPriority::Background, None).is_ok() {
      let mut state = self.state.lock().unwrap();
      if state.epoch == epoch && state.sessions.contains_key(&locator) {
        state.cache.versions.insert(locator, version);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::relay::{CachedSession, WindowInfo};

  fn locator(id: &str) -> SessionLocator {
    SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: id.into(),
      source_path: PathBuf::from(format!("/tmp/{id}.jsonl")),
    }
  }

  fn cached(priority: SessionPriority, accessed: Instant, estimated_bytes: usize) -> CachedSession {
    CachedSession {
      loaded: None,
      native: Vec::new(),
      displayed: None,
      displayed_native: Vec::new(),
      error: None,
      generation: String::new(),
      window: WindowInfo::default(),
      estimated_bytes,
      displayed_bytes: 0,
      identities: Vec::new(),
      cancel: CancellationToken::new(),
      priority,
      accessed,
      last_activity: accessed,
    }
  }

  fn settled_background(accessed: Instant) -> CachedSession {
    use tokn_session_core::{LoadedSession, SessionHistoryStatus, SessionRef};
    let mut session = cached(SessionPriority::Background, accessed, 1);
    session.loaded = Some(Arc::new(LoadedSession {
      reference: SessionRef {
        id: "cached".into(),
        parent_session_id: None,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        title: None,
        preview: None,
        path: PathBuf::from("/tmp/cached.jsonl"),
        cwd: None,
        timestamp: None,
        message_count: 0,
      },
      events: Vec::new(),
      history_status: SessionHistoryStatus::Complete,
    }));
    session
  }

  #[test]
  fn explicit_access_evicts_background_before_older_user_history_and_cancels_it() {
    let manager = ViewerRelay::new();
    let mut state = manager.state.lock().unwrap();
    let now = Instant::now();
    for id in 0..7 {
      state.sessions.insert(
        locator(&id.to_string()),
        cached(SessionPriority::Explicit, now - Duration::from_secs(10), 1),
      );
    }
    let background = locator("background");
    let old = cached(SessionPriority::Background, now, 1);
    let cancellation = old.cancel.clone();
    state.sessions.insert(background.clone(), old);
    assert!(state.admit(&locator("next"), SessionPriority::Explicit));
    assert!(!state.sessions.contains_key(&background));
    assert!(cancellation.is_cancelled());
    assert_eq!(state.sessions.len(), 7);
  }

  #[test]
  fn background_cannot_displace_opened_sessions_and_has_two_slots() {
    let manager = ViewerRelay::new();
    let mut state = manager.state.lock().unwrap();
    let now = Instant::now();
    for id in 0..8 {
      state
        .sessions
        .insert(locator(&id.to_string()), cached(SessionPriority::Explicit, now, 1));
    }
    assert!(!state.admit(&locator("new"), SessionPriority::Background));
    state.sessions.clear();
    for id in 0..2 {
      state.sessions.insert(
        locator(&id.to_string()),
        settled_background(now - PREFETCH_IDLE_PERIOD - Duration::from_secs(2 - id)),
      );
    }
    assert!(state.admit(&locator("new"), SessionPriority::Background));
    assert!(!state.sessions.contains_key(&locator("0")));
    assert!(state.sessions.contains_key(&locator("1")));
  }

  #[test]
  fn streaming_candidates_keep_background_readers_instead_of_rotating_cold_loads() {
    let manager = ViewerRelay::new();
    let targets: Vec<_> = (0..3).map(|id| locator(&id.to_string())).collect();
    manager.update_view("view", 1, None, targets.clone()).unwrap();
    let old = Instant::now() - PREFETCH_IDLE_PERIOD - Duration::from_secs(1);
    {
      let mut state = manager.state.lock().unwrap();
      state.sessions.insert(targets[0].clone(), settled_background(old));
      // Even an unusually slow initial read must not be evicted by another preload.
      state
        .sessions
        .insert(targets[1].clone(), cached(SessionPriority::Background, old, 1));
    }
    for _ in 0..100 {
      for target in &targets {
        manager.changed_source(target.provider, &target.source_path);
      }
      let mut state = manager.state.lock().unwrap();
      assert!(!state.admit(&targets[2], SessionPriority::Background));
      assert_eq!(state.sessions.len(), 2);
      assert_eq!(state.sessions[&targets[0]].accessed, old);
    }
    let mut state = manager.state.lock().unwrap();
    state.sessions.get_mut(&targets[0]).unwrap().last_activity = old;
    assert!(state.admit(&targets[2], SessionPriority::Background));
    assert!(
      !state.sessions.contains_key(&targets[0]),
      "settled idle preloads can be replaced"
    );
    assert!(
      state.sessions.contains_key(&targets[1]),
      "in-flight reader stays resident"
    );
  }

  #[test]
  fn evicted_preload_attempts_cool_down_and_expire_with_the_view_scope() {
    let manager = ViewerRelay::new();
    let target = locator("busy");
    manager.update_view("view", 1, None, vec![target.clone()]).unwrap();
    let now = Instant::now();
    {
      let mut state = manager.state.lock().unwrap();
      state.cache.record_prefetch(&target, now);
      assert!(!state.cache.can_prefetch(&target, now));
      assert!(state.cache.can_prefetch(&target, now + PREFETCH_IDLE_PERIOD));
      assert!(
        state.admit(&target, SessionPriority::Explicit),
        "explicit opening bypasses cooldown"
      );
    }
    manager.changed_source(target.provider, &target.source_path);
    assert!(manager.state.lock().unwrap().cache.pending.is_empty());
    manager.update_view("view", 2, None, vec![]).unwrap();
    assert!(manager.state.lock().unwrap().cache.attempts.is_empty());
  }

  #[test]
  fn selected_views_are_pinned_until_release_or_lease_expiry() {
    let manager = ViewerRelay::new();
    let selected = locator("selected");
    manager
      .update_view("one", 1, Some(selected.clone()), vec![selected.clone()])
      .unwrap();
    manager.update_view("two", 1, Some(selected.clone()), vec![]).unwrap();
    {
      let mut state = manager.state.lock().unwrap();
      state.sessions.insert(
        selected.clone(),
        cached(SessionPriority::Explicit, Instant::now(), MEMORY_TARGET_BYTES + 1),
      );
      state.enforce_budget(None);
      assert!(
        state.sessions.contains_key(&selected),
        "selected history has a soft byte target"
      );
    }
    manager.update_view("one", 2, None, vec![]).unwrap();
    manager
      .update_view("one", 1, Some(selected.clone()), vec![selected.clone()])
      .unwrap();
    {
      let mut state = manager.state.lock().unwrap();
      assert!(
        state.cache.views["one"].selected.is_none(),
        "stale heartbeat must not reverse a release"
      );
      state.enforce_budget(None);
      assert!(
        state.sessions.contains_key(&selected),
        "another live view still pins it"
      );
      state.expire_views(Instant::now() + VIEW_LEASE);
      state.enforce_budget(None);
      assert!(state.sessions.is_empty());
    }
  }

  #[test]
  fn byte_pressure_evicts_unselected_lru_without_waiting_for_count_limit() {
    let manager = ViewerRelay::new();
    let mut state = manager.state.lock().unwrap();
    let now = Instant::now();
    state.sessions.insert(
      locator("old"),
      cached(
        SessionPriority::Explicit,
        now - Duration::from_secs(1),
        MEMORY_TARGET_BYTES,
      ),
    );
    state
      .sessions
      .insert(locator("recent"), cached(SessionPriority::Explicit, now, 1));
    state.enforce_budget(None);
    assert!(!state.sessions.contains_key(&locator("old")));
    assert!(state.sessions.contains_key(&locator("recent")));
  }

  #[test]
  fn changes_are_scoped_debounced_and_do_not_touch_user_recency() {
    let manager = ViewerRelay::new();
    let opened = locator("opened");
    let cold = locator("cold");
    manager
      .update_view("view", 1, Some(opened.clone()), vec![opened.clone(), cold.clone()])
      .unwrap();
    let accessed = Instant::now() - Duration::from_secs(10);
    manager
      .state
      .lock()
      .unwrap()
      .sessions
      .insert(opened.clone(), cached(SessionPriority::Explicit, accessed, 1));
    for target in [&opened, &cold, &locator("hidden")] {
      manager.changed_source(target.provider, &target.source_path);
      manager.changed_source(target.provider, &target.source_path);
    }
    let state = manager.state.lock().unwrap();
    assert_eq!(state.cache.pending.len(), 1);
    assert!(state.cache.pending.contains_key(&cold));
    assert_eq!(state.sessions[&opened].accessed, accessed);
  }

  #[test]
  fn changing_view_drops_ineligible_preloads_but_preserves_opened_history() {
    let manager = ViewerRelay::new();
    let old = locator("old");
    manager.update_view("view", 1, None, vec![old.clone()]).unwrap();
    let opened = locator("opened");
    {
      let mut state = manager.state.lock().unwrap();
      state
        .sessions
        .insert(old.clone(), cached(SessionPriority::Background, Instant::now(), 1));
      state
        .sessions
        .insert(opened.clone(), cached(SessionPriority::Explicit, Instant::now(), 1));
    }
    manager.update_view("view", 2, None, vec![locator("new")]).unwrap();
    let state = manager.state.lock().unwrap();
    assert!(!state.sessions.contains_key(&old));
    assert!(state.sessions.contains_key(&opened));
  }

  #[test]
  fn source_versions_include_wal_updates_without_reading_contents() {
    let directory = tempfile::tempdir().unwrap();
    let mut target = locator("database");
    target.provider = ViewerProvider::OpenCode;
    target.source_path = directory.path().join("sessions.db");
    std::fs::write(&target.source_path, "header").unwrap();
    let before = SourceVersion::read(&target);
    assert_eq!(before, SourceVersion::read(&target));
    std::fs::write(directory.path().join("sessions.db-wal"), "new transaction").unwrap();
    assert_ne!(before, SourceVersion::read(&target));
  }

  async fn embedded_manager(root: &Path) -> Arc<ViewerRelay> {
    use crate::{
      service_client::{Connection, load_catalog_from},
      service_server::Service,
    };
    use tokn_session_relay::{ProviderRoot, RelayConfig};
    let mut config = RelayConfig::new(vec![ProviderRoot::new(tokn_session_core::Provider::Pi, root.into())]);
    config.poll_interval = Duration::from_millis(20);
    config.include_native = true;
    let connection = Connection::Embedded(Service::new(config).unwrap());
    let catalog = load_catalog_from(&connection).await.unwrap();
    let manager = ViewerRelay::new();
    {
      let mut state = manager.state.lock().unwrap();
      state.connection = Some(connection);
      state.active_endpoint = Some("embedded".into());
      state.entries = Some(catalog.entries);
      state.providers = vec![ViewerProvider::Pi];
      state.settings.mode = crate::relay::RelayMode::External;
    }
    manager
  }

  fn write_turns(path: &Path, id: &str, count: usize) {
    let mut text = format!(r#"{{"type":"session","id":"{id}","timestamp":"2026-01-01","cwd":"/tmp"}}"#);
    text.push('\n');
    for turn in 0..count {
      text.push_str(&format!("{{\"type\":\"message\",\"id\":\"user-{turn}\",\"message\":{{\"role\":\"user\",\"content\":\"question {turn}\"}}}}\n"));
      text.push_str(&format!("{{\"type\":\"message\",\"id\":\"assistant-{turn}\",\"message\":{{\"role\":\"assistant\",\"content\":\"reply {turn}\"}}}}\n"));
    }
    std::fs::write(path, text).unwrap();
  }

  fn user_count(loaded: &tokn_session_core::LoadedSession) -> usize {
    loaded.events.iter().filter(|event| matches!(event, tokn_session_core::AgentEvent::Message(message) if message.role == tokn_session_core::Role::User)).count()
  }

  #[tokio::test]
  async fn expanded_history_survives_appends_reopens_and_resets_only_after_eviction() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.jsonl");
    write_turns(&path, "history", 7);
    let manager = embedded_manager(root.path()).await;
    let mut target = locator("history");
    target.source_path = path.clone();
    manager.update_view("view", 1, Some(target.clone()), vec![]).unwrap();
    let loader = manager.clone();
    let key = target.clone();
    let initial = tokio::task::spawn_blocking(move || loader.load(&key))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(user_count(&initial), 3);
    let initial_window = manager.window_info(&target, &initial).unwrap();
    assert!(initial_window.has_earlier);
    let loader = manager.clone();
    let key = target.clone();
    let expanded = tokio::task::spawn_blocking(move || loader.load_earlier(&key, initial_window.event_offset))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(user_count(&expanded), 6);
    let expanded_window = manager.window_info(&target, &expanded).unwrap();
    assert_eq!(expanded_window.generation, initial_window.generation);
    assert!(expanded_window.event_offset < initial_window.event_offset);
    assert!(manager.native(&target, 0, &expanded).is_some());
    assert!(manager.native(&target, 0, &initial).is_none());
    assert_eq!(
      manager.window_info(&target, &initial).unwrap().event_offset,
      initial_window.event_offset,
      "concurrent requests retain old identity offsets without keeping history alive"
    );
    assert!(Arc::ptr_eq(&expanded, &manager.load(&target).unwrap()));
    let mut changes = manager.changes.subscribe();
    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(
        b"{\"type\":\"message\",\"id\":\"next\",\"message\":{\"role\":\"user\",\"content\":\"next question\"}}\n",
      )
      .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
      loop {
        changes.recv().await.unwrap();
        if manager.state.lock().unwrap().sessions[&target]
          .loaded
          .as_ref()
          .is_some_and(|loaded| user_count(loaded) == 7)
        {
          break;
        }
      }
    })
    .await
    .unwrap();
    let advanced = manager.advance(&target).unwrap();
    assert_eq!(user_count(&advanced), 7);
    assert_eq!(
      manager.window_info(&target, &advanced).unwrap().event_offset,
      expanded_window.event_offset
    );
    // Unloading owns cancellation; retained UI snapshots do not keep the reader
    // or subscription alive. A new admission starts at the latest three turns.
    manager.state.lock().unwrap().evict(&target);
    let loader = manager.clone();
    let key = target.clone();
    let reloaded = tokio::task::spawn_blocking(move || loader.load(&key))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(user_count(&reloaded), 3);
    manager.shutdown().await;
  }

  #[tokio::test]
  async fn reconnect_resumes_a_resident_preload_during_cooldown_and_retains_its_window() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("reconnect.jsonl");
    write_turns(&path, "reconnect", 7);
    let manager = embedded_manager(root.path()).await;
    let mut target = locator("reconnect");
    target.source_path = path.clone();
    manager.update_view("view", 1, None, vec![target.clone()]).unwrap();
    let mut changes = manager.changes.subscribe();
    manager
      .ensure_session(&target, SessionPriority::Background, None)
      .unwrap();
    tokio::time::timeout(Duration::from_secs(3), changes.recv())
      .await
      .unwrap()
      .unwrap();
    let (initial, offset) = {
      let mut state = manager.state.lock().unwrap();
      let session = &state.sessions[&target];
      let initial = session.loaded.clone().unwrap();
      let offset = session.window.event_offset;
      assert_eq!(user_count(&initial), 3);
      assert!(offset > 0);
      assert!(!state.cache.can_prefetch(&target, Instant::now()));
      state.connection_cancel.cancel();
      state.connection_cancel = state.cancel.child_token();
      (initial, offset)
    };
    std::fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(b"{\"type\":\"message\",\"id\":\"after-reconnect\",\"message\":{\"role\":\"user\",\"content\":\"next question\"}}\n")
      .unwrap();
    manager
      .ensure_session(&target, SessionPriority::Background, None)
      .unwrap();
    {
      let state = manager.state.lock().unwrap();
      let session = &state.sessions[&target];
      assert!(
        !session.cancel.is_cancelled(),
        "resident preload resumes before cooldown expires"
      );
      assert!(Arc::ptr_eq(session.loaded.as_ref().unwrap(), &initial));
      assert_eq!(session.window.event_offset, offset);
      assert_eq!(session.priority, SessionPriority::Background);
      assert!(session.displayed.is_none());
    }
    tokio::time::timeout(Duration::from_secs(3), async {
      loop {
        changes.recv().await.unwrap();
        let state = manager.state.lock().unwrap();
        let session = &state.sessions[&target];
        if user_count(session.loaded.as_ref().unwrap()) == 4 {
          assert_eq!(session.window.event_offset, offset);
          assert!(session.displayed.is_none());
          break;
        }
      }
    })
    .await
    .unwrap();
    manager.shutdown().await;
  }

  #[tokio::test]
  async fn changed_candidates_prefetch_without_becoming_displayed_or_user_accessed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("active.jsonl");
    write_turns(&path, "active", 5);
    let manager = embedded_manager(root.path()).await;
    let mut target = locator("active");
    target.source_path = path.clone();
    manager.update_view("view", 1, None, vec![target.clone()]).unwrap();
    let cancel = manager.state.lock().unwrap().cancel.clone();
    let worker = tokio::spawn(manager.clone().cache_loop(0, cancel));
    manager.changed_source(target.provider, &path);
    manager.changed_source(target.provider, &path);
    let mut changes = manager.changes.subscribe();
    tokio::time::timeout(Duration::from_secs(3), async {
      loop {
        changes.recv().await.unwrap();
        if manager
          .state
          .lock()
          .unwrap()
          .sessions
          .get(&target)
          .is_some_and(|session| session.loaded.is_some())
        {
          break;
        }
      }
    })
    .await
    .unwrap();
    {
      let mut state = manager.state.lock().unwrap();
      let session = &state.sessions[&target];
      assert_eq!(session.priority, SessionPriority::Background);
      assert!(session.displayed.is_none());
      assert_eq!(user_count(session.loaded.as_ref().unwrap()), 3);
      assert_eq!(state.cache.versions.len(), 1);
      state.evict(&target);
    }
    // Duplicated feed frames after eviction cannot repeatedly load an unchanged file.
    let duplicate = manager.clone();
    let key = target.clone();
    tokio::task::spawn_blocking(move || duplicate.prefetch_changed(key, 0))
      .await
      .unwrap();
    assert!(!manager.state.lock().unwrap().sessions.contains_key(&target));
    manager.shutdown().await;
    worker.await.unwrap();
  }
}
