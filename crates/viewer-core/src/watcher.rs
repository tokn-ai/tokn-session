//! Native filesystem watching for the indexed file-backed session providers.
//!
//! The index worker is the source of truth: filesystem notifications only
//! decide whether it may safely inspect a known JSONL file directly or must
//! fall back to its durable catalog pass. This module intentionally knows
//! nothing about SQLite, session decoding, or Tauri events so it can stay
//! small and testable.

use std::{
  collections::{BTreeSet, HashMap, HashSet},
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};

// The relay deliberately selects Notify 8/Kqueue on macOS. This viewer uses
// its target-local Notify 7 dependency there so `RecommendedWatcher` stays
// FSEvents and a large recursive Codex history does not consume one descriptor
// per rollout file. Other platforms retain the workspace's Notify dependency.
#[cfg(target_os = "macos")]
use notify_fsevent as notify;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind};
use tokio::{
  sync::mpsc,
  time::{Instant, sleep_until},
};

use crate::model::ViewerProvider;

/// Coalesce a short burst of native notifications before asking the indexer
/// for work. Appending one JSONL record commonly produces both data and
/// metadata notifications on the same path.
const WATCH_QUIET_PERIOD: Duration = Duration::from_millis(200);
/// Do not defer an active session indefinitely while writes keep arriving.
const WATCH_MAX_BATCH_AGE: Duration = Duration::from_secs(1);
/// Native backends can emit several records for every append. Bound callback
/// buffering so a sustained filesystem storm cannot grow the viewer process;
/// an overflow intentionally upgrades to one safe full catalog.
const NATIVE_WATCH_CHANNEL_CAPACITY: usize = 1_024;
/// Notify's FSEvents backend starts its run loop asynchronously. Its own
/// native tests wait two seconds after registration before relying on the
/// stream, so the scheduler gives the first catalog the same short guard.
#[cfg(target_os = "macos")]
const FSEVENT_STARTUP_GUARD: Duration = Duration::from_secs(2);

/// Work requested by a filesystem notification.
///
/// `ChangedFiles` covers ordinary JSONL writes plus file creations that will
/// be identity-checked by the indexer. An unfamiliar, moved, or replaced file
/// fails that targeted check and immediately requests `FullCatalog`; this lets
/// FSEvents' stale initial file-create records avoid a redundant startup scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WatchRequest {
  ChangedFiles(Vec<(ViewerProvider, PathBuf)>),
  FullCatalog,
}

impl WatchRequest {
  fn is_empty(&self) -> bool {
    matches!(self, Self::ChangedFiles(paths) if paths.is_empty())
  }
}

/// A batch that has started collecting native callbacks but has not yet
/// reached its quiet or maximum-age deadline.
///
/// This lives on [`SessionFileWatcher`] instead of inside `next_request` so
/// Tokio may cancel a wait for a scheduler timer without discarding a callback
/// that the watcher has already received.
struct PendingWatchBatch {
  coalescer: WatchRequestCoalescer,
  oldest_deadline: Instant,
  quiet_deadline: Instant,
}

/// One configured provider root and the path spelling used by the native
/// watcher backend.
///
/// The durable index keeps the provider's original root spelling. macOS
/// FSEvents can instead report a canonicalized spelling (such as `/private`
/// for a `/var` root), so callback paths are rebased back onto `source_root`
/// before they reach the indexer.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WatchRoot {
  provider: ViewerProvider,
  source_root: PathBuf,
  watched_root: PathBuf,
}

impl WatchRoot {
  fn new(provider: ViewerProvider, source_root: PathBuf) -> Self {
    let watched_root = canonical_watcher_root(&source_root);
    Self {
      provider,
      source_root,
      watched_root,
    }
  }
}

/// Retains the native watcher and turns its callbacks into coalesced index
/// work. Roots are `(provider, path)` pairs so callers can resolve provider
/// configuration without introducing a second shared root model.
pub(crate) struct SessionFileWatcher {
  watcher: RecommendedWatcher,
  roots: Vec<WatchRoot>,
  /// The strongest mode registered for each native path. Each session root
  /// also has a non-recursive parent registration, so this records both the
  /// rollout tree and its lifecycle boundary.
  watched_paths: HashMap<PathBuf, RecursiveMode>,
  watched_roots: HashSet<WatchRoot>,
  wake_rx: mpsc::Receiver<Result<Event, String>>,
  wake_overflowed: Arc<AtomicBool>,
  /// The Notify callback can observe an error just as its bounded channel
  /// overflows, so this must be shared with the callback rather than inferred
  /// only while the scheduler drains queued wakes.
  backend_failed: Arc<AtomicBool>,
  pending: Option<PendingWatchBatch>,
  initial_watch_errors: Vec<String>,
}

impl SessionFileWatcher {
  /// Registers native watches for the supplied Codex and Pi session roots.
  ///
  /// Existing roots are watched recursively. If only the final root directory
  /// is absent, its existing parent is watched non-recursively so creation of
  /// that root can still request a safe catalog refresh. Deeper absent paths
  /// are left to the periodic recovery catalog rather than recursively
  /// watching an unexpectedly broad ancestor.
  pub(crate) fn new(roots: Vec<(ViewerProvider, PathBuf)>) -> Result<Self, String> {
    let mut unique_roots = Vec::with_capacity(roots.len());
    let mut seen_roots = HashSet::with_capacity(roots.len());
    for (provider, root) in roots {
      let root = WatchRoot::new(provider, root);
      if seen_roots.insert(root.clone()) {
        unique_roots.push(root);
      }
    }

    let (wake_tx, wake_rx) = mpsc::channel(NATIVE_WATCH_CHANNEL_CAPACITY);
    let wake_overflowed = Arc::new(AtomicBool::new(false));
    let callback_overflowed = Arc::clone(&wake_overflowed);
    let backend_failed = Arc::new(AtomicBool::new(false));
    let callback_backend_failed = Arc::clone(&backend_failed);
    let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
      let wake = result.map_err(|error| error.to_string());
      if wake.is_err() {
        // Record this before `try_send`: the error itself may be the callback
        // dropped by a full channel, but it still means this watcher must be
        // retired after the recovery catalog.
        callback_backend_failed.store(true, Ordering::Release);
      }
      if matches!(wake_tx.try_send(wake), Err(mpsc::error::TrySendError::Full(_))) {
        // A full channel means a callback was intentionally dropped, so the
        // indexer must reconcile topology rather than trust a partial target
        // batch. A closed receiver only occurs during shutdown.
        callback_overflowed.store(true, Ordering::Release);
      }
    })
    .map_err(|error| format!("failed to create session filesystem watcher: {error}"))?;

    let mut session_watcher = Self {
      watcher,
      roots: unique_roots,
      watched_paths: HashMap::new(),
      watched_roots: HashSet::new(),
      wake_rx,
      wake_overflowed,
      backend_failed,
      pending: None,
      initial_watch_errors: Vec::new(),
    };
    session_watcher.initial_watch_errors = session_watcher.refresh_watches();
    Ok(session_watcher)
  }

  /// Registers roots that appeared after construction.
  ///
  /// Call this after a full catalog pass. It is idempotent and leaves existing
  /// watches in place, which makes it safe to invoke after every recovery
  /// scan. Existing roots retain a non-recursive parent watch alongside their
  /// recursive tree watch so a later root rename/removal is observable.
  pub(crate) fn refresh_watches(&mut self) -> Vec<String> {
    let mut errors = Vec::new();
    for root in self.roots.clone() {
      let targets = watch_targets(&root.watched_root);
      if targets.is_empty() {
        self.watched_roots.remove(&root);
        continue;
      }
      let mut root_is_covered = true;
      for (path, mode) in targets {
        if let Err(error) = self.ensure_watch(&path, mode) {
          errors.push(error);
          root_is_covered = false;
        }
      }
      if root_is_covered {
        self.watched_roots.insert(root);
      } else {
        // A recursive tree without its lifecycle-parent registration is still
        // useful for best-effort notifications, but it is not reliable enough
        // to skip the short provider-local recovery cadence.
        self.watched_roots.remove(&root);
      }
    }
    errors
  }

  fn ensure_watch(&mut self, path: &Path, mode: RecursiveMode) -> Result<(), String> {
    if self.watch_mode_covers(path, mode) {
      return Ok(());
    }
    if self.watched_paths.contains_key(path) {
      // A root can also be another configured root's lifecycle parent. Upgrade
      // that registration when necessary; recursive coverage safely subsumes
      // the earlier non-recursive request.
      self
        .watcher
        .unwatch(path)
        .map_err(|error| format!("failed to upgrade watch {}: {error}", path.display()))?;
      self.watched_paths.remove(path);
    }
    self
      .watcher
      .watch(path, mode)
      .map_err(|error| format!("failed to watch {}: {error}", path.display()))?;
    self.watched_paths.insert(path.to_path_buf(), mode);
    Ok(())
  }

  fn watch_mode_covers(&self, path: &Path, requested_mode: RecursiveMode) -> bool {
    self.watched_paths.get(path).is_some_and(|registered_mode| {
      matches!(registered_mode, RecursiveMode::Recursive) || matches!(requested_mode, RecursiveMode::NonRecursive)
    })
  }

  /// Returns failures from the initial per-root registration attempt.
  ///
  /// A single unwatcheable history tree must not disable the other provider's
  /// incremental path. The caller logs these diagnostics and the next full
  /// catalog attempts the failed roots again.
  pub(crate) fn take_initial_watch_errors(&mut self) -> Vec<String> {
    std::mem::take(&mut self.initial_watch_errors)
  }

  /// Returns whether Notify reported a backend error since the scheduler last
  /// checked. The caller disables this watcher after one recovery catalog so a
  /// recurring backend failure cannot trigger an all-provider scan loop.
  pub(crate) fn take_backend_failure(&mut self) -> bool {
    self.backend_failed.swap(false, Ordering::AcqRel)
  }

  /// Returns providers whose configured file roots are all covered by a
  /// native registration. The scheduler keeps a short provider-local catalog
  /// cadence for every other source so a partial watcher failure does not turn
  /// into a five-minute update delay.
  pub(crate) fn covered_providers(&self) -> BTreeSet<ViewerProvider> {
    self
      .roots
      .iter()
      .map(|root| root.provider)
      .collect::<BTreeSet<_>>()
      .into_iter()
      .filter(|provider| {
        self
          .roots
          .iter()
          .filter(|root| root.provider == *provider)
          .all(|root| self.watched_roots.contains(root))
      })
      .collect()
  }

  /// Gives macOS FSEvents time to start before the first catalog snapshot.
  /// The viewer keeps a prior SQLite index responsive during this short
  /// background-only guard; it prevents a write between watcher registration
  /// and stream activation from escaping both the watcher and the first scan.
  pub(crate) fn startup_guard(&self) -> Duration {
    #[cfg(target_os = "macos")]
    {
      if self.watched_paths.is_empty() {
        Duration::ZERO
      } else {
        FSEVENT_STARTUP_GUARD
      }
    }
    #[cfg(not(target_os = "macos"))]
    {
      Duration::ZERO
    }
  }

  /// Waits for a meaningful, coalesced filesystem request.
  ///
  /// `None` means the callback channel closed, which callers should treat as
  /// a failed watcher and recover with their regular full catalog cadence.
  pub(crate) async fn next_request(&mut self) -> Option<WatchRequest> {
    loop {
      if self.take_wake_overflow() {
        return Some(WatchRequest::FullCatalog);
      }
      if self.pending.is_none() {
        let first = self.wake_rx.recv().await?;
        self.start_pending_batch(first);
        // `start_pending_batch` synchronously records a meaningful callback
        // before this method can await again. If a surrounding select cancels
        // us after this point, the next call resumes the same batch.
        continue;
      }

      if self
        .pending
        .as_ref()
        .is_some_and(|batch| batch.coalescer.requires_full_catalog())
      {
        self.drain_wakes_into_pending_batch();
        if let Some(request) = self.finish_pending_batch() {
          return Some(request);
        }
        continue;
      }

      let (oldest_deadline, quiet_deadline) = {
        let batch = self.pending.as_ref().expect("pending batch should exist");
        (batch.oldest_deadline, batch.quiet_deadline)
      };
      tokio::select! {
        _ = sleep_until(oldest_deadline) => return self.finish_pending_batch(),
        _ = sleep_until(quiet_deadline) => return self.finish_pending_batch(),
        wake = self.wake_rx.recv() => match wake {
          Some(wake) => self.merge_wake_into_pending_batch(wake),
          // Deliver an already collected request before reporting the failed
          // callback channel on the following scheduler wait.
          None => return self.finish_pending_batch(),
        },
      }
    }
  }

  fn start_pending_batch(&mut self, wake: Result<Event, String>) {
    let mut coalescer = WatchRequestCoalescer::default();
    coalescer.push(self.classify_wake(wake));
    if coalescer.is_empty() {
      return;
    }

    let now = Instant::now();
    self.pending = Some(PendingWatchBatch {
      coalescer,
      oldest_deadline: now + WATCH_MAX_BATCH_AGE,
      quiet_deadline: now + WATCH_QUIET_PERIOD,
    });
  }

  fn take_wake_overflow(&mut self) -> bool {
    if !self.wake_overflowed.swap(false, Ordering::AcqRel) {
      return false;
    }
    // A full catalog subsumes both an already coalesced subset and any queued
    // callbacks that preceded the overflow. Draining keeps an event storm from
    // immediately producing a stale second batch after that catalog commits.
    self.pending = None;
    for _ in 0..NATIVE_WATCH_CHANNEL_CAPACITY {
      if self.wake_rx.try_recv().is_err() {
        break;
      }
    }
    true
  }

  fn merge_wake_into_pending_batch(&mut self, wake: Result<Event, String>) {
    let request = self.classify_wake(wake);
    let Some(batch) = self.pending.as_mut() else {
      self.start_pending_batch_from_request(request);
      return;
    };
    let extends_quiet_period = request.is_some();
    batch.coalescer.push(request);
    if extends_quiet_period && !batch.coalescer.requires_full_catalog() {
      batch.quiet_deadline = Instant::now() + WATCH_QUIET_PERIOD;
    }
  }

  fn start_pending_batch_from_request(&mut self, request: Option<WatchRequest>) {
    let mut coalescer = WatchRequestCoalescer::default();
    coalescer.push(request);
    if coalescer.is_empty() {
      return;
    }

    let now = Instant::now();
    self.pending = Some(PendingWatchBatch {
      coalescer,
      oldest_deadline: now + WATCH_MAX_BATCH_AGE,
      quiet_deadline: now + WATCH_QUIET_PERIOD,
    });
  }

  fn drain_wakes_into_pending_batch(&mut self) {
    for _ in 0..NATIVE_WATCH_CHANNEL_CAPACITY {
      let Ok(wake) = self.wake_rx.try_recv() else {
        break;
      };
      self.merge_wake_into_pending_batch(wake);
    }
  }

  fn finish_pending_batch(&mut self) -> Option<WatchRequest> {
    self.pending.take().and_then(|batch| batch.coalescer.finish())
  }

  fn classify_wake(&mut self, wake: Result<Event, String>) -> Option<WatchRequest> {
    if wake.is_err() {
      self.backend_failed.store(true, Ordering::Release);
    }
    if let Ok(event) = &wake {
      self.forget_invalidated_watches(event);
    }
    classify_watcher_wake_for_roots(&self.roots, wake)
  }

  fn forget_invalidated_watches(&mut self, event: &Event) {
    if !matches!(
      event.kind,
      EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    ) {
      return;
    }
    let mut registrations_to_remove = HashSet::new();
    let mut roots_to_remove = HashSet::new();
    for path in &event.paths {
      let watched_path = canonical_callback_path(path);
      if self.watched_paths.contains_key(&watched_path) {
        registrations_to_remove.insert(watched_path.clone());
      }
      for root in &self.roots {
        let parent_was_invalidated = lifecycle_parent_was_invalidated(root, path, &watched_path);
        if root_watch_was_invalidated(root, path, &watched_path) || parent_was_invalidated {
          roots_to_remove.insert(root.clone());
          registrations_to_remove.insert(root.watched_root.clone());
          if parent_was_invalidated && let Some(parent) = root.watched_root.parent() {
            registrations_to_remove.insert(parent.to_path_buf());
          }
        }
      }
    }
    for root in roots_to_remove {
      self.watched_roots.remove(&root);
    }
    for path in registrations_to_remove {
      self.unregister_watch(&path);
    }
  }

  /// Drops a registration from both Notify and the local coverage model. If a
  /// native backend cannot confirm the unregister, retire this watcher after
  /// the pending full catalog rather than re-registering a path that may still
  /// have a stale native stream attached.
  fn unregister_watch(&mut self, path: &Path) {
    if self.watched_paths.remove(path).is_some() {
      if self.watcher.unwatch(path).is_err() {
        self.backend_failed.store(true, Ordering::Release);
      }
    }
  }

  #[cfg(test)]
  fn with_test_wakes(roots: Vec<(ViewerProvider, PathBuf)>) -> (Self, mpsc::Sender<Result<Event, String>>) {
    let (wake_tx, wake_rx) = mpsc::channel(NATIVE_WATCH_CHANNEL_CAPACITY);
    let watcher = notify::recommended_watcher(|_| {}).expect("test watcher should initialize");
    (
      Self {
        watcher,
        roots: roots
          .into_iter()
          .map(|(provider, root)| WatchRoot::new(provider, root))
          .collect(),
        watched_paths: HashMap::new(),
        watched_roots: HashSet::new(),
        wake_rx,
        wake_overflowed: Arc::new(AtomicBool::new(false)),
        backend_failed: Arc::new(AtomicBool::new(false)),
        pending: None,
        initial_watch_errors: Vec::new(),
      },
      wake_tx,
    )
  }
}

/// Classifies one Notify event without doing provider I/O.
///
/// This is `pub(crate)` so scheduler-level tests can verify edge cases without
/// creating a platform watcher. Paths outside the configured session roots are
/// ignored: an absent root may cause us to watch its parent non-recursively,
/// and that parent can contain unrelated application state.
#[cfg(test)]
pub(crate) fn classify_session_file_event(roots: &[(ViewerProvider, PathBuf)], event: &Event) -> Option<WatchRequest> {
  let roots = roots
    .iter()
    .cloned()
    .map(|(provider, root)| WatchRoot::new(provider, root))
    .collect::<Vec<_>>();
  classify_session_file_event_for_roots(&roots, event)
}

fn classify_session_file_event_for_roots(roots: &[WatchRoot], event: &Event) -> Option<WatchRequest> {
  if event.need_rescan() || event.paths.is_empty() && !matches!(event.kind, EventKind::Access(_)) {
    return Some(WatchRequest::FullCatalog);
  }
  // A session root's parent is deliberately registered as a lightweight
  // lifecycle watch. It lies outside the root itself, so it cannot be found
  // by the ordinary child-path rebasing below; recognize it before filtering
  // affected paths so a root rename/removal immediately reconciles topology.
  if matches!(
    event.kind,
    EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
  ) && event.paths.iter().any(|path| {
    let watched_path = canonical_callback_path(path);
    roots.iter().any(|root| {
      root_watch_was_invalidated(root, path, &watched_path)
        || lifecycle_parent_was_invalidated(root, path, &watched_path)
    })
  }) {
    return Some(WatchRequest::FullCatalog);
  }

  let affected_paths = event
    .paths
    .iter()
    .filter_map(|path| {
      let source_paths = source_paths_for_event(roots, path).collect::<Vec<_>>();
      (!source_paths.is_empty()).then_some((path, source_paths))
    })
    .collect::<Vec<_>>();
  if affected_paths.is_empty() || matches!(event.kind, EventKind::Access(_)) {
    return None;
  }

  if is_targeted_file_change_kind(&event.kind) {
    let mut changed_files = Vec::new();
    let mut seen_files = HashSet::new();
    for (path, source_paths) in affected_paths {
      if is_directory_path(roots, path) {
        return Some(WatchRequest::FullCatalog);
      }
      if is_compressed_codex_history(path)
        && source_paths
          .iter()
          .any(|(provider, _)| *provider == ViewerProvider::Codex)
      {
        // Compressing a rollout can replace an inherited prefix for several
        // sessions. Reconcile the catalog rather than treating packed bytes
        // as an append to one established JSONL source.
        return Some(WatchRequest::FullCatalog);
      }
      if !is_session_file(path) {
        continue;
      }
      for changed_file in source_paths {
        if seen_files.insert(changed_file.clone()) {
          changed_files.push(changed_file);
        }
      }
    }
    return (!changed_files.is_empty()).then_some(WatchRequest::ChangedFiles(changed_files));
  }

  // Removals, renames, and unfamiliar modification kinds can retire or
  // replace a source, so only a complete catalog can preserve relocation and
  // unread semantics. Windows is the one exception: ReadDirectoryChangesW
  // reports an ordinary file append as `ModifyKind::Any`, which the helper
  // above recognizes as the safe one-file path.
  Some(WatchRequest::FullCatalog)
}

fn is_targeted_file_change_kind(kind: &EventKind) -> bool {
  match kind {
    EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Metadata(_)) => true,
    // Notify's Windows ReadDirectoryChangesW backend maps FILE_ACTION_MODIFIED
    // to `ModifyKind::Any`; the path and extension checks above still reject
    // directories and route unknown JSONL files through the identity guard.
    #[cfg(target_os = "windows")]
    EventKind::Modify(ModifyKind::Any) => true,
    _ => false,
  }
}

#[cfg(test)]
fn classify_watcher_wake(roots: &[(ViewerProvider, PathBuf)], wake: Result<Event, String>) -> Option<WatchRequest> {
  let roots = roots
    .iter()
    .cloned()
    .map(|(provider, root)| WatchRoot::new(provider, root))
    .collect::<Vec<_>>();
  classify_watcher_wake_for_roots(&roots, wake)
}

fn classify_watcher_wake_for_roots(roots: &[WatchRoot], wake: Result<Event, String>) -> Option<WatchRequest> {
  match wake {
    Ok(event) => classify_session_file_event_for_roots(roots, &event),
    // Native watcher errors do not identify a trustworthy subset of changed
    // files. Let the durable catalog re-establish the complete topology.
    Err(_) => Some(WatchRequest::FullCatalog),
  }
}

#[derive(Default)]
struct WatchRequestCoalescer {
  full_catalog: bool,
  changed_files: Vec<(ViewerProvider, PathBuf)>,
  seen_changed_files: HashSet<(ViewerProvider, PathBuf)>,
}

impl WatchRequestCoalescer {
  fn push(&mut self, request: Option<WatchRequest>) {
    match request {
      None => {}
      Some(WatchRequest::ChangedFiles(paths)) if paths.is_empty() => {}
      Some(WatchRequest::FullCatalog) => {
        self.full_catalog = true;
        self.changed_files.clear();
        self.seen_changed_files.clear();
      }
      Some(WatchRequest::ChangedFiles(paths)) if !self.full_catalog => {
        for changed_file in paths {
          if self.seen_changed_files.insert(changed_file.clone()) {
            self.changed_files.push(changed_file);
          }
        }
      }
      Some(WatchRequest::ChangedFiles(_)) => {}
    }
  }

  fn requires_full_catalog(&self) -> bool {
    self.full_catalog
  }

  fn is_empty(&self) -> bool {
    !self.full_catalog && self.changed_files.is_empty()
  }

  fn finish(self) -> Option<WatchRequest> {
    if self.full_catalog {
      return Some(WatchRequest::FullCatalog);
    }
    let request = WatchRequest::ChangedFiles(self.changed_files);
    (!request.is_empty()).then_some(request)
  }
}

/// Returns the registrations needed to follow one configured session root.
///
/// The parent is always watched non-recursively when it exists. Notify cannot
/// promise useful events after a watched directory itself is renamed or
/// removed, but this parent registration observes that root lifecycle event.
/// When the root exists, its recursive watch still carries ordinary rollout
/// writes. A missing root deliberately watches only its immediate existing
/// parent, never a broader ancestor.
fn watch_targets(root: &Path) -> Vec<(PathBuf, RecursiveMode)> {
  let mut targets = Vec::with_capacity(2);
  if let Some(parent) = root.parent().filter(|parent| parent.exists()) {
    targets.push((parent.to_path_buf(), RecursiveMode::NonRecursive));
  }
  if root.exists() {
    let mode = if root.is_dir() {
      RecursiveMode::Recursive
    } else {
      RecursiveMode::NonRecursive
    };
    if !targets.iter().any(|(path, _)| path == root) {
      targets.push((root.to_path_buf(), mode));
    }
  }
  targets
}

fn canonical_watcher_root(root: &Path) -> PathBuf {
  for ancestor in root.ancestors() {
    let Ok(canonical_ancestor) = ancestor.canonicalize() else {
      continue;
    };
    let Ok(suffix) = root.strip_prefix(ancestor) else {
      continue;
    };
    return canonical_ancestor.join(suffix);
  }
  root.to_path_buf()
}

fn canonical_callback_path(path: &Path) -> PathBuf {
  path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn rebase_event_path(root: &WatchRoot, path: &Path) -> Option<PathBuf> {
  let watched_path = canonical_callback_path(path);
  watched_path
    .strip_prefix(&root.watched_root)
    .ok()
    .or_else(|| path.strip_prefix(&root.source_root).ok())
    .map(|suffix| root.source_root.join(suffix))
}

fn root_watch_was_invalidated(root: &WatchRoot, path: &Path, watched_path: &Path) -> bool {
  path == root.source_root || watched_path == root.watched_root
}

fn lifecycle_parent_was_invalidated(root: &WatchRoot, path: &Path, watched_path: &Path) -> bool {
  root.watched_root.parent().is_some_and(|parent| watched_path == parent)
    || root.source_root.parent().is_some_and(|parent| path == parent)
}

fn source_paths_for_event<'a>(
  roots: &'a [WatchRoot],
  path: &'a Path,
) -> impl Iterator<Item = (ViewerProvider, PathBuf)> + 'a {
  roots
    .iter()
    .filter_map(move |root| rebase_event_path(root, path).map(|source_path| (root.provider, source_path)))
}

fn is_directory_path(roots: &[WatchRoot], path: &Path) -> bool {
  let watched_path = canonical_callback_path(path);
  path.is_dir()
    || roots
      .iter()
      .any(|root| watched_path == root.watched_root || path == root.source_root)
}

fn is_session_file(path: &Path) -> bool {
  path.extension().is_some_and(|extension| extension == "jsonl")
}

fn is_compressed_codex_history(path: &Path) -> bool {
  path.extension().is_some_and(|extension| extension == "zst") && is_session_file(&path.with_extension(""))
}

#[cfg(test)]
mod tests {
  use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
  };

  #[cfg(target_os = "macos")]
  use super::notify;
  use notify::{
    Event, EventKind, RecursiveMode,
    event::{DataChange, Flag, MetadataKind, ModifyKind, RemoveKind, RenameMode},
  };
  use tempfile::TempDir;

  use super::{
    SessionFileWatcher, WatchRequest, WatchRequestCoalescer, classify_session_file_event, classify_watcher_wake,
  };
  use crate::model::ViewerProvider;

  fn roots() -> Vec<(ViewerProvider, PathBuf)> {
    vec![
      (ViewerProvider::Codex, PathBuf::from("/sessions/codex")),
      (ViewerProvider::Pi, PathBuf::from("/sessions/pi")),
    ]
  }

  #[test]
  fn data_and_metadata_changes_target_only_the_changed_jsonl_file() {
    let path = PathBuf::from("/sessions/codex/2026/session.jsonl");
    for kind in [
      EventKind::Modify(ModifyKind::Data(DataChange::Content)),
      EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
    ] {
      let event = Event::new(kind).add_path(path.clone());
      assert_eq!(
        classify_session_file_event(&roots(), &event),
        Some(WatchRequest::ChangedFiles(vec![(ViewerProvider::Codex, path.clone())]))
      );
    }
  }

  #[test]
  fn compressed_codex_history_changes_reconcile_the_full_catalog() {
    let path = PathBuf::from("/sessions/codex/2026/session.jsonl.zst");
    for kind in [
      EventKind::Create(notify::event::CreateKind::File),
      EventKind::Modify(ModifyKind::Data(DataChange::Content)),
      EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
    ] {
      let event = Event::new(kind).add_path(path.clone());
      assert_eq!(
        classify_session_file_event(&roots(), &event),
        Some(WatchRequest::FullCatalog)
      );
    }
    let pi = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
      .add_path(PathBuf::from("/sessions/pi/session.jsonl.zst"));
    let unrelated = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
      .add_path(PathBuf::from("/sessions/codex/other.zst"));
    assert_eq!(classify_session_file_event(&roots(), &pi), None);
    assert_eq!(classify_session_file_event(&roots(), &unrelated), None);
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn windows_any_modify_change_targets_only_the_changed_jsonl_file() {
    let path = PathBuf::from(r"C:\sessions\codex\session.jsonl");
    let roots = vec![(ViewerProvider::Codex, PathBuf::from(r"C:\sessions\codex"))];
    let event = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path.clone());

    assert_eq!(
      classify_session_file_event(&roots, &event),
      Some(WatchRequest::ChangedFiles(vec![(ViewerProvider::Codex, path)]))
    );
  }

  #[cfg(not(target_os = "windows"))]
  #[test]
  fn non_windows_any_modify_change_remains_a_structural_fallback() {
    let path = PathBuf::from("/sessions/codex/session.jsonl");
    let event = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path);

    assert_eq!(
      classify_session_file_event(&roots(), &event),
      Some(WatchRequest::FullCatalog)
    );
  }

  #[test]
  fn topology_or_reliability_events_escalate_to_a_full_catalog() {
    let path = PathBuf::from("/sessions/pi/session.jsonl");
    let directory = TempDir::new().unwrap();
    let directory_path = directory.path().join("sessions.jsonl");
    std::fs::create_dir(&directory_path).unwrap();
    let roots_with_directory = vec![(ViewerProvider::Pi, directory.path().to_path_buf())];

    let events = vec![
      Event::new(EventKind::Remove(notify::event::RemoveKind::File)).add_path(path.clone()),
      Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both))).add_path(path.clone()),
      Event::new(EventKind::Other).add_path(path.clone()),
      Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(directory_path),
      Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).set_flag(Flag::Rescan),
    ];

    for event in &events[..3] {
      assert_eq!(
        classify_session_file_event(&roots(), event),
        Some(WatchRequest::FullCatalog)
      );
    }
    assert_eq!(
      classify_session_file_event(&roots_with_directory, &events[3]),
      Some(WatchRequest::FullCatalog)
    );
    assert_eq!(
      classify_session_file_event(&roots(), &events[4]),
      Some(WatchRequest::FullCatalog)
    );
  }

  #[test]
  fn created_jsonl_files_reach_the_safe_targeted_identity_check() {
    let path = PathBuf::from("/sessions/codex/active.jsonl");
    let event = Event::new(EventKind::Create(notify::event::CreateKind::File)).add_path(path.clone());
    assert_eq!(
      classify_session_file_event(&roots(), &event),
      Some(WatchRequest::ChangedFiles(vec![(ViewerProvider::Codex, path)]))
    );
  }

  #[test]
  fn unrelated_and_access_events_are_ignored() {
    let unrelated = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
      .add_path(PathBuf::from("/elsewhere/session.jsonl"));
    let access = Event::new(EventKind::Access(notify::event::AccessKind::Read))
      .add_path(PathBuf::from("/sessions/codex/session.jsonl"));

    assert_eq!(classify_session_file_event(&roots(), &unrelated), None);
    assert_eq!(classify_session_file_event(&roots(), &access), None);
  }

  #[test]
  fn watcher_errors_escalate_to_a_full_catalog() {
    assert_eq!(
      classify_watcher_wake(&roots(), Err("backend stopped".to_owned())),
      Some(WatchRequest::FullCatalog)
    );
  }

  #[tokio::test]
  async fn backend_error_marks_the_native_watcher_for_scheduler_downgrade() {
    let (mut watcher, wake_tx) = SessionFileWatcher::with_test_wakes(roots());
    wake_tx.try_send(Err("backend stopped".to_owned())).unwrap();
    assert_eq!(watcher.next_request().await, Some(WatchRequest::FullCatalog));
    assert!(watcher.take_backend_failure());
    assert!(!watcher.take_backend_failure());
  }

  #[test]
  fn coalescer_deduplicates_changed_files_and_full_catalog_wins() {
    let codex_path = PathBuf::from("/sessions/codex/session.jsonl");
    let pi_path = PathBuf::from("/sessions/pi/session.jsonl");
    let mut coalescer = WatchRequestCoalescer::default();
    coalescer.push(Some(WatchRequest::ChangedFiles(vec![
      (ViewerProvider::Codex, codex_path.clone()),
      (ViewerProvider::Codex, codex_path.clone()),
    ])));
    coalescer.push(Some(WatchRequest::ChangedFiles(vec![(ViewerProvider::Pi, pi_path)])));
    assert_eq!(
      coalescer.finish(),
      Some(WatchRequest::ChangedFiles(vec![
        (ViewerProvider::Codex, codex_path),
        (ViewerProvider::Pi, PathBuf::from("/sessions/pi/session.jsonl")),
      ]))
    );

    let mut coalescer = WatchRequestCoalescer::default();
    coalescer.push(Some(WatchRequest::ChangedFiles(vec![(
      ViewerProvider::Codex,
      PathBuf::from("/sessions/codex/session.jsonl"),
    )])));
    coalescer.push(Some(WatchRequest::FullCatalog));
    coalescer.push(Some(WatchRequest::ChangedFiles(vec![(
      ViewerProvider::Pi,
      PathBuf::from("/sessions/pi/session.jsonl"),
    )])));
    assert_eq!(coalescer.finish(), Some(WatchRequest::FullCatalog));
  }

  #[test]
  fn watcher_roots_follow_a_canonical_existing_ancestor() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("missing-sessions");
    let expected = directory.path().canonicalize().unwrap().join("missing-sessions");
    assert_eq!(super::canonical_watcher_root(&root), expected);
  }

  #[test]
  fn existing_roots_watch_both_the_tree_and_its_lifecycle_parent() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("sessions");
    std::fs::create_dir(&root).unwrap();

    let targets = super::watch_targets(&root);
    assert!(targets.contains(&(directory.path().to_path_buf(), RecursiveMode::NonRecursive)));
    assert!(targets.contains(&(root, RecursiveMode::Recursive)));
  }

  #[test]
  fn removing_a_root_retires_its_native_coverage_but_keeps_the_parent_watch() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("sessions");
    std::fs::create_dir(&root).unwrap();
    let (mut watcher, _wake_tx) = SessionFileWatcher::with_test_wakes(vec![(ViewerProvider::Codex, root.clone())]);
    let watched_root = watcher.roots[0].clone();
    let lifecycle_parent = watched_root
      .watched_root
      .parent()
      .expect("fixture root should have a parent")
      .to_path_buf();
    watcher
      .watched_paths
      .insert(lifecycle_parent.clone(), RecursiveMode::NonRecursive);
    watcher
      .watched_paths
      .insert(watched_root.watched_root.clone(), RecursiveMode::Recursive);
    watcher.watched_roots.insert(watched_root.clone());

    std::fs::remove_dir(&root).unwrap();
    watcher.forget_invalidated_watches(&Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(root));

    assert!(watcher.covered_providers().is_empty());
    assert!(!watcher.watched_paths.contains_key(&watched_root.watched_root));
    assert!(watcher.watched_paths.contains_key(&lifecycle_parent));
  }

  #[test]
  fn removing_a_lifecycle_parent_retires_every_dependent_root_registration() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("sessions");
    std::fs::create_dir(&root).unwrap();
    let (mut watcher, _wake_tx) = SessionFileWatcher::with_test_wakes(vec![(ViewerProvider::Codex, root.clone())]);
    let watched_root = watcher.roots[0].clone();
    let lifecycle_parent = watched_root
      .watched_root
      .parent()
      .expect("fixture root should have a parent")
      .to_path_buf();
    watcher
      .watched_paths
      .insert(lifecycle_parent.clone(), RecursiveMode::NonRecursive);
    watcher
      .watched_paths
      .insert(watched_root.watched_root.clone(), RecursiveMode::Recursive);
    watcher.watched_roots.insert(watched_root.clone());

    // Keep the directory present: this proves source-parent equality retires
    // coverage even though the path cannot be rebased as a child of `root`.
    watcher.forget_invalidated_watches(
      &Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(directory.path().to_path_buf()),
    );

    assert!(watcher.covered_providers().is_empty());
    assert!(!watcher.watched_paths.contains_key(&watched_root.watched_root));
    assert!(!watcher.watched_paths.contains_key(&lifecycle_parent));
  }

  #[tokio::test]
  async fn lifecycle_parent_event_requests_an_immediate_full_catalog() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("sessions");
    std::fs::create_dir(&root).unwrap();
    let (mut watcher, wake_tx) = SessionFileWatcher::with_test_wakes(vec![(ViewerProvider::Codex, root)]);

    wake_tx
      .try_send(Ok(
        Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(directory.path().to_path_buf()),
      ))
      .unwrap();

    assert_eq!(watcher.next_request().await, Some(WatchRequest::FullCatalog));
  }

  #[test]
  fn coverage_requires_every_configured_root_for_a_provider() {
    let (mut watcher, _wake_tx) = SessionFileWatcher::with_test_wakes(roots());
    assert!(watcher.covered_providers().is_empty());
    for root in watcher
      .roots
      .iter()
      .filter(|root| root.provider == ViewerProvider::Codex)
      .cloned()
      .collect::<Vec<_>>()
    {
      watcher.watched_roots.insert(root);
    }
    assert_eq!(watcher.covered_providers(), BTreeSet::from([ViewerProvider::Codex]));
  }

  #[test]
  fn canonical_callbacks_rebase_to_the_provider_root_spelling() {
    let root = super::WatchRoot {
      provider: ViewerProvider::Codex,
      source_root: PathBuf::from("/var/example/sessions"),
      watched_root: PathBuf::from("/private/var/example/sessions"),
    };
    let event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
      .add_path(PathBuf::from("/private/var/example/sessions/active.jsonl"));
    assert_eq!(
      super::classify_session_file_event_for_roots(&[root], &event),
      Some(WatchRequest::ChangedFiles(vec![(
        ViewerProvider::Codex,
        PathBuf::from("/var/example/sessions/active.jsonl"),
      )]))
    );
  }

  #[tokio::test]
  async fn cancelling_a_wait_preserves_the_received_callback_for_the_next_wait() {
    let path = PathBuf::from("/sessions/codex/session.jsonl");
    let (mut watcher, wake_tx) = SessionFileWatcher::with_test_wakes(roots());
    wake_tx
      .try_send(Ok(
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content))).add_path(path.clone()),
      ))
      .unwrap();

    tokio::select! {
      biased;
      request = watcher.next_request() => panic!("watch batch returned too early: {request:?}"),
      _ = tokio::task::yield_now() => {}
    }

    assert!(watcher.pending.is_some(), "the cancelled wait should retain its batch");
    watcher
      .pending
      .as_mut()
      .expect("pending batch should exist")
      .quiet_deadline = tokio::time::Instant::now() - Duration::from_millis(1);

    assert_eq!(
      watcher.next_request().await,
      Some(WatchRequest::ChangedFiles(vec![(ViewerProvider::Codex, path)]))
    );
  }

  #[tokio::test]
  async fn callback_buffer_overflow_escalates_to_a_full_catalog() {
    let (mut watcher, _wake_tx) = SessionFileWatcher::with_test_wakes(roots());
    watcher.wake_overflowed.store(true, Ordering::Release);
    assert_eq!(watcher.next_request().await, Some(WatchRequest::FullCatalog));
    assert!(watcher.pending.is_none());
  }

  #[tokio::test]
  async fn overflowed_backend_error_still_marks_the_watcher_for_downgrade() {
    let (mut watcher, wake_tx) = SessionFileWatcher::with_test_wakes(roots());
    watcher.wake_overflowed.store(true, Ordering::Release);
    wake_tx
      .try_send(Err("backend stopped during overflow".to_owned()))
      .unwrap();
    // The raw error is deliberately discarded by the overflow recovery batch;
    // model the native callback's shared failure signal so its loss cannot
    // leave the scheduler trusting this watcher.
    watcher.backend_failed.store(true, Ordering::Release);

    assert_eq!(watcher.next_request().await, Some(WatchRequest::FullCatalog));
    assert!(watcher.take_backend_failure());
  }

  #[tokio::test]
  async fn native_watcher_targets_an_appended_rollout_file() {
    #[cfg(target_os = "macos")]
    assert_eq!(
      <notify::RecommendedWatcher as notify::Watcher>::kind(),
      notify::WatcherKind::Fsevent,
      "the viewer smoke test must exercise FSEvents rather than the relay's Kqueue backend"
    );

    // Exercise the same system temporary volume that a normal macOS process
    // uses. FSEvents canonicalizes this path through `/private`, which also
    // validates the callback rebasing needed for provider root spellings.
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("codex-sessions");
    std::fs::create_dir(&root).unwrap();
    let path = root.join("active.jsonl");
    std::fs::write(&path, "{\"initial\":true}\n").unwrap();
    let mut watcher = SessionFileWatcher::new(vec![(ViewerProvider::Codex, root.clone())]).unwrap();

    // Match the production scheduler's FSEvents startup guard before using a
    // post-registration creation as a stream fence. Notify documents that its
    // macOS run loop comes up asynchronously.
    let startup_guard = watcher.startup_guard();
    if !startup_guard.is_zero() {
      tokio::time::sleep(startup_guard).await;
    }

    // Use a post-registration file creation as an event-stream fence. FSEvents
    // may still deliver a record from the short interval while its run loop is
    // coming up, so an immediate assertion about `active.jsonl` could pass on
    // its earlier create rather than the append below. Once this fence has
    // arrived, earlier callbacks have been consumed; discard any trailing
    // metadata callbacks for the fence before writing the existing rollout.
    let fence = root.join("watcher-ready.jsonl");
    std::fs::write(&fence, "{\"ready\":true}\n").unwrap();
    next_native_request_for_path(&mut watcher, &fence).await;
    discard_queued_native_wakes(&mut watcher).await;

    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{{\"next\":true}}").unwrap();
    file.flush().unwrap();
    drop(file);

    let request = tokio::time::timeout(Duration::from_secs(2), watcher.next_request())
      .await
      .expect("native watcher should observe a rollout append");
    assert_eq!(
      request,
      Some(WatchRequest::ChangedFiles(vec![(ViewerProvider::Codex, path)]))
    );
  }

  async fn next_native_request_for_path(watcher: &mut SessionFileWatcher, path: &Path) -> WatchRequest {
    tokio::time::timeout(Duration::from_secs(3), async {
      loop {
        let request = watcher
          .next_request()
          .await
          .expect("native watcher callback channel should remain open");
        if changed_file_request_contains(&request, path) {
          return request;
        }
      }
    })
    .await
    .unwrap_or_else(|_| panic!("native watcher should observe {}", path.display()))
  }

  fn changed_file_request_contains(request: &WatchRequest, path: &Path) -> bool {
    matches!(
      request,
      WatchRequest::ChangedFiles(paths)
        if paths.contains(&(ViewerProvider::Codex, path.to_path_buf()))
    )
  }

  async fn discard_queued_native_wakes(watcher: &mut SessionFileWatcher) {
    assert!(
      watcher.pending.is_none(),
      "the fence request should have completed its batch"
    );
    let oldest_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut quiet_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    loop {
      tokio::select! {
        // Waiting on the raw callback channel is cancellation-safe: unlike a
        // cancelled `next_request`, it cannot leave a partially coalesced
        // request behind for the append assertion. Bound the test's drain so
        // an unrelated filesystem storm cannot starve it forever.
        _ = tokio::time::sleep_until(oldest_deadline) => return,
        _ = tokio::time::sleep_until(quiet_deadline) => return,
        wake = watcher.wake_rx.recv() => match wake {
          Some(_) => quiet_deadline = tokio::time::Instant::now() + Duration::from_millis(300),
          None => panic!("native watcher callback channel should remain open"),
        },
      }
    }
  }
}
