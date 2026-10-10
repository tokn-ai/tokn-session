//! Viewer Relay adapter. Covered providers never fall back to local
//! body reads while a Relay catalog is authoritative, including reconnects.
mod cache;
mod managed;
mod settings;
pub use settings::{RelayMode, RelaySettings};
use std::{
  collections::HashMap,
  path::PathBuf,
  sync::{Arc, Condvar, Mutex, Weak},
  time::{Duration, Instant},
};

use crate::{
  service_client::{Connection, RelaySubscription, load_catalog_from},
  service_protocol::CatalogEntry,
};
use cache::{CachePolicy, SessionPriority};
use serde::Serialize;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;
use tokn_session_core::{LoadedSession, Provider, SessionHeader};

use crate::model::{SessionLocator, ViewerProvider, encode_session_key};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RelayStatus {
  pub settings: RelaySettings,
  pub active_endpoint: Option<String>,
  pub phase: String,
  pub native: bool,
  pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RelayChange {
  pub session_key: Option<String>,
  pub reset: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WindowInfo {
  pub event_offset: usize,
  pub has_earlier: bool,
  pub generation: String,
}

struct PreviousSubscription(Option<CancellationToken>);

impl Drop for PreviousSubscription {
  fn drop(&mut self) {
    if let Some(cancel) = self.0.take() {
      cancel.cancel();
    }
  }
}

struct SnapshotIdentity {
  loaded: Weak<LoadedSession>,
  window: WindowInfo,
}

struct CachedSession {
  loaded: Option<Arc<LoadedSession>>,
  native: Vec<Option<Arc<serde_json::Value>>>,
  displayed: Option<Arc<LoadedSession>>,
  displayed_native: Vec<Option<Arc<serde_json::Value>>>,
  error: Option<String>,
  generation: String,
  window: WindowInfo,
  estimated_bytes: usize,
  displayed_bytes: usize,
  identities: Vec<SnapshotIdentity>,
  cancel: CancellationToken,
  priority: SessionPriority,
  accessed: Instant,
  last_activity: Instant,
}

struct State {
  settings: RelaySettings,
  phase: String,
  error: Option<String>,
  native: bool,
  epoch: u64,
  cancel: CancellationToken,
  connection_cancel: CancellationToken,
  active_endpoint: Option<String>,
  connection: Option<Connection>,
  providers: Vec<ViewerProvider>,
  entries: Option<Vec<CatalogEntry>>,
  sessions: HashMap<SessionLocator, CachedSession>,
  cache: CachePolicy,
}

pub struct ViewerRelay {
  index: Option<Arc<tokn_session_index::SessionIndex>>,
  pub(crate) index_wakes: broadcast::Sender<(ViewerProvider, PathBuf)>,
  pub configure_lock: tokio::sync::Mutex<()>,
  managed_lock: tokio::sync::Mutex<()>,
  state: Mutex<State>,
  ready: Condvar,
  prefetch_wake: Notify,
  pub changes: broadcast::Sender<RelayChange>,
}

impl ViewerRelay {
  pub fn new() -> Arc<Self> {
    Self::with_index(None)
  }

  pub(crate) fn with_index(index: Option<Arc<tokn_session_index::SessionIndex>>) -> Arc<Self> {
    Arc::new(Self {
      index,
      index_wakes: broadcast::channel(256).0,
      configure_lock: tokio::sync::Mutex::new(()),
      managed_lock: tokio::sync::Mutex::new(()),
      state: Mutex::new(State {
        settings: RelaySettings::default(),
        phase: "starting".into(),
        error: None,
        native: false,
        epoch: 0,
        cancel: CancellationToken::new(),
        connection_cancel: CancellationToken::new(),
        active_endpoint: None,
        connection: None,
        providers: Vec::new(),
        entries: None,
        sessions: HashMap::new(),
        cache: CachePolicy::default(),
      }),
      ready: Condvar::new(),
      prefetch_wake: Notify::new(),
      changes: broadcast::channel(128).0,
    })
  }

  pub fn status(&self) -> RelayStatus {
    let state = self.state.lock().unwrap();
    let error = state.error.clone();
    RelayStatus {
      settings: state.settings.clone(),
      active_endpoint: state.active_endpoint.clone(),
      phase: if error.is_some() && state.phase == "live" {
        "reconnecting".into()
      } else {
        state.phase.clone()
      },
      native: state.native,
      error,
    }
  }

  pub fn configure(self: &Arc<Self>, settings: RelaySettings) -> Result<(), String> {
    settings.validate()?;
    // Prepare the core reader before publishing Automatic mode. No provider
    // history is read here, and feed startup must not gate snapshot requests.
    let snapshots = if settings.mode != RelayMode::External && self.index.is_some() {
      Some(self.snapshot_service(settings.include_native)?)
    } else {
      None
    };
    let (epoch, cancel, reset) = {
      let mut state = self.state.lock().unwrap();
      state.cancel.cancel();
      state.connection_cancel.cancel();
      for session in state.sessions.values() {
        session.cancel.cancel();
      }
      // Explicit source changes clear snapshots. Child crash/restart does not.
      let reset = state.settings != settings || state.entries.is_none();
      if reset || settings.mode == RelayMode::Local {
        state.entries = None;
        state.providers.clear();
        state.sessions.clear();
      }
      if (settings.mode != RelayMode::Local || snapshots.is_some()) && state.entries.is_none() {
        state.providers = tokn_session_relay::PROVIDERS
          .into_iter()
          .filter_map(viewer_provider)
          .collect();
      }
      if settings.mode == RelayMode::Local && snapshots.is_none() {
        state.providers.clear();
      }
      state.epoch += 1;
      state.cancel = CancellationToken::new();
      state.connection_cancel = state.cancel.child_token();
      state.active_endpoint = snapshots.as_ref().map(|_| "embedded".into());
      state.connection = snapshots.map(Connection::Embedded);
      state.native = state.connection.is_some() && settings.include_native;
      state.settings = settings.clone();
      state.phase = match settings.mode {
        RelayMode::Automatic if state.connection.is_some() => "live",
        RelayMode::Automatic => "starting",
        RelayMode::External => "connecting",
        RelayMode::Local => "local",
      }
      .into();
      state.error = None;
      self.ready.notify_all();
      (state.epoch, state.cancel.clone(), reset)
    };
    self.notify(None, reset);
    let maintenance = self.clone();
    let maintenance_cancel = cancel.clone();
    tokio::spawn(async move {
      maintenance.cache_loop(epoch, maintenance_cancel).await;
    });
    match settings.mode {
      RelayMode::Automatic if self.index.is_some() => {}
      RelayMode::Automatic => {
        let manager = self.clone();
        tokio::task::spawn(async move {
          manager.run_managed(epoch, cancel, settings.include_native).await;
        });
      }
      RelayMode::External => self.connect_endpoint(settings.endpoint, epoch),
      RelayMode::Local => {}
    }
    Ok(())
  }

  /// The indexed viewer already owns source readers and native file watches.
  /// Wake those readers directly rather than parsing a second live-feed copy.
  pub(crate) async fn wake_sources(&self) {
    let service = {
      let state = self.state.lock().unwrap();
      match &state.connection {
        Some(Connection::Embedded(service)) => Some(service.clone()),
        _ => None,
      }
    };
    if let Some(service) = service {
      service.invalidate().await;
    }
  }

  pub fn configuration_failed(&self, error: String) {
    {
      let mut state = self.state.lock().unwrap();
      state.phase = "failed".into();
      state.error = Some(error);
      // Never silently read a different source when saved settings are invalid.
      state.providers = tokn_session_relay::PROVIDERS
        .into_iter()
        .filter_map(viewer_provider)
        .collect();
    }
    self.notify(None, false);
  }

  fn connect_endpoint(self: &Arc<Self>, endpoint: String, epoch: u64) {
    self.connect_source(Connection::Tcp(endpoint.clone()), endpoint, epoch);
  }

  fn connect_source(self: &Arc<Self>, connection: Connection, endpoint: String, epoch: u64) {
    let cancel = {
      let mut state = self.state.lock().unwrap();
      if state.epoch != epoch || state.cancel.is_cancelled() {
        return;
      }
      state.connection_cancel.cancel();
      state.connection_cancel = state.cancel.child_token();
      state.active_endpoint = Some(endpoint.clone());
      state.connection = Some(connection.clone());
      state.phase = "connecting".into();
      state.error = None;
      state.connection_cancel.clone()
    };
    let manager = self.clone();
    tokio::task::spawn(async move {
      manager.catalog_loop(connection, epoch, cancel).await;
    });
    self.notify(None, false);
  }

  async fn catalog_loop(self: Arc<Self>, connection: Connection, epoch: u64, cancel: CancellationToken) {
    loop {
      let result = tokio::select! {
        _ = cancel.cancelled() => return,
        result = load_catalog_from(&connection) => result,
      };
      let mut changed_sources = Vec::new();
      let (changed, first_catalog, resume) = {
        let mut state = self.state.lock().unwrap();
        if state.epoch != epoch || cancel.is_cancelled() {
          return;
        }
        let before = serde_json::to_vec(&state.entries).ok();
        let had_catalog = state.entries.is_some();
        let was_live = state.phase == "live";
        let old_error = state.error.clone();
        match result {
          Ok(catalog) => {
            state.providers = catalog.providers.iter().filter_map(|p| viewer_provider(*p)).collect();
            state.native = catalog.native;
            // Catalog failures keep the last complete catalog rather than
            // presenting partial discovery as authoritative deletion.
            if catalog.warnings.is_empty() || state.entries.is_none() {
              if let Some(previous) = &state.entries {
                let previous: HashMap<_, _> = previous.iter().map(|entry| (&entry.key, entry)).collect();
                changed_sources.extend(
                  catalog
                    .entries
                    .iter()
                    .filter(|entry| {
                      previous.get(&entry.key).is_none_or(|old| {
                        old.header.updated_at != entry.header.updated_at
                          || old.header.updated_at_ms != entry.header.updated_at_ms
                          || old.header.path != entry.header.path
                      })
                    })
                    .filter_map(|entry| {
                      viewer_provider(entry.provider)
                        .map(|provider| (provider, entry.header.path.clone(), entry.header.id.clone()))
                    }),
                );
              }
              state.entries = Some(catalog.entries);
            }
            state.error = (!catalog.warnings.is_empty()).then(|| catalog.warnings.join("; "));
            state.phase = "live".into();
          }
          Err(error) => {
            state.phase = "reconnecting".into();
            state.error = Some(error);
          }
        }
        let became_live = !was_live && state.phase == "live";
        let resume: Vec<_> = if became_live {
          state
            .sessions
            .iter()
            .filter(|(_, s)| s.cancel.is_cancelled())
            .map(|(key, _)| key.clone())
            .collect()
        } else {
          Vec::new()
        };
        (
          before != serde_json::to_vec(&state.entries).ok() || became_live || old_error != state.error,
          !had_catalog && state.entries.is_some(),
          resume,
        )
      };
      self.ready.notify_all();
      for (provider, path, session_id) in changed_sources {
        self.changed_session_source(provider, &path, Some(&session_id));
      }
      if changed {
        self.notify(None, first_catalog);
      }
      for locator in resume {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
          let priority = manager.state.lock().unwrap().sessions.get(&locator).map(|s| s.priority);
          if let Some(priority) = priority {
            let _ = manager.ensure_session(&locator, priority, None);
          }
        });
      }
      tokio::select! { _ = cancel.cancelled() => return, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
    }
  }

  pub fn covers(&self, provider: ViewerProvider) -> bool {
    self.state.lock().unwrap().providers.contains(&provider)
  }

  pub(crate) fn external_catalog_covers(&self, provider: ViewerProvider) -> bool {
    let state = self.state.lock().unwrap();
    state.settings.mode == RelayMode::External && state.providers.contains(&provider)
  }

  pub fn has_catalog(&self) -> bool {
    self.state.lock().unwrap().entries.is_some()
  }

  pub fn headers(&self, provider: ViewerProvider) -> Vec<SessionHeader> {
    self
      .state
      .lock()
      .unwrap()
      .entries
      .as_ref()
      .into_iter()
      .flatten()
      .filter(|entry| viewer_provider(entry.provider) == Some(provider))
      .map(|entry| entry.header.clone())
      .collect()
  }

  fn ensure_session(
    self: &Arc<Self>,
    locator: &SessionLocator,
    priority: SessionPriority,
    before_event: Option<usize>,
  ) -> Result<(), String> {
    let mut state = self.state.lock().unwrap();
    state.expire_views(Instant::now());
    if priority == SessionPriority::Explicit
      && let Some(session) = state.sessions.get_mut(locator)
    {
      session.priority = priority;
      session.accessed = Instant::now();
    }
    let should_start = state.connection.is_some()
      && (before_event.is_some() || state.sessions.get(locator).is_none_or(|s| s.cancel.is_cancelled()));
    if !should_start {
      return Ok(());
    }
    if priority == SessionPriority::Background
      && (!state.cache.eligible(locator)
        || (!state.sessions.contains_key(locator) && !state.cache.can_prefetch(locator, Instant::now())))
    {
      return Ok(());
    }
    let entry =
      if let (RelayMode::Automatic | RelayMode::Local, Some(index)) = (state.settings.mode, self.index.as_ref()) {
        crate::index_queries::snapshot_entry(index, locator)?
      } else {
        state
          .entries
          .as_ref()
          .into_iter()
          .flatten()
          .find(|entry| matches_locator(entry, locator))
          .cloned()
      }
      .ok_or("Session is no longer in the index")?;
    if !state.sessions.contains_key(locator) && !state.admit(locator, priority) {
      return if priority == SessionPriority::Background {
        Ok(())
      } else {
        Err("All cached sessions are currently open; close a viewer before opening another session".into())
      };
    }
    let cancel = state.connection_cancel.child_token();
    let old = state.sessions.remove(locator);
    let retain_from = old
      .as_ref()
      .and_then(|session| session.loaded.as_ref().map(|_| session.window.event_offset));
    let previous_cancel = old.as_ref().map(|s| s.cancel.clone());
    let mut session = old.unwrap_or_else(|| CachedSession {
      loaded: None,
      native: Vec::new(),
      displayed: None,
      displayed_native: Vec::new(),
      error: None,
      generation: String::new(),
      window: WindowInfo::default(),
      estimated_bytes: 0,
      displayed_bytes: 0,
      identities: Vec::new(),
      cancel: cancel.clone(),
      priority,
      accessed: Instant::now(),
      last_activity: Instant::now(),
    });
    session.cancel = cancel.clone();
    session.error = None;
    if priority == SessionPriority::Explicit {
      session.priority = priority;
      session.accessed = Instant::now();
    }
    state.sessions.insert(locator.clone(), session);
    if priority == SessionPriority::Background {
      state.cache.record_prefetch(locator, Instant::now());
    }
    let manager = self.clone();
    let connection = state.connection.clone().expect("active connection checked above");
    let epoch = state.epoch;
    let locator = locator.clone();
    tokio::task::spawn(async move {
      manager
        .session_loop(
          connection,
          entry.key,
          locator,
          epoch,
          cancel,
          retain_from,
          before_event,
          previous_cancel,
        )
        .await;
    });
    Ok(())
  }

  pub(crate) fn load(self: &Arc<Self>, locator: &SessionLocator) -> Result<Arc<LoadedSession>, String> {
    self.ensure_session(locator, SessionPriority::Explicit, None)?;
    self.wait_for_snapshot(locator, None)
  }

  fn wait_for_snapshot(
    &self,
    locator: &SessionLocator,
    before_event: Option<usize>,
  ) -> Result<Arc<LoadedSession>, String> {
    let mut state = self.state.lock().unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    let epoch = state.epoch;
    loop {
      if state.epoch != epoch {
        return Err("Relay connection changed; reload the session".into());
      }
      let unavailable = state
        .error
        .clone()
        .unwrap_or_else(|| "Waiting for Relay; retry once it is live".into());
      let session = state.sessions.get_mut(locator).ok_or_else(|| unavailable.clone())?;
      let expanded = before_event.is_some_and(|before| {
        session.loaded.is_some() && (session.window.event_offset < before || !session.window.has_earlier)
      });
      if session.displayed.is_none() || expanded {
        session.displayed = session.loaded.clone();
        session.displayed_native = session.native.clone();
        session.displayed_bytes = session.estimated_bytes;
      }
      if (before_event.is_none() || expanded)
        && let Some(loaded) = &session.displayed
      {
        return Ok(loaded.clone());
      }
      if let Some(error) = &session.error {
        return Err(error.clone());
      }
      if session.cancel.is_cancelled() {
        return Err(unavailable);
      }
      let remaining = deadline.saturating_duration_since(Instant::now());
      if remaining.is_zero() {
        return Err("Waiting for Relay snapshot; retry when connected".into());
      }
      state = self.ready.wait_timeout(state, remaining).unwrap().0;
    }
  }

  pub(crate) fn load_earlier(
    self: &Arc<Self>,
    locator: &SessionLocator,
    before_event: usize,
  ) -> Result<Arc<LoadedSession>, String> {
    self.load(locator)?;
    {
      let state = self.state.lock().unwrap();
      let session = state.sessions.get(locator).ok_or("Session unloaded; reopen it")?;
      if !session.window.has_earlier || session.window.event_offset < before_event {
        drop(state);
        return self.advance(locator);
      }
    }
    self.ensure_session(locator, SessionPriority::Explicit, Some(before_event))?;
    self.wait_for_snapshot(locator, Some(before_event))
  }

  pub(crate) fn window_info(&self, locator: &SessionLocator, loaded: &Arc<LoadedSession>) -> Option<WindowInfo> {
    let state = self.state.lock().ok()?;
    let session = state.sessions.get(locator)?;
    session.identities.iter().find_map(|identity| {
      identity
        .loaded
        .upgrade()
        .filter(|image| Arc::ptr_eq(image, loaded))
        .map(|_| identity.window.clone())
    })
  }

  /// The last successful snapshot remains readable while its follower retries.
  /// Expose the current failure with that page so reconnects cannot miss it.
  pub(crate) fn follow_error(&self, locator: &SessionLocator) -> Option<String> {
    self.state.lock().ok()?.sessions.get(locator)?.error.clone()
  }

  async fn session_loop(
    self: Arc<Self>,
    connection: Connection,
    key: String,
    locator: SessionLocator,
    epoch: u64,
    cancel: CancellationToken,
    retain_from: Option<usize>,
    before_event: Option<usize>,
    previous_cancel: Option<CancellationToken>,
  ) {
    let mut previous_cancel = PreviousSubscription(previous_cancel);
    loop {
      let result = tokio::select! {
        _ = cancel.cancelled() => return,
        result = self.consume_session(&connection, &key, &locator, epoch, &cancel, retain_from, before_event, &mut previous_cancel.0) => result,
      };
      if let Err(error) = result {
        let mut state = self.state.lock().unwrap();
        if state.epoch != epoch || cancel.is_cancelled() {
          return;
        }
        if state
          .sessions
          .get(&locator)
          .is_some_and(|session| session.priority == SessionPriority::Background)
        {
          // Advisory preloads must not turn a corrupt background file into a
          // retry loop or a global connection error. A changed source may retry.
          state.sessions.remove(&locator);
          cancel.cancel();
          self.ready.notify_all();
          return;
        }
        let changed = if let Some(session) = state.sessions.get_mut(&locator) {
          let changed = session.error.as_deref() != Some(error.as_str());
          session.error = Some(error);
          changed
        } else {
          false
        };
        drop(state);
        self.ready.notify_all();
        if changed {
          self.notify(Some(&locator), false);
        }
      }
      tokio::select! { _ = cancel.cancelled() => return, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
    }
  }

  async fn consume_session(
    &self,
    connection: &Connection,
    key: &str,
    locator: &SessionLocator,
    epoch: u64,
    cancel: &CancellationToken,
    retain_from: Option<usize>,
    before_event: Option<usize>,
    previous_cancel: &mut Option<CancellationToken>,
  ) -> Result<(), String> {
    let retain_from = self
      .state
      .lock()
      .unwrap()
      .sessions
      .get(locator)
      .and_then(|s| s.loaded.as_ref().map(|_| s.window.event_offset))
      .or(retain_from);
    let mut subscription = RelaySubscription::connect_window_from(connection, key, retain_from, before_event).await?;
    loop {
      let snapshot = subscription.next_snapshot().await?;
      let reset = {
        let mut state = self.state.lock().unwrap();
        if state.epoch != epoch || cancel.is_cancelled() {
          return Ok(());
        }
        let Some(session) = state.sessions.get_mut(locator) else {
          return Ok(());
        };
        let reset = !session.generation.is_empty() && session.generation != snapshot.generation;
        let first_explicit = session.loaded.is_none() && session.priority == SessionPriority::Explicit;
        session.generation = snapshot.generation.clone();
        session.window = WindowInfo {
          event_offset: snapshot.event_offset,
          has_earlier: snapshot.has_earlier,
          generation: snapshot.generation,
        };
        session.estimated_bytes = snapshot.estimated_bytes;
        let loaded = Arc::new(snapshot.loaded);
        session.identities.retain(|identity| identity.loaded.strong_count() > 0);
        session.identities.push(SnapshotIdentity {
          loaded: Arc::downgrade(&loaded),
          window: session.window.clone(),
        });
        session.loaded = Some(loaded);
        session.native = snapshot.native;
        session.error = None;
        if let Some(previous) = previous_cancel.take() {
          previous.cancel();
        }
        state.enforce_budget(first_explicit.then_some(locator));
        reset
      };
      self.ready.notify_all();
      self.notify(Some(locator), reset);
    }
  }

  pub(crate) fn native(
    &self,
    locator: &SessionLocator,
    index: usize,
    loaded: &Arc<LoadedSession>,
  ) -> Option<serde_json::Value> {
    let state = self.state.lock().ok()?;
    let session = state.sessions.get(locator)?;
    if !Arc::ptr_eq(session.displayed.as_ref()?, loaded) {
      return None;
    }
    session
      .displayed_native
      .get(index)?
      .as_ref()
      .map(|value| value.as_ref().clone())
  }

  /// Only newest-page requests advance the displayed snapshot. Pagination,
  /// expanded trajectories and Inspector requests remain on that same image.
  pub(crate) fn advance(self: &Arc<Self>, locator: &SessionLocator) -> Result<Arc<LoadedSession>, String> {
    self.load(locator)?;
    let mut state = self.state.lock().unwrap();
    let session = state
      .sessions
      .get_mut(locator)
      .ok_or("Relay connection changed; reload the session")?;
    session.displayed = session.loaded.clone();
    session.displayed_native = session.native.clone();
    session.displayed_bytes = session.estimated_bytes;
    session
      .displayed
      .clone()
      .ok_or_else(|| "Relay snapshot unavailable".into())
  }

  fn notify(&self, locator: Option<&SessionLocator>, reset: bool) {
    let session_key = locator.and_then(|locator| encode_session_key(locator).ok());
    let _ = self.changes.send(RelayChange { session_key, reset });
  }
}

fn matches_locator(entry: &CatalogEntry, locator: &SessionLocator) -> bool {
  viewer_provider(entry.provider) == Some(locator.provider)
    && entry.header.path == locator.source_path
    && entry.header.id == locator.session_id
}

fn viewer_provider(provider: Provider) -> Option<ViewerProvider> {
  match provider {
    Provider::Codex => Some(ViewerProvider::Codex),
    Provider::Pi => Some(ViewerProvider::Pi),
    Provider::OpenCode => Some(ViewerProvider::OpenCode),
    Provider::ZCode => Some(ViewerProvider::ZCode),
    Provider::WorkBuddy => Some(ViewerProvider::WorkBuddy),
    Provider::Dsh => Some(ViewerProvider::Dsh),
  }
}

pub fn read_settings(path: &PathBuf) -> Result<RelaySettings, String> {
  match std::fs::read(path) {
    Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("Invalid Relay settings: {e}")),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RelaySettings::default()),
    Err(e) => Err(e.to_string()),
  }
}

pub fn write_settings(path: &PathBuf, settings: &RelaySettings) -> Result<(), String> {
  settings.validate()?;
  let parent = path.parent().ok_or("Invalid settings path")?;
  std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
  let bytes = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
  let temporary = path.with_extension("json.tmp");
  std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
  std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::service_protocol::DEFAULT_SERVICE_ENDPOINT;
  use crate::service_server::serve_listener;
  use std::{fs::OpenOptions, io::Write};
  use tokn_session_relay::{ProviderRoot, RelayConfig};

  #[test]
  fn saves_settings_and_rejects_nonlocal_endpoint_without_overwriting() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("settings/relay.json");
    assert_eq!(read_settings(&path).unwrap().mode, RelayMode::Automatic);
    write_settings(
      &path,
      &RelaySettings {
        endpoint: DEFAULT_SERVICE_ENDPOINT.into(),
        mode: RelayMode::External,
        ..Default::default()
      },
    )
    .unwrap();
    assert!(
      write_settings(
        &path,
        &RelaySettings {
          endpoint: "tcp://0.0.0.0:5557".into(),
          mode: RelayMode::External,
          ..Default::default()
        }
      )
      .is_err()
    );
    assert_eq!(read_settings(&path).unwrap().mode, RelayMode::External);
  }

  #[tokio::test]
  async fn pinned_snapshot_survives_updates_disconnect_and_reconnect() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    const HEADER: &str = "{\"type\":\"session\",\"id\":\"session-1\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n";
    const MESSAGE: &str =
      "{\"type\":\"message\",\"id\":\"one\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";
    std::fs::write(&path, format!("{HEADER}{MESSAGE}")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, root.path().into())]);
    config.include_native = true;
    config.poll_interval = Duration::from_millis(20);
    let server = tokio::spawn(serve_listener(listener, config.clone()));
    let manager = ViewerRelay::new();
    let mut changes = manager.changes.subscribe();
    manager
      .configure(RelaySettings {
        endpoint: endpoint.clone(),
        mode: RelayMode::External,
        ..Default::default()
      })
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      while !manager.has_catalog() {
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    assert!(manager.covers(ViewerProvider::Pi));
    assert!(!manager.covers(ViewerProvider::Codex));
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "session-1".into(),
      source_path: path.clone(),
    };
    let load_manager = manager.clone();
    let load_locator = locator.clone();
    let initial = tokio::task::spawn_blocking(move || load_manager.load(&load_locator))
      .await
      .unwrap()
      .unwrap();
    let initial_count = initial.events.len();
    OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(MESSAGE.replace("one", "two").as_bytes())
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        if manager.state.lock().unwrap().sessions[&locator]
          .loaded
          .as_ref()
          .unwrap()
          .events
          .len()
          > initial_count
        {
          break;
        }
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    assert!(
      Arc::ptr_eq(&initial, &manager.load(&locator).unwrap()),
      "details and paging retain the displayed snapshot"
    );
    let latest = manager.advance(&locator).unwrap();
    assert_eq!(latest.events.len(), initial_count + 1);
    assert!(manager.native(&locator, initial_count, &latest).is_some());
    assert!(
      manager.native(&locator, 0, &initial).is_none(),
      "native payload cannot cross snapshots"
    );
    server.abort();
    let _ = server.await;
    assert!(Arc::ptr_eq(&latest, &manager.load(&locator).unwrap()));
    let listener = tokio::net::TcpListener::bind(endpoint.trim_start_matches("tcp://"))
      .await
      .unwrap();
    let server = tokio::spawn(serve_listener(listener, config));
    manager
      .configure(RelaySettings {
        endpoint: endpoint.clone(),
        mode: RelayMode::External,
        ..Default::default()
      })
      .unwrap();
    OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(MESSAGE.replace("one", "three").as_bytes())
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        if manager.state.lock().unwrap().sessions[&locator]
          .loaded
          .as_ref()
          .unwrap()
          .events
          .len()
          > latest.events.len()
        {
          break;
        }
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    assert_eq!(manager.advance(&locator).unwrap().events.len(), initial_count + 2);
    manager
      .configure(RelaySettings {
        endpoint,
        mode: RelayMode::Local,
        ..Default::default()
      })
      .unwrap();
    manager
      .configure(RelaySettings {
        endpoint: "tcp://127.0.0.1:1".into(),
        mode: RelayMode::Local,
        ..Default::default()
      })
      .unwrap();
    assert!(!manager.covers(ViewerProvider::Pi));
    assert!(
      manager.load(&locator).is_err(),
      "different services never share cached snapshots"
    );
    server.abort();
  }

  #[tokio::test]
  async fn follow_failure_preserves_last_snapshot_and_notifies_recovery() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    const HEADER: &str = "{\"type\":\"session\",\"id\":\"session-1\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}\n";
    const MESSAGE: &str =
      "{\"type\":\"message\",\"id\":\"one\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";
    std::fs::write(&path, format!("{HEADER}{MESSAGE}")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, root.path().into())]);
    config.poll_interval = Duration::from_millis(20);
    let server = tokio::spawn(serve_listener(listener, config));
    let manager = ViewerRelay::new();
    let mut changes = manager.changes.subscribe();
    manager
      .configure(RelaySettings {
        endpoint,
        mode: RelayMode::External,
        ..Default::default()
      })
      .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
      while !manager.has_catalog() {
        changes.recv().await.unwrap();
      }
    })
    .await
    .unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "session-1".into(),
      source_path: path.clone(),
    };
    let key = encode_session_key(&locator).unwrap();
    let load_manager = manager.clone();
    let load_locator = locator.clone();
    let initial = tokio::task::spawn_blocking(move || load_manager.load(&load_locator))
      .await
      .unwrap()
      .unwrap();
    assert_eq!(manager.follow_error(&locator), None);

    OpenOptions::new()
      .append(true)
      .open(&path)
      .unwrap()
      .write_all(&[0xff, b'\n'])
      .unwrap();
    let failure = tokio::time::timeout(Duration::from_secs(4), async {
      loop {
        let change = changes.recv().await.unwrap();
        if change.session_key.as_deref() == Some(key.as_str()) && manager.follow_error(&locator).is_some() {
          break change;
        }
      }
    })
    .await
    .unwrap();
    assert!(!failure.reset);
    assert!(manager.follow_error(&locator).unwrap().contains("UTF-8"));
    assert_eq!(manager.status().phase, "live");
    assert_eq!(manager.status().error, None);
    assert!(Arc::ptr_eq(&initial, &manager.advance(&locator).unwrap()));

    std::fs::write(&path, format!("{HEADER}{MESSAGE}{}", MESSAGE.replace("one", "two"))).unwrap();
    let recovery = tokio::time::timeout(Duration::from_secs(8), async {
      loop {
        let change = changes.recv().await.unwrap();
        if change.session_key.as_deref() == Some(key.as_str())
          && manager.follow_error(&locator).is_none()
          && manager.state.lock().unwrap().sessions[&locator]
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.events.len() > initial.events.len())
        {
          break change;
        }
      }
    })
    .await
    .unwrap();
    assert_eq!(recovery.session_key.as_deref(), Some(key.as_str()));
    assert_eq!(
      manager.advance(&locator).unwrap().events.len(),
      initial.events.len() + 1
    );
    server.abort();
  }
}
