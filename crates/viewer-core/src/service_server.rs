use std::{
  collections::HashMap,
  sync::{Arc, Weak},
  time::Duration,
};

use tokio::{
  net::TcpListener,
  sync::{Mutex, OnceCell, Semaphore, watch},
};
use tokio_util::sync::CancellationToken;
use tokn_session_client::AgentClient;
use tokn_session_core::Provider;

use crate::{
  RelayConfig,
  service_metadata::PresentationCache,
  service_protocol::*,
  service_source::{SessionReader, Snapshot},
};

struct FollowedSession {
  current: OnceCell<watch::Sender<Arc<Snapshot>>>,
  reader: OnceCell<Arc<std::sync::Mutex<SessionReader>>>,
  initialized: watch::Sender<Option<Result<(), String>>>,
  wake: watch::Sender<()>,
  cancel: CancellationToken,
  #[cfg(test)]
  polls: std::sync::atomic::AtomicUsize,
}

impl FollowedSession {
  fn snapshots(&self) -> &watch::Sender<Arc<Snapshot>> {
    self.current.get().expect("follow waits for reader initialization")
  }
}

impl Drop for FollowedSession {
  fn drop(&mut self) {
    self.cancel.cancel();
  }
}

// A burst gets one reader pass after a short quiet period, bounded so sustained
// writes still reach the viewer. Changes arriving during a poll remain pending.
const READER_QUIET_PERIOD: Duration = Duration::from_millis(50);
const READER_MAX_BATCH_AGE: Duration = Duration::from_millis(200);

async fn wait_for_reader_poll(wake: &mut watch::Receiver<()>, cancel: &CancellationToken, interval: Duration) -> bool {
  tokio::select! {
    biased;
    _ = cancel.cancelled() => return false,
    _ = tokio::time::sleep(interval) => return true,
    result = wake.changed() => if result.is_err() { return false; },
  }
  let deadline = tokio::time::Instant::now() + READER_MAX_BATCH_AGE;
  loop {
    let quiet = (tokio::time::Instant::now() + READER_QUIET_PERIOD).min(deadline);
    tokio::select! {
      biased;
      _ = cancel.cancelled() => return false,
      _ = tokio::time::sleep_until(quiet) => return true,
      result = wake.changed() => if result.is_err() { return false; },
    }
  }
}

pub struct Service {
  index: Option<Arc<tokn_session_index::SessionIndex>>,
  config: RelayConfig,
  sessions: Mutex<HashMap<String, Weak<FollowedSession>>>,
  catalog: Mutex<Option<(std::time::Instant, Arc<Vec<CatalogEntry>>, Vec<String>)>>,
  metadata: Arc<PresentationCache>,
  #[cfg(test)]
  load_gates: std::sync::Mutex<HashMap<String, Arc<tests::LoadGate>>>,
}

/// Serve independently configured local consumers. Session bodies are opened
/// for follow requests and a shared background title/preview backfill.
pub async fn serve(endpoint: &str, config: RelayConfig) -> Result<(), String> {
  let listener = TcpListener::bind(local_endpoint(endpoint)?)
    .await
    .map_err(|err| err.to_string())?;
  serve_listener(listener, config).await
}

pub async fn serve_listener(listener: TcpListener, config: RelayConfig) -> Result<(), String> {
  if config.poll_interval.is_zero() {
    return Err("Poll interval must be positive".into());
  }
  if !listener.local_addr().map_err(|e| e.to_string())?.ip().is_loopback() {
    return Err("Relay service listener must be loopback-only".into());
  }
  let service = Service::new(config)?;
  let connections = Arc::new(Semaphore::new(64));
  // Dropping the service future closes its client sockets as well as its
  // listener, so consumers actually reconnect after a service shutdown.
  let mut peers = tokio::task::JoinSet::new();
  loop {
    let accepted = tokio::select! {
      accepted = listener.accept() => accepted,
      _ = peers.join_next(), if !peers.is_empty() => continue,
    };
    let (mut stream, _) = accepted.map_err(|err| err.to_string())?;
    let Ok(permit) = connections.clone().try_acquire_owned() else {
      continue;
    };
    let service = service.clone();
    peers.spawn(async move {
      let _permit = permit;
      if let Err(message) = service.handle(&mut stream).await {
        let _ = tokio::time::timeout(
          Duration::from_secs(2),
          write_frame(&mut stream, &Frame::Error { message }),
        )
        .await;
      }
    });
  }
}

impl Service {
  pub fn new(config: RelayConfig) -> Result<Arc<Self>, String> {
    Self::with_index(config, None)
  }

  pub(crate) fn from_index(
    config: RelayConfig,
    index: Arc<tokn_session_index::SessionIndex>,
  ) -> Result<Arc<Self>, String> {
    Self::with_index(config, Some(index))
  }

  fn with_index(
    config: RelayConfig,
    index: Option<Arc<tokn_session_index::SessionIndex>>,
  ) -> Result<Arc<Self>, String> {
    if config.poll_interval.is_zero() {
      return Err("Poll interval must be positive".into());
    }
    let service = Arc::new(Self {
      index,
      config,
      sessions: Mutex::new(HashMap::new()),
      catalog: Mutex::new(None),
      metadata: Arc::new(PresentationCache::default()),
      #[cfg(test)]
      load_gates: std::sync::Mutex::new(HashMap::new()),
    });
    let weak = Arc::downgrade(&service);
    if service.index.is_none() {
      tokio::spawn(async move {
        loop {
          tokio::time::sleep(Duration::from_millis(250)).await;
          let Some(service) = weak.upgrade() else {
            return;
          };
          let metadata = service.metadata.clone();
          drop(service);
          if tokio::task::spawn_blocking(move || metadata.step()).await.is_err() {
            return;
          }
        }
      });
    }
    Ok(service)
  }

  /// Live feed notifications only wake authoritative readers. Polling remains
  /// the recovery path for feed startup gaps, restarts, or omitted records.
  pub async fn invalidate(&self) {
    *self.catalog.lock().await = None;
    for session in self.sessions.lock().await.values().filter_map(Weak::upgrade) {
      session.wake.send_replace(());
    }
  }

  /// A managed feed update targets just the source readers it changed. Each
  /// watch channel retains one pending wake, including during initial loading.
  pub(crate) async fn invalidate_sessions(&self, hints: &[tokn_session_relay::stdio::SessionHint]) {
    if hints.is_empty() {
      return;
    }
    if self.index.is_none() {
      *self.catalog.lock().await = None;
    }
    let sessions = self.sessions.lock().await;
    for hint in hints {
      let key = serde_json::to_string(&(hint.provider, &hint.path, &hint.session_id))
        .expect("session source identity is serializable");
      if let Some(session) = sessions.get(&key).and_then(Weak::upgrade) {
        session.wake.send_replace(());
      }
    }
  }

  async fn catalog(&self) -> Result<(Arc<Vec<CatalogEntry>>, Vec<String>), String> {
    if let Some(index) = &self.index {
      let index = index.clone();
      let entries = tokio::task::spawn_blocking(move || crate::index_queries::snapshot_entries(&index))
        .await
        .map_err(|error| error.to_string())??;
      return Ok((Arc::new(entries), Vec::new()));
    }
    let mut cached = self.catalog.lock().await;
    if let Some((time, entries, warnings)) = cached.as_ref()
      && time.elapsed() < Duration::from_secs(2)
    {
      return Ok((Arc::new(self.metadata.decorate(entries)), warnings.clone()));
    }
    let roots = self.config.roots.clone();
    let metadata = self.metadata.clone();
    let (entries, warnings) = tokio::task::spawn_blocking(move || {
      let mut entries = Vec::new();
      let mut warnings = Vec::new();
      for root in roots {
        // Providers are optional installations. An absent resolved root is an
        // empty source; corrupt or inaccessible existing roots still report errors.
        if matches!(std::fs::metadata(&root.path), Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
          continue;
        }
        let source = tokn_session_relay::providers::source(root.provider);
        match AgentClient::list_session_headers(source, Some(root.path)) {
          Ok(headers) => entries.extend(headers.into_iter().map(|header| CatalogEntry {
            key: serde_json::to_string(&(root.provider, &header.path, &header.id)).expect("header key is serializable"),
            provider: root.provider,
            header,
          })),
          Err(error) => warnings.push(format!("{:?}: {error}", root.provider)),
        }
      }
      entries.sort_by(|a, b| a.key.cmp(&b.key));
      entries.dedup_by(|a, b| a.key == b.key);
      metadata.reconcile(&entries);
      (entries, warnings)
    })
    .await
    .map_err(|err| err.to_string())?;
    let entries = Arc::new(entries);
    *cached = Some((std::time::Instant::now(), entries.clone(), warnings.clone()));
    Ok((Arc::new(self.metadata.decorate(&entries)), warnings))
  }

  #[cfg(test)]
  async fn follow(self: &Arc<Self>, key: &str) -> Result<Arc<FollowedSession>, String> {
    self.follow_mode(key, false).await
  }

  async fn follow_mode(self: &Arc<Self>, key: &str, window: bool) -> Result<Arc<FollowedSession>, String> {
    let session = {
      let mut sessions = self.sessions.lock().await;
      if let Some(session) = sessions.get(key).and_then(Weak::upgrade) {
        session
      } else {
        sessions.retain(|_, value| value.strong_count() > 0);
        if sessions.len() >= 16 {
          return Err("Relay active-session limit reached; close an unused viewer session".into());
        }
        let session = Arc::new(FollowedSession {
          current: OnceCell::new(),
          reader: OnceCell::new(),
          initialized: watch::channel(None).0,
          wake: watch::channel(()).0,
          cancel: CancellationToken::new(),
          #[cfg(test)]
          polls: std::sync::atomic::AtomicUsize::new(0),
        });
        // Reserve this key before any I/O. The weak entry counts in-flight
        // loads toward the limit without making unused readers resident.
        sessions.insert(key.to_owned(), Arc::downgrade(&session));
        let service = self.clone();
        let key = key.to_owned();
        let worker = Arc::downgrade(&session);
        let cancel = session.cancel.clone();
        let wake = session.wake.subscribe();
        tokio::spawn(async move {
          let result = tokio::select! {
            _ = cancel.cancelled() => return,
            result = service.initialize_reader(&key, &cancel, window) => result,
          };
          let Some(session) = worker.upgrade() else {
            return;
          };
          match result {
            Ok(reader) => {
              session
                .current
                .set(watch::channel(Arc::new(reader.snapshot.clone())).0)
                .unwrap_or_else(|_| unreachable!("one initializer per reserved session"));
              session
                .reader
                .set(Arc::new(std::sync::Mutex::new(reader)))
                .unwrap_or_else(|_| unreachable!("one initializer per reserved session"));
              session.initialized.send_replace(Some(Ok(())));
              service.follow_reader(worker, cancel, wake);
            }
            Err(error) => {
              session.initialized.send_replace(Some(Err(error)));
            }
          }
        });
        session
      }
    };
    // Every caller owns the same reservation. Cancelling one waiter leaves
    // other subscribers' initialization intact; the last drop cancels it.
    let mut initialized = session.initialized.subscribe();
    loop {
      let result = initialized.borrow_and_update().clone();
      if let Some(result) = result {
        result?;
        return Ok(session);
      }
      initialized
        .changed()
        .await
        .map_err(|_| "Relay reader initialization stopped")?;
    }
  }

  async fn initialize_reader(
    &self,
    key: &str,
    cancel: &CancellationToken,
    window: bool,
  ) -> Result<SessionReader, String> {
    #[cfg(test)]
    {
      let gate = self.load_gates.lock().unwrap().get(key).cloned();
      if let Some(gate) = gate {
        gate.entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _permit = gate.release.acquire().await.map_err(|e| e.to_string())?;
      }
    }
    let entry = if let Some(index) = &self.index {
      let index = index.clone();
      let key = key.to_owned();
      tokio::task::spawn_blocking(move || crate::index_queries::snapshot_entry_for_key(&index, &key))
        .await
        .map_err(|e| e.to_string())??
    } else {
      let (catalog, _) = self.catalog().await?;
      catalog.iter().find(|entry| entry.key == key).cloned()
    }
    .ok_or(if self.index.is_some() {
      "Unknown session; refresh the index"
    } else {
      "Unknown Relay session; refresh the catalog"
    })?;
    let native = self.config.include_native;
    let root = self
      .config
      .roots
      .iter()
      .find(|root| root.provider == entry.provider && entry.header.path.starts_with(&root.path))
      .or_else(|| self.config.roots.iter().find(|root| root.provider == entry.provider))
      .ok_or("Relay provider is no longer configured")?
      .path
      .clone();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
      if window {
        SessionReader::new_window_cancellable(entry, native, root, cancel)
      } else {
        SessionReader::new_cancellable(entry, native, root, cancel)
      }
    })
    .await
    .map_err(|e| e.to_string())?
  }

  fn follow_reader(&self, worker: Weak<FollowedSession>, cancel: CancellationToken, mut wake: watch::Receiver<()>) {
    let interval = self.config.poll_interval;
    let metadata = self.metadata.clone();
    let index = self.index.clone();
    tokio::spawn(async move {
      loop {
        if !wait_for_reader_poll(&mut wake, &cancel, interval).await {
          break;
        }
        #[cfg(test)]
        if let Some(worker) = worker.upgrade() {
          worker.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let Some(session) = worker.upgrade() else {
          break;
        };
        let reader = session.reader.get().unwrap().clone();
        drop(session);
        let metadata = metadata.clone();
        let index = index.clone();
        let publisher = worker.clone();
        let result = tokio::task::spawn_blocking(move || {
          let mut reader = reader.lock().map_err(|_| "Session reader lock poisoned".to_string())?;
          let reader = &mut *reader;
          let result = reader.poll().and_then(|changed| {
            if let Some(index) = &index {
              if let Some(entry) = crate::index_queries::snapshot_entry_for_key(index, &reader.snapshot.entry.key)? {
                let header = &mut reader.snapshot.entry.header;
                if header.title != entry.header.title || header.preview != entry.header.preview {
                  header.title = entry.header.title;
                  header.preview = entry.header.preview;
                  reader.snapshot.revision += 1;
                  return Ok(true);
                }
              }
              return Ok(changed);
            }
            // Refresh even when a metadata-only source row produced no events.
            metadata.refresh_followed(&reader.snapshot.entry);
            // OpenCode follows already load current presentation metadata.
            // JSONL follows need their separately cached names/previews.
            if matches!(reader.snapshot.entry.provider, Provider::Codex | Provider::Pi) {
              let before = reader.snapshot.entry.header.clone();
              metadata.apply(
                &reader.snapshot.entry.key,
                reader.snapshot.entry.provider,
                &mut reader.snapshot.entry.header,
              );
              if before != reader.snapshot.entry.header {
                reader.snapshot.revision += 1;
                return Ok(true);
              }
            }
            Ok(changed)
          });
          if let Err(error) = &result {
            reader.snapshot.error = Some(error.clone());
          }
          if (result.is_err() || result.as_ref().is_ok_and(|changed| *changed))
            && let Some(publisher) = publisher.upgrade()
          {
            // Publish while holding the reader lock, in the same order as
            // explicit history expansions. An older poll cannot overwrite a
            // newer expanded snapshot after releasing that lock.
            publisher.snapshots().send_replace(Arc::new(reader.snapshot.clone()));
          }
          result.map(|_| ())
        })
        .await;
        let Some(worker) = worker.upgrade() else {
          break;
        };
        match result {
          Ok(Ok(())) => {}
          Ok(Err(_)) => break,
          Err(error) => {
            let mut failed = worker.snapshots().borrow().as_ref().clone();
            failed.error = Some(format!("Relay session reader stopped: {error}"));
            worker.snapshots().send_replace(Arc::new(failed));
            break;
          }
        }
      }
    });
  }

  pub async fn handle(
    self: &Arc<Self>,
    stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send),
  ) -> Result<(), String> {
    let request: Request = tokio::time::timeout(Duration::from_secs(10), read_frame(stream))
      .await
      .map_err(|_| "Relay request timed out")??;
    if request.version != PROTOCOL_VERSION {
      return Err("Unsupported Relay protocol version".into());
    }
    let mut providers = Vec::new();
    for root in &self.config.roots {
      if !providers.contains(&root.provider) {
        providers.push(root.provider);
      }
    }
    send(
      stream,
      Frame::Hello {
        version: PROTOCOL_VERSION,
        native: self.config.include_native,
        providers,
      },
    )
    .await?;
    match request.action {
      Action::Catalog => {
        let (entries, warnings) = self.catalog().await?;
        for entry in entries.iter() {
          send(stream, Frame::Header { entry: entry.clone() }).await?;
        }
        send(stream, Frame::CatalogEnd { warnings }).await
      }
      action @ (Action::Follow { .. } | Action::FollowWindow { .. }) => {
        let (session_key, window) = match action {
          Action::Follow { session_key } => (session_key, None),
          Action::FollowWindow {
            session_key,
            retain_from,
            before_event,
          } => (session_key, Some((retain_from, before_event))),
          _ => unreachable!(),
        };
        let session = self.follow_mode(&session_key, window.is_some()).await?;
        let selected = session.clone();
        tokio::task::spawn_blocking(move || {
          let mut reader = selected
            .reader
            .get()
            .unwrap()
            .lock()
            .map_err(|_| "Session reader lock poisoned".to_string())?;
          if reader.ensure_history(window)? {
            selected.snapshots().send_replace(Arc::new(reader.snapshot.clone()));
          }
          Ok::<_, String>(())
        })
        .await
        .map_err(|e| e.to_string())??;
        let mut changes = session.snapshots().subscribe();
        let mut previous_generation = String::new();
        let mut previous_length = 0;
        let mut event_offset: usize = 0;
        let mut base = 0;
        let mut retained_turns = 0;
        let mut retained_events = 0;
        loop {
          let snapshot = changes.borrow_and_update().clone();
          if let Some(error) = &snapshot.error {
            return Err(error.clone());
          }
          let generation_changed = previous_generation != snapshot.generation;
          let previous_offset = event_offset;
          let previous_base = base;
          base = snapshot.event_base;
          if let Some((retain_from, before_event)) = window {
            event_offset = if previous_generation.is_empty() {
              snapshot.records.window_start(
                retain_from.map(|position| position.saturating_sub(snapshot.event_base)),
                before_event.map(|position| position.saturating_sub(snapshot.event_base)),
              )
            } else if generation_changed {
              if event_offset == 0 {
                0
              } else {
                snapshot.records.replacement_start(retained_turns, retained_events)
              }
            } else {
              snapshot
                .records
                .context_start(event_offset.saturating_sub(snapshot.event_base))
            };
          }
          if window.is_some() {
            event_offset += base;
          }
          let local_offset = event_offset.saturating_sub(base);
          let reset = generation_changed || event_offset != previous_offset || base != previous_base;
          send(
            stream,
            Frame::Begin {
              generation: snapshot.generation.clone(),
              revision: snapshot.revision.to_string(),
              reset,
              header: snapshot.entry.header.clone(),
            },
          )
          .await?;
          if window.is_some() {
            send(
              stream,
              Frame::Window {
                event_offset,
                has_earlier: snapshot.has_earlier || local_offset > 0,
              },
            )
            .await?;
          }
          let mut position = if reset {
            if window.is_some() {
              snapshot.records.record_at_event(local_offset)
            } else {
              0
            }
          } else {
            previous_length
          };
          while position < snapshot.records.len() {
            let history = snapshot.records.clone();
            let end = (position + 32).min(history.len());
            let records = tokio::task::spawn_blocking(move || {
              (position..end)
                .map(|index| history.read(index))
                .collect::<Result<Vec<_>, String>>()
            })
            .await
            .map_err(|e| e.to_string())??;
            for (mut record, start) in records {
              if start < local_offset {
                // A redacted sibling may be trimmed out of this window, but
                // its shared native payload must remain withheld.
                if record.record.events.iter().any(|event| {
                  matches!(event,
                  tokn_session_core::AgentEvent::Reasoning(reasoning) if reasoning.redacted == Some(true))
                }) {
                  record.record.native = None;
                }
                let skip = (local_offset - start).min(record.record.events.len());
                record.record.events.drain(..skip);
              }
              send(
                stream,
                Frame::Record {
                  record: Box::new(record),
                },
              )
              .await?;
            }
            position = end;
          }
          send(
            stream,
            Frame::Commit {
              generation: snapshot.generation.clone(),
              revision: snapshot.revision.to_string(),
            },
          )
          .await?;
          previous_generation = snapshot.generation.clone();
          previous_length = snapshot.records.len();
          retained_turns = snapshot.records.retained_turns(local_offset);
          retained_events = snapshot.records.events.saturating_sub(local_offset);
          loop {
            tokio::select! {
              result = changes.changed() => { result.map_err(|_| "Relay session reader stopped")?; break; }
              _ = tokio::time::sleep(Duration::from_secs(2)) => send(stream, Frame::Heartbeat).await?,
            }
          }
        }
      }
    }
  }
}

async fn send(stream: &mut (impl tokio::io::AsyncWrite + Unpin), frame: Frame) -> Result<(), String> {
  tokio::time::timeout(Duration::from_secs(10), write_frame(stream, &frame))
    .await
    .map_err(|_| "Relay subscriber is too slow; reconnect for a fresh snapshot")?
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::service_client::{Connection, RelaySubscription};
  use std::sync::atomic::{AtomicUsize, Ordering};
  use tokn_session_relay::ProviderRoot;

  pub(super) struct LoadGate {
    pub entered: AtomicUsize,
    pub release: Semaphore,
  }

  impl LoadGate {
    async fn wait_for_load(&self) {
      tokio::time::timeout(Duration::from_secs(2), async {
        while self.entered.load(Ordering::SeqCst) == 0 {
          tokio::task::yield_now().await;
        }
      })
      .await
      .expect("reader should start");
    }
  }

  fn gate(service: &Service, key: &str) -> Arc<LoadGate> {
    let gate = Arc::new(LoadGate {
      entered: AtomicUsize::new(0),
      release: Semaphore::new(0),
    });
    service.load_gates.lock().unwrap().insert(key.into(), gate.clone());
    gate
  }

  async fn fixture() -> (tempfile::TempDir, Arc<Service>, Vec<String>) {
    let root = tempfile::TempDir::new().unwrap();
    for id in ["slow", "fast"] {
      std::fs::write(
        root.path().join(format!("{id}.jsonl")),
        format!(
          "{{\"type\":\"session\",\"id\":\"{id}\",\"timestamp\":\"2026-01-01\",\"cwd\":\"/tmp\"}}\n\
           {{\"type\":\"message\",\"id\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n"
        ),
      )
      .unwrap();
    }
    let mut config = RelayConfig::new(vec![ProviderRoot::new(Provider::Pi, root.path().into())]);
    config.poll_interval = Duration::from_secs(60);
    let service = Service::new(config).unwrap();
    let (catalog, _) = service.catalog().await.unwrap();
    let keys = ["slow", "fast"]
      .iter()
      .map(|id| catalog.iter().find(|entry| entry.header.id == *id).unwrap().key.clone())
      .collect();
    (root, service, keys)
  }

  async fn session_cancel(service: &Service, key: &str) -> CancellationToken {
    service.sessions.lock().await[key].upgrade().unwrap().cancel.clone()
  }

  #[tokio::test]
  async fn managed_bursts_poll_only_the_changed_session_once() {
    use std::io::Write;
    use tokn_session_relay::stdio::SessionHint;
    let (root, service, keys) = fixture().await;
    let slow = service.follow(&keys[0]).await.unwrap();
    let fast = service.follow(&keys[1]).await.unwrap();
    let mut slow_changes = slow.snapshots().subscribe();
    let mut fast_changes = fast.snapshots().subscribe();
    for id in ["slow", "fast"] {
      let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(root.path().join(format!("{id}.jsonl")))
        .unwrap();
      writeln!(
        file,
        "{{\"type\":\"message\",\"id\":\"appended\",\"message\":{{\"role\":\"user\",\"content\":\"new prompt\"}}}}"
      )
      .unwrap();
    }
    let hint = SessionHint {
      provider: Provider::Pi,
      path: root.path().join("slow.jsonl"),
      session_id: "slow".into(),
    };
    for _ in 0..100 {
      service.invalidate_sessions(std::slice::from_ref(&hint)).await;
    }
    tokio::time::timeout(Duration::from_secs(2), slow_changes.changed())
      .await
      .unwrap()
      .unwrap();
    assert_eq!(slow_changes.borrow().records.len(), 3);
    assert_eq!(
      slow.polls.load(Ordering::SeqCst),
      1,
      "a hundred hints must coalesce into one reader poll"
    );
    assert_eq!(fast.polls.load(Ordering::SeqCst), 0, "unrelated sources must not poll");
    assert_eq!(fast_changes.borrow().records.len(), 2);

    service
      .invalidate_sessions(&[SessionHint {
        provider: Provider::Pi,
        path: root.path().join("fast.jsonl"),
        session_id: "fast".into(),
      }])
      .await;
    tokio::time::timeout(Duration::from_secs(2), fast_changes.changed())
      .await
      .unwrap()
      .unwrap();
    assert_eq!(fast_changes.borrow().records.len(), 3);
    assert_eq!(fast.polls.load(Ordering::SeqCst), 1);
  }

  #[tokio::test(start_paused = true)]
  async fn sustained_reader_hints_cannot_postpone_a_poll_indefinitely() {
    let (sender, mut receiver) = watch::channel(());
    let producer = tokio::spawn(async move {
      loop {
        sender.send_replace(());
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    });
    let started = tokio::time::Instant::now();
    assert!(wait_for_reader_poll(&mut receiver, &CancellationToken::new(), Duration::from_secs(60)).await);
    assert_eq!(started.elapsed(), READER_MAX_BATCH_AGE);
    producer.abort();
  }

  #[tokio::test]
  async fn unrelated_session_loads_do_not_wait_for_a_slow_initializer() {
    let (_root, service, keys) = fixture().await;
    let slow = gate(&service, &keys[0]);
    let loader = service.clone();
    let key = keys[0].clone();
    let pending = tokio::spawn(async move { loader.follow(&key).await });
    slow.wait_for_load().await;

    let fast = tokio::time::timeout(Duration::from_secs(2), service.follow(&keys[1]))
      .await
      .expect("unrelated session must not wait for the slow reader")
      .unwrap();
    assert_eq!(fast.snapshots().borrow().entry.header.id, "fast");
    assert_eq!(slow.entered.load(Ordering::SeqCst), 1);
    pending.abort();
    let _ = pending.await;
  }

  #[tokio::test]
  async fn same_session_load_is_shared_when_one_initial_waiter_is_cancelled() {
    let (_root, service, keys) = fixture().await;
    let slow = gate(&service, &keys[0]);
    let loader = service.clone();
    let key = keys[0].clone();
    let first = tokio::spawn(async move { loader.follow(&key).await });
    slow.wait_for_load().await;
    let cancel = session_cancel(&service, &keys[0]).await;
    let loader = service.clone();
    let key = keys[0].clone();
    let second = tokio::spawn(async move { loader.follow(&key).await });
    tokio::time::timeout(Duration::from_secs(2), async {
      while service.sessions.lock().await[&keys[0]].strong_count() < 2 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .unwrap();
    first.abort();
    let _ = first.await;
    assert!(!cancel.is_cancelled(), "remaining waiter must keep its initializer");
    assert_eq!(slow.entered.load(Ordering::SeqCst), 1);

    slow.release.add_permits(1);
    let loaded = tokio::time::timeout(Duration::from_secs(2), second)
      .await
      .unwrap()
      .unwrap()
      .unwrap();
    let shared = service.follow(&keys[0]).await.unwrap();
    assert!(Arc::ptr_eq(&loaded, &shared));
    assert_eq!(slow.entered.load(Ordering::SeqCst), 1);
  }

  #[tokio::test]
  async fn dropping_embedded_subscription_cancels_its_queued_initial_load() {
    let (_root, service, keys) = fixture().await;
    let slow = gate(&service, &keys[0]);
    let connection = Connection::Embedded(service.clone());
    let client = RelaySubscription::connect_window_from(&connection, &keys[0], None, None)
      .await
      .unwrap();
    slow.wait_for_load().await;
    let cancel = session_cancel(&service, &keys[0]).await;
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), cancel.cancelled())
      .await
      .expect("client drop must release the handler and cancel the reader");
    assert!(service.sessions.lock().await[&keys[0]].upgrade().is_none());

    let mut fast = RelaySubscription::connect_window_from(&connection, &keys[1], None, None)
      .await
      .unwrap();
    let loaded = tokio::time::timeout(Duration::from_secs(2), fast.next_snapshot())
      .await
      .expect("explicit follow should remain responsive")
      .unwrap();
    assert_eq!(loaded.loaded.reference.id, "fast");
  }

  #[tokio::test]
  async fn pending_initializers_count_toward_the_limit_and_release_on_drop() {
    let (_root, service, _) = fixture().await;
    let mut pending = Vec::new();
    for id in 0..16 {
      let key = format!("blocked-{id}");
      let blocked = gate(&service, &key);
      let loader = service.clone();
      pending.push(tokio::spawn(async move { loader.follow(&key).await }));
      blocked.wait_for_load().await;
    }
    assert!(
      service
        .follow("overflow")
        .await
        .err()
        .unwrap()
        .contains("active-session limit")
    );
    let cancel = session_cancel(&service, "blocked-0").await;
    pending.remove(0).abort();
    tokio::time::timeout(Duration::from_secs(2), cancel.cancelled())
      .await
      .unwrap();
    let replacement = gate(&service, "replacement");
    let loader = service.clone();
    pending.push(tokio::spawn(async move { loader.follow("replacement").await }));
    replacement.wait_for_load().await;
    assert_eq!(service.sessions.lock().await.len(), 16);
    for task in pending {
      task.abort();
      let _ = task.await;
    }
  }
}
