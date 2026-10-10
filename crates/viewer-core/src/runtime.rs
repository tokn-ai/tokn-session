use crate::{
  model::{IndexWorkerError, ViewerProvider},
  service::{IndexRefresh, SessionIndexWake, ViewerService},
  watcher::{SessionFileWatcher, WatchRequest},
};
use std::{
  collections::{BTreeMap, BTreeSet},
  path::{Path, PathBuf},
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::broadcast;

#[derive(Clone, Debug, serde::Serialize)]
pub struct ViewerEvent {
  pub event: String,
  pub payload: serde_json::Value,
}

fn emit<T: serde::Serialize>(events: &broadcast::Sender<ViewerEvent>, event: &str, payload: T) -> Result<(), String> {
  let payload = serde_json::to_value(payload).map_err(|e| e.to_string())?;
  let _ = events.send(ViewerEvent {
    event: event.into(),
    payload,
  });
  Ok(())
}

/// Owns the shared background workers. Adapters subscribe and forward events.
pub struct ViewerRuntime {
  pub service: ViewerService,
  pub events: broadcast::Sender<ViewerEvent>,
  tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for ViewerRuntime {
  fn drop(&mut self) {
    for task in &self.tasks {
      task.abort();
    }
  }
}

impl ViewerRuntime {
  pub fn start(service: ViewerService) -> Self {
    let (events, _) = broadcast::channel(128);
    let mut tasks = Vec::new();
    let relay = service.relay.clone();
    let mut changes = relay.changes.subscribe();
    let relay_events = events.clone();
    let update_service = service.clone();
    tasks.push(tokio::spawn(async move {
      let mut last_status = None;
      loop {
        let change = match changes.recv().await {
          Ok(change) => change,
          Err(broadcast::error::RecvError::Lagged(_)) => crate::relay::RelayChange {
            session_key: None,
            reset: true,
          },
          Err(_) => return,
        };
        let service = update_service.clone();
        let source_change = change.clone();
        if let Ok(updates) = tokio::task::spawn_blocking(move || service.publish_session_updates(&source_change)).await
        {
          for update in updates {
            let _ = emit(&relay_events, "session-updated", update);
          }
        }
        let _ = emit(&relay_events, "relay-changed", change);
        emit_changed_relay_status(&relay_events, &mut last_status, relay.status());
      }
    }));
    let (retry_sender, mut retry_receiver) = tokio::sync::mpsc::unbounded_channel();
    service.set_session_index_retry_sender(retry_sender);
    let mut index_wakes = service.relay.index_wakes.subscribe();
    // Register before the first catalog so a rollout append during initial
    // discovery is retained by the watcher rather than waiting for the
    // recovery scan. Watcher setup is advisory: a platform or permission
    // failure leaves the durable full-catalog path intact.
    let mut session_watcher = match SessionFileWatcher::new(service.watched_file_catalog_roots()) {
      Ok(watcher) => Some(watcher),
      Err(error) => {
        eprintln!("viewer session watcher is unavailable: {error}");
        None
      }
    };
    if let Some(watcher) = session_watcher.as_mut() {
      for error in watcher.take_initial_watch_errors() {
        eprintln!("viewer session watcher is partially unavailable: {error}");
      }
    }
    let watcher_startup_guard = session_watcher
      .as_ref()
      .map(SessionFileWatcher::startup_guard)
      .unwrap_or_default();
    let refresh_service = service.clone();
    let scheduler_events = events.clone();
    let mut progress_receiver = service.subscribe_session_index_progress();
    let progress_events = events.clone();

    // Progress is deliberately a separate event stream from
    // `session-index-changed`: changing an active provider or remaining
    // queue count must not make the sidebar reread SQLite. A new subscriber
    // also receives the latest watch snapshot immediately.
    tasks.push(tokio::spawn(async move {
      let initial_progress = progress_receiver.borrow_and_update().clone();
      let _ = emit(&progress_events, "session-index-progress", initial_progress);
      while progress_receiver.changed().await.is_ok() {
        let progress = progress_receiver.borrow_and_update().clone();
        let _ = emit(&progress_events, "session-index-progress", progress);
      }
    }));

    // Initial discovery is deliberately background-only: an established
    // sidebar remains responsive from its previous index, while a first run
    // exposes its compact catalog as soon as the catalog pass commits.
    tasks.push(tokio::spawn(async move {
      // FSEvents starts its run loop asynchronously. The previous durable
      // sidebar stays available during this tiny guard, then the first
      // catalog snapshots after the stream can observe its own writes.
      if !watcher_startup_guard.is_zero() {
        tokio::time::sleep(watcher_startup_guard).await;
      }
      let mut next_full_catalog_refresh = Instant::now();
      let mut next_unwatched_provider_catalog_refresh = Instant::now();
      let mut next_pending_body_refresh = Instant::now();
      let mut consecutive_catalog_retries = 0_u8;
      let mut consecutive_unwatched_provider_catalog_retries = 0_u8;
      let mut consecutive_changed_file_retries = 0_u8;
      let mut pending_wake = None;
      let mut delayed_changed_file_wake = None;
      let mut next_changed_file_retry = None;
      let mut lease = None;
      let mut retry_generation = 0;
      let mut has_pending_body_jobs = refresh_service.session_index_progress().body.pending_jobs > 0;
      loop {
        // An explicitly external viewer must not monopolize the native index
        // lease while sourcing all its catalogs from a different server.
        if refresh_service.relay.status().settings.mode == crate::relay::RelayMode::External {
          lease = None;
          refresh_service.pause_native_index();
          while retry_receiver.try_recv().is_ok() {}
          while index_wakes.try_recv().is_ok() {}
          tokio::time::sleep(Duration::from_secs(1)).await;
          continue;
        }
        if lease.is_none() {
          let acquired = refresh_service.indexer_lock.as_ref().map_or_else(
            || Ok(Some(crate::indexer::IndexerLease::in_memory())),
            |lock| lock.try_acquire(),
          );
          match acquired {
            Ok(Some(owner)) => {
              lease = Some(std::sync::Arc::new(owner));
              next_full_catalog_refresh = Instant::now();
            }
            Ok(None) => {
              match refresh_service.follow_shared_index() {
                Ok(true) => {
                  let _ = emit(
                    &scheduler_events,
                    "session-index-changed",
                    IndexRefresh {
                      changed: true,
                      catalog_refresh_required: true,
                      ..Default::default()
                    },
                  );
                  let _ = emit(
                    &scheduler_events,
                    "relay-changed",
                    crate::relay::RelayChange {
                      session_key: None,
                      reset: true,
                    },
                  );
                }
                Ok(false) => {}
                Err(error) => eprintln!("shared index observation failed: {error}"),
              }
              // Followers never consume provider work. The owner has its own
              // watcher/feed; explicit retries use the shared generation file.
              while retry_receiver.try_recv().is_ok() {}
              while index_wakes.try_recv().is_ok() {}
              tokio::time::sleep(Duration::from_secs(1)).await;
              continue;
            }
            Err(error) => {
              eprintln!("could not acquire session indexer: {error}");
              refresh_service.settle_session_index_worker_error_after_refresh(
                IndexWorkerError::RefreshFailed,
                retry_at_ms_after(Duration::from_secs(1)),
              );
              tokio::time::sleep(Duration::from_secs(1)).await;
              continue;
            }
          }
        }
        if let Some(lock) = &refresh_service.indexer_lock {
          match lock.retry_generation() {
            Ok(generation) if generation != retry_generation => {
              retry_generation = generation;
              pending_wake = Some(SessionIndexWake::FullCatalog);
            }
            Ok(_) => {}
            Err(error) => eprintln!("could not observe session index retry: {error}"),
          }
        }
        let now = Instant::now();
        let catalog_due = now >= next_full_catalog_refresh;
        let unwatched_provider_catalog_due = now >= next_unwatched_provider_catalog_refresh;
        let changed_file_retry_due = next_changed_file_retry.is_some_and(|deadline| now >= deadline);
        let pending_body_due = has_pending_body_jobs && now >= next_pending_body_refresh;
        let work = if catalog_due {
          // A full pass subsumes any queued single-file checks.
          pending_wake = None;
          delayed_changed_file_wake = None;
          next_changed_file_retry = None;
          Some(SessionIndexWork::FullCatalog)
        } else if matches!(pending_wake.as_ref(), Some(SessionIndexWake::FullCatalog)) {
          pending_wake = None;
          Some(SessionIndexWork::FullCatalog)
        } else if changed_file_retry_due {
          next_changed_file_retry = None;
          delayed_changed_file_wake
            .take()
            .or_else(|| pending_wake.take())
            .map(session_index_work_from_wake)
        } else if let Some(wake) = pending_wake.take() {
          Some(session_index_work_from_wake(wake))
        } else if unwatched_provider_catalog_due {
          let providers = providers_without_native_watch(&session_watcher);
          if providers.is_empty() {
            next_unwatched_provider_catalog_refresh = now + INDEX_FULL_CATALOG_RECOVERY_INTERVAL;
            None
          } else {
            Some(SessionIndexWork::ProviderCatalog(providers))
          }
        } else {
          None
        };
        if work.is_none() && !pending_body_due {
          match refresh_service.observe_shared_index_change() {
            Ok(true) => {
              let _ = emit(
                &scheduler_events,
                "session-index-changed",
                IndexRefresh {
                  changed: true,
                  catalog_refresh_required: true,
                  ..Default::default()
                },
              );
            }
            Ok(false) => {}
            Err(error) => eprintln!("shared index observation failed: {error}"),
          }
          if let Some(wake) = wait_for_session_index_work(
            &mut retry_receiver,
            &mut session_watcher,
            &mut index_wakes,
            Duration::from_secs(1),
          )
          .await
          {
            merge_session_index_wake(&mut pending_wake, wake);
          }
          continue;
        }
        let full_catalog = matches!(work.as_ref(), Some(SessionIndexWork::FullCatalog));
        let provider_catalog = matches!(work.as_ref(), Some(SessionIndexWork::ProviderCatalog(_)));
        let service = refresh_service.clone();
        let worker_lease = lease.clone();
        let result = tokio::task::spawn_blocking(move || {
          // Aborting an async runtime cannot stop a blocking scan. Retain the
          // lease until that scan has actually finished writing.
          let _lease = worker_lease;
          match work {
            Some(SessionIndexWork::FullCatalog) => service.refresh_session_catalog(),
            Some(SessionIndexWork::ProviderCatalog(providers)) => service.refresh_session_catalog_providers(&providers),
            Some(SessionIndexWork::ChangedFiles(paths)) => service.refresh_changed_file_catalogs(paths),
            None => service.refresh_pending_session_index_automated(),
          }
        })
        .await;
        match result {
          Ok(Ok(mut refresh)) => {
            // A full catalog is the deliberate recovery path for watcher
            // gaps and provider topology changes. Its normal cadence starts
            // after the potentially slow scan returns; ordinary writes use
            // `ChangedFiles` above instead of resetting this timer.
            if full_catalog {
              if let Some(watcher) = session_watcher.as_mut() {
                for error in watcher.refresh_watches() {
                  eprintln!("viewer session watcher is partially unavailable: {error}");
                }
              }
              let retry_soon = full_catalog_needs_prompt_retry(&refresh)
                && consecutive_catalog_retries < MAX_CONSECUTIVE_CATALOG_RETRIES;
              if retry_soon {
                consecutive_catalog_retries += 1;
                next_full_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
              } else {
                consecutive_catalog_retries = 0;
                consecutive_unwatched_provider_catalog_retries = 0;
                consecutive_changed_file_retries = 0;
                next_full_catalog_refresh = Instant::now() + INDEX_FULL_CATALOG_RECOVERY_INTERVAL;
              }
              next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_UNWATCHED_PROVIDER_CATALOG_INTERVAL;
            } else if provider_catalog {
              // SQLite- and compressed-log-backed providers retain their
              // responsive cadence without making every Codex/Pi append
              // enumerate unrelated histories. Retry only the subset that
              // just changed topology or failed to enumerate.
              let retry_soon = provider_catalog_needs_prompt_retry(&refresh)
                && consecutive_unwatched_provider_catalog_retries < MAX_CONSECUTIVE_CATALOG_RETRIES;
              if retry_soon {
                consecutive_unwatched_provider_catalog_retries += 1;
                next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
              } else {
                consecutive_unwatched_provider_catalog_retries = 0;
                next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_UNWATCHED_PROVIDER_CATALOG_INTERVAL;
              }
            } else if refresh.retry_catalog_soon {
              // A direct header read that raced a rename/delete leaves its
              // old source visible and asks the proven full path to settle
              // membership shortly.
              consecutive_catalog_retries = 0;
              consecutive_changed_file_retries = 0;
              next_full_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
              delayed_changed_file_wake = None;
              next_changed_file_retry = None;
            } else if !refresh.retry_changed_file_paths.is_empty()
              && consecutive_changed_file_retries < MAX_CONSECUTIVE_CHANGED_FILE_RETRIES
            {
              // An ordinary append happened during the tiny direct-header
              // read window. Recheck that one known file shortly instead of
              // turning normal session activity into a global rescan.
              merge_session_index_wake(
                &mut delayed_changed_file_wake,
                SessionIndexWake::ChangedFiles(refresh.retry_changed_file_paths.clone()),
              );
              consecutive_changed_file_retries += 1;
              next_changed_file_retry = Some(Instant::now() + INDEX_CATALOG_RETRY_INTERVAL);
            } else if !refresh.retry_changed_file_paths.is_empty() {
              // If a file never stays still long enough for a direct header
              // snapshot, avoid a permanent one-second target loop. The
              // full catalog's per-source cursor checks remain the bounded
              // recovery path.
              consecutive_changed_file_retries = 0;
              next_full_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
            } else if !full_catalog {
              consecutive_changed_file_retries = 0;
            }
            has_pending_body_jobs = refresh.has_pending_body_jobs;
            if has_pending_body_jobs {
              next_pending_body_refresh = Instant::now() + INDEX_PENDING_BODY_REFRESH_INTERVAL;
            }
            let needs_retry = session_index_needs_retry(&refresh);
            if refresh.changed {
              let mut keys = refresh.updated_session_keys.clone();
              keys.extend(refresh.attention_session_keys.iter().cloned());
              keys.sort();
              keys.dedup();
              let service = refresh_service.clone();
              let expected_notifications = keys.clone();
              match tokio::task::spawn_blocking(move || service.session_notifications(&keys)).await {
                Ok(notifications) => {
                  refresh.catalog_refresh_required |= expected_notifications.iter().any(|key| {
                    !notifications
                      .iter()
                      .any(|notification| notification["session_key"].as_str() == Some(key))
                  });
                  for notification in notifications {
                    if emit(&scheduler_events, "session-notification", notification).is_err() {
                      refresh.catalog_refresh_required = true;
                    }
                  }
                }
                Err(_) => refresh.catalog_refresh_required = true,
              }
              let _ = emit(&scheduler_events, "session-index-changed", refresh);
            }
            let until_full_catalog = next_full_catalog_refresh.saturating_duration_since(Instant::now());
            let until_unwatched_provider_catalog =
              next_unwatched_provider_catalog_refresh.saturating_duration_since(Instant::now());
            let until_catalog = until_full_catalog.min(until_unwatched_provider_catalog);
            let until_changed_file_retry = next_changed_file_retry
              .map(|deadline| deadline.saturating_duration_since(Instant::now()))
              .unwrap_or(until_catalog);
            let body_or_catalog_delay = if has_pending_body_jobs {
              INDEX_PENDING_BODY_REFRESH_INTERVAL.min(until_catalog)
            } else {
              until_catalog
            };
            let delay = body_or_catalog_delay.min(until_changed_file_retry);
            if needs_retry {
              refresh_service.settle_session_index_waiting_to_retry_after_refresh(retry_at_ms_after(delay));
            } else {
              refresh_service.settle_session_index_idle_after_refresh();
            }
            if let Some(wake) = wait_for_session_index_work(
              &mut retry_receiver,
              &mut session_watcher,
              &mut index_wakes,
              delay.min(Duration::from_secs(1)),
            )
            .await
            {
              refresh_service.relay.wake_sources().await;
              merge_session_index_wake(&mut pending_wake, wake);
            }
          }
          Ok(Err(error)) => {
            eprintln!("viewer session index refresh failed: {error}");
            if full_catalog {
              let retry_soon = consecutive_catalog_retries < MAX_CONSECUTIVE_CATALOG_RETRIES;
              if retry_soon {
                consecutive_catalog_retries += 1;
                next_full_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
              } else {
                consecutive_catalog_retries = 0;
                next_full_catalog_refresh = Instant::now() + INDEX_FULL_CATALOG_RECOVERY_INTERVAL;
              }
              consecutive_changed_file_retries = 0;
              next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_UNWATCHED_PROVIDER_CATALOG_INTERVAL;
            } else if provider_catalog {
              let retry_soon = consecutive_unwatched_provider_catalog_retries < MAX_CONSECUTIVE_CATALOG_RETRIES;
              if retry_soon {
                consecutive_unwatched_provider_catalog_retries += 1;
                next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
              } else {
                consecutive_unwatched_provider_catalog_retries = 0;
                next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_UNWATCHED_PROVIDER_CATALOG_INTERVAL;
              }
            } else {
              consecutive_changed_file_retries = 0;
              next_full_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
            }
            let delay = next_full_catalog_refresh
              .saturating_duration_since(Instant::now())
              .min(next_unwatched_provider_catalog_refresh.saturating_duration_since(Instant::now()));
            refresh_service.settle_session_index_worker_error_after_refresh(
              IndexWorkerError::RefreshFailed,
              retry_at_ms_after(delay),
            );
            if let Some(wake) = wait_for_session_index_work(
              &mut retry_receiver,
              &mut session_watcher,
              &mut index_wakes,
              delay.min(Duration::from_secs(1)),
            )
            .await
            {
              refresh_service.relay.wake_sources().await;
              merge_session_index_wake(&mut pending_wake, wake);
            }
          }
          Err(error) => {
            eprintln!("viewer session index refresh task failed: {error}");
            if full_catalog {
              let retry_soon = consecutive_catalog_retries < MAX_CONSECUTIVE_CATALOG_RETRIES;
              if retry_soon {
                consecutive_catalog_retries += 1;
                next_full_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
              } else {
                consecutive_catalog_retries = 0;
                next_full_catalog_refresh = Instant::now() + INDEX_FULL_CATALOG_RECOVERY_INTERVAL;
              }
              consecutive_changed_file_retries = 0;
              next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_UNWATCHED_PROVIDER_CATALOG_INTERVAL;
            } else if provider_catalog {
              let retry_soon = consecutive_unwatched_provider_catalog_retries < MAX_CONSECUTIVE_CATALOG_RETRIES;
              if retry_soon {
                consecutive_unwatched_provider_catalog_retries += 1;
                next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
              } else {
                consecutive_unwatched_provider_catalog_retries = 0;
                next_unwatched_provider_catalog_refresh = Instant::now() + INDEX_UNWATCHED_PROVIDER_CATALOG_INTERVAL;
              }
            } else {
              consecutive_changed_file_retries = 0;
              next_full_catalog_refresh = Instant::now() + INDEX_CATALOG_RETRY_INTERVAL;
            }
            let delay = next_full_catalog_refresh
              .saturating_duration_since(Instant::now())
              .min(next_unwatched_provider_catalog_refresh.saturating_duration_since(Instant::now()));
            refresh_service
              .settle_session_index_worker_error_after_refresh(IndexWorkerError::TaskFailed, retry_at_ms_after(delay));
            if let Some(wake) = wait_for_session_index_work(
              &mut retry_receiver,
              &mut session_watcher,
              &mut index_wakes,
              delay.min(Duration::from_secs(1)),
            )
            .await
            {
              refresh_service.relay.wake_sources().await;
              merge_session_index_wake(&mut pending_wake, wake);
            }
          }
        }
      }
    }));

    Self { service, events, tasks }
  }
}

fn emit_changed_relay_status(
  events: &broadcast::Sender<ViewerEvent>,
  previous: &mut Option<crate::relay::RelayStatus>,
  status: crate::relay::RelayStatus,
) {
  if previous.as_ref() != Some(&status) {
    let _ = emit(events, "relay-status", &status);
    *previous = Some(status);
  }
}

/// A native watcher keeps actively written Codex and Pi files current. This
/// slow full pass is only a recovery net for missed notifications, session
/// tree topology changes, and providers that do not have an incremental path
/// yet.
const INDEX_FULL_CATALOG_RECOVERY_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Providers without native file watching keep the former short catalog
/// cadence, but only they are enumerated. This prevents a large Codex/Pi
/// rollout tree from being rediscovered just to notice a SQLite- or
/// compressed-log-backed provider update.
const INDEX_UNWATCHED_PROVIDER_CATALOG_INTERVAL: Duration = Duration::from_secs(10);
const INDEX_PENDING_BODY_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const INDEX_CATALOG_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAX_CONSECUTIVE_CATALOG_RETRIES: u8 = 2;
const MAX_CONSECUTIVE_CHANGED_FILE_RETRIES: u8 = 3;
// Relay emits one hint per normalized record. Apply the same bounded batching
// as filesystem writes so streaming events do not each start an index pass.
const RELAY_INDEX_QUIET_PERIOD: Duration = Duration::from_millis(200);
const RELAY_INDEX_MAX_BATCH_AGE: Duration = Duration::from_secs(1);

fn retry_at_ms_after(delay: Duration) -> Option<i64> {
  SystemTime::now()
    .checked_add(delay)?
    .duration_since(UNIX_EPOCH)
    .ok()?
    .as_millis()
    .try_into()
    .ok()
}

fn session_index_needs_retry(refresh: &IndexRefresh) -> bool {
  refresh.has_pending_body_jobs
    || refresh.retry_catalog_soon
    || !refresh.retry_changed_file_paths.is_empty()
    || refresh.has_catalog_errors
}

/// A complete catalog must retry promptly when a provider could not be read
/// or its source tree changed during enumeration. The bounded scheduler retry
/// keeps a transient failure from leaving the notification center stale for a
/// full recovery interval without turning a persistent failure into a scan
/// loop.
fn full_catalog_needs_prompt_retry(refresh: &IndexRefresh) -> bool {
  refresh.retry_catalog_soon || refresh.has_catalog_errors
}

fn provider_catalog_needs_prompt_retry(refresh: &IndexRefresh) -> bool {
  refresh.retry_catalog_soon || refresh.catalog_attempt_has_errors
}

fn index_wake_from_watch_request(request: WatchRequest) -> SessionIndexWake {
  match request {
    WatchRequest::FullCatalog => SessionIndexWake::FullCatalog,
    WatchRequest::ChangedFiles(paths) => {
      let mut by_provider = BTreeMap::<_, BTreeSet<_>>::new();
      for (provider, path) in paths {
        by_provider.entry(provider).or_default().insert(path);
      }
      SessionIndexWake::ChangedFiles(by_provider)
    }
  }
}

/// A Notify backend error invalidates any individual-file request that happens
/// to arrive in the same scheduler wait. The full catalog is the only path
/// that can prove no source update was lost before the watcher is retired.
fn watcher_wake_after_backend_failure(request: WatchRequest, backend_failed: bool) -> SessionIndexWake {
  if backend_failed {
    SessionIndexWake::FullCatalog
  } else {
    index_wake_from_watch_request(request)
  }
}

/// Coalesce all scheduler requests observed before the next worker pass. A
/// topology-changing request always wins over individual file updates, since
/// a complete catalog is the only path allowed to move or tombstone sources.
fn merge_session_index_wake(current: &mut Option<SessionIndexWake>, incoming: SessionIndexWake) {
  match (current.as_mut(), incoming) {
    (Some(SessionIndexWake::FullCatalog), _) => {}
    (_, SessionIndexWake::FullCatalog) => *current = Some(SessionIndexWake::FullCatalog),
    (Some(SessionIndexWake::ChangedFiles(existing)), SessionIndexWake::ChangedFiles(next)) => {
      for (provider, paths) in next {
        existing.entry(provider).or_default().extend(paths);
      }
    }
    (None, wake) => *current = Some(wake),
    (Some(SessionIndexWake::ProviderCatalog(existing)), SessionIndexWake::ProviderCatalog(next)) => {
      existing.extend(next)
    }
    (Some(SessionIndexWake::ProviderCatalog(existing)), SessionIndexWake::ChangedFiles(next)) => {
      existing.extend(next.into_keys())
    }
    (Some(SessionIndexWake::ChangedFiles(existing)), SessionIndexWake::ProviderCatalog(mut next)) => {
      next.extend(existing.keys().copied());
      *current = Some(SessionIndexWake::ProviderCatalog(next));
    }
  }
}

fn drain_session_index_wakes(
  receiver: &mut tokio::sync::mpsc::UnboundedReceiver<SessionIndexWake>,
  current: &mut Option<SessionIndexWake>,
) {
  while let Ok(next) = receiver.try_recv() {
    merge_session_index_wake(current, next);
  }
}

enum SessionIndexWaitSignal {
  TimedOut,
  Scheduler(Option<SessionIndexWake>),
  Relay(Result<(ViewerProvider, PathBuf), broadcast::error::RecvError>),
  Watcher(Option<WatchRequest>),
}

/// One pass selected by the scheduler. Provider-local discovery preserves the
/// pre-watcher update cadence for SQLite and compressed-log providers without
/// making an active Codex/Pi append pay for a whole inventory scan.
enum SessionIndexWork {
  FullCatalog,
  ProviderCatalog(Vec<ViewerProvider>),
  ChangedFiles(BTreeMap<ViewerProvider, BTreeSet<PathBuf>>),
}

fn session_index_work_from_wake(wake: SessionIndexWake) -> SessionIndexWork {
  match wake {
    SessionIndexWake::FullCatalog => SessionIndexWork::FullCatalog,
    SessionIndexWake::ProviderCatalog(providers) => SessionIndexWork::ProviderCatalog(providers.into_iter().collect()),
    SessionIndexWake::ChangedFiles(paths) => SessionIndexWork::ChangedFiles(paths),
  }
}

fn providers_without_native_watch(watcher: &Option<SessionFileWatcher>) -> Vec<ViewerProvider> {
  let covered = watcher
    .as_ref()
    .map(SessionFileWatcher::covered_providers)
    .unwrap_or_default();
  ViewerProvider::ALL
    .into_iter()
    .filter(|provider| !covered.contains(provider))
    .collect()
}

fn index_wake_from_relay(
  hint: Result<(ViewerProvider, PathBuf), broadcast::error::RecvError>,
) -> Option<SessionIndexWake> {
  match hint {
    Ok((provider, path)) if matches!(provider, ViewerProvider::Codex | ViewerProvider::Pi) => Some(
      SessionIndexWake::ChangedFiles(BTreeMap::from([(provider, BTreeSet::from([path]))])),
    ),
    Ok((provider, _)) => Some(SessionIndexWake::ProviderCatalog(BTreeSet::from([provider]))),
    // Relay is advisory. Its historical replay can overflow this receiver;
    // watchers and periodic recovery retain correctness without an expensive
    // global rescan for records the indexer may already contain.
    Err(broadcast::error::RecvError::Lagged(_)) => None,
    Err(broadcast::error::RecvError::Closed) => None,
  }
}

async fn wait_for_session_index_work(
  receiver: &mut tokio::sync::mpsc::UnboundedReceiver<SessionIndexWake>,
  watcher: &mut Option<SessionFileWatcher>,
  relay: &mut broadcast::Receiver<(ViewerProvider, PathBuf)>,
  delay: Duration,
) -> Option<SessionIndexWake> {
  let scheduled_deadline = tokio::time::Instant::now() + delay;
  let mut batch_deadline = None;
  let mut quiet_deadline = None;
  let mut current = None;
  loop {
    let deadline = batch_deadline
      .into_iter()
      .chain(quiet_deadline)
      .fold(scheduled_deadline, tokio::time::Instant::min);
    let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
    let signal = next_session_index_signal(receiver, watcher, relay, wait).await;
    match signal {
      SessionIndexWaitSignal::TimedOut => break,
      SessionIndexWaitSignal::Scheduler(Some(request)) => {
        // Explicit retry requests and mandatory recovery never wait for a
        // stream to become quiet.
        merge_session_index_wake(&mut current, request);
        break;
      }
      SessionIndexWaitSignal::Scheduler(None) => {
        // Shutdown normally stops the runtime too. Avoid spinning if its
        // sender disappears just before that happens.
        if current.is_none() {
          tokio::time::sleep(wait).await;
        }
        break;
      }
      SessionIndexWaitSignal::Relay(hint) => {
        if matches!(hint, Err(broadcast::error::RecvError::Closed)) {
          if current.is_none() {
            tokio::time::sleep(wait).await;
          }
          break;
        }
        if let Some(wake) = index_wake_from_relay(hint) {
          merge_session_index_wake(&mut current, wake);
          let now = tokio::time::Instant::now();
          batch_deadline.get_or_insert(now + RELAY_INDEX_MAX_BATCH_AGE);
          quiet_deadline = Some(now + RELAY_INDEX_QUIET_PERIOD);
        }
      }
      SessionIndexWaitSignal::Watcher(request) => {
        let wake = index_wake_from_watcher(request, watcher);
        merge_session_index_wake(&mut current, wake);
        // A file watcher already debounced its batch. It can join a pending
        // Relay batch, but does not restart that batch's quiet period.
        if batch_deadline.is_none() || matches!(current, Some(SessionIndexWake::FullCatalog)) {
          break;
        }
      }
    }
    // An always-ready Relay stream must still yield at the maximum batch age
    // or a scheduled recovery deadline; select fairness is not a time bound.
    if tokio::time::Instant::now() >= deadline {
      break;
    }
  }
  drain_session_index_wakes(receiver, &mut current);
  // Drain only the batch already queued. New arrivals belong to the next
  // bounded wait rather than extending a busy stream past its deadline.
  for _ in 0..relay.len() {
    let wake = match relay.try_recv() {
      Ok(hint) => index_wake_from_relay(Ok(hint)),
      Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
      Err(_) => break,
    };
    if let Some(wake) = wake {
      merge_session_index_wake(&mut current, wake);
    }
  }
  current
}

async fn next_session_index_signal(
  receiver: &mut tokio::sync::mpsc::UnboundedReceiver<SessionIndexWake>,
  watcher: &mut Option<SessionFileWatcher>,
  relay: &mut broadcast::Receiver<(ViewerProvider, PathBuf)>,
  delay: Duration,
) -> SessionIndexWaitSignal {
  if let Some(file_watcher) = watcher.as_mut() {
    tokio::select! {
      _ = tokio::time::sleep(delay) => SessionIndexWaitSignal::TimedOut,
      request = receiver.recv() => SessionIndexWaitSignal::Scheduler(request),
      hint = relay.recv() => SessionIndexWaitSignal::Relay(hint),
      request = file_watcher.next_request() => SessionIndexWaitSignal::Watcher(request),
    }
  } else {
    tokio::select! {
      _ = tokio::time::sleep(delay) => SessionIndexWaitSignal::TimedOut,
      request = receiver.recv() => SessionIndexWaitSignal::Scheduler(request),
      hint = relay.recv() => SessionIndexWaitSignal::Relay(hint),
    }
  }
}

fn index_wake_from_watcher(
  request: Option<WatchRequest>,
  watcher: &mut Option<SessionFileWatcher>,
) -> SessionIndexWake {
  match request {
    Some(request) => {
      let backend_failed = watcher.as_mut().is_some_and(SessionFileWatcher::take_backend_failure);
      if backend_failed {
        // A single complete catalog reconciles anything lost with the failed
        // backend, then the scheduler downgrades to its bounded provider-local
        // cadence. Keeping an erroring watcher alive would turn every repeat
        // error into another immediate all-provider scan.
        *watcher = None;
      }
      // The callback that won the select can be an ordinary file write while
      // a backend error is already queued behind it. Do not let that race
      // retire the watcher without the promised recovery catalog.
      watcher_wake_after_backend_failure(request, backend_failed)
    }
    None => {
      // A closed watcher callback channel cannot be trusted for subsequent
      // file updates. Disable it and use one complete catalog to reconcile
      // anything it may have missed; the slow recovery cadence remains safe.
      *watcher = None;
      SessionIndexWake::FullCatalog
    }
  }
}

fn session_index_path_for_home(home: &Path) -> PathBuf {
  home.join(".tokn").join("sessions").join("index.sqlite")
}

pub fn session_index_path() -> Result<PathBuf, String> {
  dirs::home_dir()
    .map(|home| session_index_path_for_home(&home))
    .ok_or_else(|| "could not resolve the user home directory for the session index".to_owned())
}

#[cfg(test)]
mod tests {
  use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
  };

  use super::{
    IndexRefresh, SessionIndexWake, full_catalog_needs_prompt_retry, index_wake_from_relay,
    index_wake_from_watch_request, merge_session_index_wake, session_index_needs_retry, session_index_path_for_home,
    wait_for_session_index_work, watcher_wake_after_backend_failure,
  };
  use crate::{model::ViewerProvider, watcher::WatchRequest};

  #[tokio::test]
  async fn session_updates_do_not_repeat_status_but_errors_and_recovery_do() {
    let (events, mut receiver) = tokio::sync::broadcast::channel(128);
    let manager = crate::relay::ViewerRelay::new();
    let mut previous = None;
    let mut status = manager.status();
    for _ in 0..100 {
      super::emit(
        &events,
        "relay-changed",
        crate::relay::RelayChange {
          session_key: Some("active".into()),
          reset: false,
        },
      )
      .unwrap();
      super::emit_changed_relay_status(&events, &mut previous, status.clone());
    }
    status.error = Some("source unavailable".into());
    super::emit_changed_relay_status(&events, &mut previous, status.clone());
    status.error = None;
    super::emit_changed_relay_status(&events, &mut previous, status);
    let mut changes = 0;
    let mut statuses = Vec::new();
    while let Ok(event) = receiver.try_recv() {
      match event.event.as_str() {
        "relay-changed" => changes += 1,
        "relay-status" => statuses.push(event.payload),
        _ => panic!("unexpected event"),
      }
    }
    assert_eq!(changes, 100);
    assert_eq!(statuses.len(), 3);
    assert_eq!(statuses[1]["error"], "source unavailable");
    assert!(statuses[2]["error"].is_null());
  }

  #[test]
  fn lagged_relay_hints_do_not_force_a_global_catalog() {
    assert!(index_wake_from_relay(Err(tokio::sync::broadcast::error::RecvError::Lagged(500))).is_none());
  }

  #[tokio::test(start_paused = true)]
  async fn relay_event_burst_produces_one_deduplicated_index_pass() {
    let (_retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel();
    let (relay_tx, mut relay_rx) = tokio::sync::broadcast::channel(256);
    let paths = [
      PathBuf::from("/sessions/first.jsonl"),
      PathBuf::from("/sessions/second.jsonl"),
    ];
    let expected = BTreeSet::from(paths.clone());
    let producer = tokio::spawn(async move {
      for i in 0..40 {
        relay_tx.send((ViewerProvider::Codex, paths[i % 2].clone())).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
      relay_tx
    });
    let started = tokio::time::Instant::now();
    let mut watcher = None;
    let wake = wait_for_session_index_work(&mut retry_rx, &mut watcher, &mut relay_rx, Duration::from_secs(5)).await;
    let _relay_tx = producer.await.unwrap();
    assert_eq!(
      wake,
      Some(SessionIndexWake::ChangedFiles(BTreeMap::from([(
        ViewerProvider::Codex,
        expected
      )])))
    );
    assert_eq!(started.elapsed(), Duration::from_millis(395));
    assert!(
      wait_for_session_index_work(&mut retry_rx, &mut watcher, &mut relay_rx, Duration::from_millis(50))
        .await
        .is_none()
    );
  }

  #[tokio::test(start_paused = true)]
  async fn continuous_relay_hints_yield_at_the_maximum_batch_age() {
    let (_retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel();
    let (relay_tx, mut relay_rx) = tokio::sync::broadcast::channel(256);
    let producer = tokio::spawn(async move {
      loop {
        relay_tx
          .send((ViewerProvider::Codex, PathBuf::from("/sessions/active.jsonl")))
          .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
      }
    });
    let started = tokio::time::Instant::now();
    let wake = wait_for_session_index_work(&mut retry_rx, &mut None, &mut relay_rx, Duration::from_secs(5)).await;
    producer.abort();
    assert!(matches!(wake, Some(SessionIndexWake::ChangedFiles(_))));
    assert_eq!(started.elapsed(), Duration::from_secs(1));
  }

  #[tokio::test(start_paused = true)]
  async fn relay_batch_does_not_postpone_a_scheduled_deadline() {
    let (_retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel();
    let (relay_tx, mut relay_rx) = tokio::sync::broadcast::channel(256);
    relay_tx
      .send((ViewerProvider::Codex, PathBuf::from("/sessions/active.jsonl")))
      .unwrap();
    let started = tokio::time::Instant::now();
    let wake = wait_for_session_index_work(&mut retry_rx, &mut None, &mut relay_rx, Duration::from_millis(30)).await;
    assert!(matches!(wake, Some(SessionIndexWake::ChangedFiles(_))));
    assert_eq!(started.elapsed(), Duration::from_millis(30));
  }

  #[tokio::test(start_paused = true)]
  async fn explicit_recovery_interrupts_and_subsumes_a_relay_batch() {
    let (retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel();
    let (relay_tx, mut relay_rx) = tokio::sync::broadcast::channel(256);
    relay_tx
      .send((ViewerProvider::Codex, PathBuf::from("/sessions/active.jsonl")))
      .unwrap();
    tokio::spawn(async move {
      tokio::time::sleep(Duration::from_millis(20)).await;
      retry_tx.send(SessionIndexWake::FullCatalog).unwrap();
    });
    let started = tokio::time::Instant::now();
    assert_eq!(
      wait_for_session_index_work(&mut retry_rx, &mut None, &mut relay_rx, Duration::from_secs(5)).await,
      Some(SessionIndexWake::FullCatalog)
    );
    assert_eq!(started.elapsed(), Duration::from_millis(20));
  }

  #[test]
  fn session_index_uses_the_shared_tokn_sessions_directory() {
    assert_eq!(
      session_index_path_for_home(Path::new("/example/home")),
      Path::new("/example/home/.tokn/sessions/index.sqlite"),
    );
  }

  #[test]
  fn scheduler_keeps_catalog_failures_in_a_waiting_state() {
    assert!(session_index_needs_retry(&IndexRefresh {
      has_catalog_errors: true,
      ..Default::default()
    }));
    assert!(!session_index_needs_retry(&IndexRefresh::default()));
  }

  #[test]
  fn scheduler_retries_catalog_errors_promptly_before_falling_back_to_recovery() {
    assert!(full_catalog_needs_prompt_retry(&IndexRefresh {
      has_catalog_errors: true,
      ..Default::default()
    }));
    assert!(full_catalog_needs_prompt_retry(&IndexRefresh {
      retry_catalog_soon: true,
      ..Default::default()
    }));
    assert!(!full_catalog_needs_prompt_retry(&IndexRefresh::default()));
  }

  #[test]
  fn scheduler_keeps_a_changed_file_retry_visible() {
    assert!(session_index_needs_retry(&IndexRefresh {
      retry_changed_file_paths: BTreeMap::from([(
        ViewerProvider::Codex,
        BTreeSet::from([PathBuf::from("/sessions/codex/active.jsonl")]),
      )]),
      ..Default::default()
    }));
  }

  #[test]
  fn watcher_wakes_deduplicate_files_and_a_full_catalog_wins() {
    let codex_path = PathBuf::from("/sessions/codex/active.jsonl");
    let pi_path = PathBuf::from("/sessions/pi/active.jsonl");
    let mut wake = Some(index_wake_from_watch_request(WatchRequest::ChangedFiles(vec![
      (ViewerProvider::Codex, codex_path.clone()),
      (ViewerProvider::Codex, codex_path.clone()),
    ])));
    merge_session_index_wake(
      &mut wake,
      index_wake_from_watch_request(WatchRequest::ChangedFiles(vec![(ViewerProvider::Pi, pi_path.clone())])),
    );
    assert_eq!(
      wake,
      Some(SessionIndexWake::ChangedFiles(BTreeMap::from([
        (ViewerProvider::Codex, BTreeSet::from([codex_path])),
        (ViewerProvider::Pi, BTreeSet::from([pi_path])),
      ])))
    );

    merge_session_index_wake(&mut wake, SessionIndexWake::FullCatalog);
    merge_session_index_wake(
      &mut wake,
      SessionIndexWake::ChangedFiles(BTreeMap::from([(
        ViewerProvider::Codex,
        BTreeSet::from([PathBuf::from("/sessions/codex/later.jsonl")]),
      )])),
    );
    assert_eq!(wake, Some(SessionIndexWake::FullCatalog));
  }

  #[test]
  fn scheduler_upgrades_a_file_wake_when_the_watcher_backend_failed() {
    let request = WatchRequest::ChangedFiles(vec![(
      ViewerProvider::Codex,
      PathBuf::from("/sessions/codex/active.jsonl"),
    )]);

    assert_eq!(
      watcher_wake_after_backend_failure(request, true),
      SessionIndexWake::FullCatalog,
    );
  }
}
