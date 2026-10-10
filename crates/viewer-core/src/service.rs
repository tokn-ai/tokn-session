use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokn_session_client::SessionHeader;
use tokn_session_core::{
  AgentActivity, AgentEvent, LifecycleOutcome, LoadedSession, MessageDelivery, Phase, Provider, Role, TerminalAction,
  ToolCallEvent, ToolKind, ToolOperation, ToolOperationStatus, ToolSummary, UsageKind, assemble_tool_operations,
};
use tokn_session_index::{
  IndexedSession, SessionBaselineCompletionRequest, SessionIndex, SessionIndexError, SessionKey as IndexedSessionKey,
  SessionMetadata, SourceCursorPrecondition, SourceKey, SourceReplacement, SourceState,
  StagedSessionBaselineSourceCount,
};
use tokn_session_render::render_event_summary;

use crate::model::{
  AcknowledgeSessionAttentionRequest, AcknowledgeSessionAttentionResponse, AgentActivityCardSummary,
  CatalogRefreshScope, EventDetail, EventPage, EventPageRequest, EventSummary, IndexActivity, IndexWorkerError,
  ListSessionChildrenRequest, ListSessionChildrenResponse, ListSessionsRequest, ListSessionsResponse,
  LoadEventDetailRequest, LoadTrajectoryEventPageRequest, PageDirection, ProviderBody, ReasoningCardSummary,
  SessionIndexProgress, SessionLocator, SessionOrder, SessionSummary, SourceError, ToolCardSummary, ToolOutputPreview,
  ToolOutputSection, TrajectoryCardSummary, TrajectoryEventPage, UsageCardSummary, ViewerProvider, bounded_limit,
  decode_event_cursor, decode_event_key, decode_list_cursor, decode_session_key, decode_trajectory_event_cursor,
  decode_trajectory_key, encode_event_cursor, encode_event_key, encode_list_cursor, encode_session_key,
  encode_trajectory_event_cursor, encode_trajectory_key, parse_updated_at_ms, requested_offset,
};
use crate::repository::activity::Activity;
use crate::repository::{NativeRepository, SessionBodyIndexing, ViewerRepository};

mod agent_activity;
mod compaction;
mod event_filter;
mod input;
mod trajectory_state;
mod usage_filter;
mod windows;

use agent_activity::{ActivityTargets, agent_activity_card_summary};

const MAX_DETAIL_VALUE_BYTES: usize = 512 * 1024;
const MAX_MESSAGE_SUMMARY_CHARS: usize = 16 * 1024;
const MAX_SESSION_PREVIEW_CHARS: usize = 240;
const MAX_SESSION_TITLE_CHARS: usize = 160;
const MAX_AGENT_IDENTITY_CHARS: usize = 160;
const MAX_REASONING_CARD_PREVIEW_CHARS: usize = 240;
const MAX_TECHNICAL_SUMMARY_CHARS: usize = 500;
const MAX_TOOL_CARD_STRING_CHARS: usize = 500;
const MAX_TOOL_NAME_CHARS: usize = 120;
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TRAJECTORY_TIMESTAMP_CHARS: usize = 128;
const MAX_TRAJECTORY_DETAIL_SOURCE_RECORDS: usize = 100;
const MAX_TRAJECTORY_DETAIL_SOURCE_RECORD_BYTES: usize = 64 * 1024;
const MAX_INDEXED_CWD_CHARS: usize = 4 * 1024;
const MAX_INDEXED_TIMESTAMP_CHARS: usize = 256;
const INDEX_CATALOG_SOURCE_KEY: &str = "catalog.v1";
const INDEX_MISSING_SOURCE_CURSOR: &str = "missing.v1";
const INDEX_PENDING_BODY_CURSOR_PREFIX: &str = "catalog.v3.";
const INDEX_COMPLETED_BODY_CURSOR_PREFIX: &str = "body.v3.";
const LEGACY_PENDING_BODY_CURSOR_PREFIX: &str = "catalog.v2.";
const LEGACY_COMPLETED_BODY_CURSOR_PREFIX: &str = "body.v2.";
const INDEX_BODY_SCAN_BATCH_SIZE: usize = 8;
const TOOL_OUTPUT_TRUNCATION_MARKER: &str = "\n\u{2026} output truncated \u{2026}\n";

#[derive(Clone)]
pub struct ViewerService {
  pub relay: Arc<crate::relay::ViewerRelay>,
  pub(crate) indexer_lock: Option<crate::indexer::IndexerLock>,
  repository: Arc<dyn ViewerRepository>,
  session_index: Arc<SessionIndex>,
  index_refresh_gate: Arc<Mutex<()>>,
  index_progress: IndexProgressStore,
  index_retry_sender: Arc<Mutex<Option<mpsc::UnboundedSender<SessionIndexWake>>>>,
  /// Header-discovery failures remain visible until a later full catalog
  /// succeeds, even if a direct body retry happens to work in the meantime.
  catalog_errors: Arc<Mutex<HashMap<ViewerProvider, String>>>,
  index_errors: Arc<Mutex<HashMap<ViewerProvider, String>>>,
  /// SQLite `data_version` most recently observed through this service's own
  /// index connection. It lets one app process notice a clean commit made by
  /// another process even when this process's next catalog scan is a no-op.
  observed_index_data_version: Arc<Mutex<Option<i64>>>,
  failed_body_jobs: Arc<Mutex<HashMap<(SourceKey, String), FailedBodyJob>>>,
  loaded_session_cache: Arc<Mutex<Option<CachedSession>>>,
  input_broker: crate::input::InputBroker,
  pub(crate) updates: Arc<Mutex<crate::updates::UpdateStore>>,
  /// Per-request metadata projection. The adapter still owns grant verification,
  /// command permissions, and stream lifetime; never install this on a worker.
  session_scope: Option<Arc<HashSet<String>>>,
}

/// The progress center must never need to touch provider storage. This store
/// owns one compact snapshot and publishes replacements to every window. A
/// separate mutex gives synchronous service code a cheap, race-free update
/// path while Tokio's watch channel handles asynchronous fan-out.
#[derive(Clone)]
struct IndexProgressStore {
  state: Arc<Mutex<IndexProgressState>>,
  sender: watch::Sender<SessionIndexProgress>,
}

struct IndexProgressState {
  next_revision: u64,
  /// Changes whenever a user explicitly queues a retry. It is
  /// intentionally internal: the scheduler uses it to distinguish its own
  /// stale post-refresh transition from a newer user request without adding
  /// another Tauri payload field.
  manual_retry_generation: u64,
  /// The manual-retry generation observed when the most recent synchronous
  /// refresh began. A terminal state is valid only while this still matches
  /// `manual_retry_generation`.
  latest_refresh_retry_generation: u64,
  snapshot: SessionIndexProgress,
}

struct ManualRetryRequest {
  generation: u64,
  previous_generation: u64,
  snapshot: SessionIndexProgress,
  previous_snapshot: Option<SessionIndexProgress>,
  published_revision: Option<String>,
}

enum IndexProgressSettlement {
  Idle,
  WaitingToRetry {
    retry_at_ms: Option<i64>,
    worker_error: Option<IndexWorkerError>,
  },
}

fn set_index_progress_waiting_to_retry(
  progress: &mut SessionIndexProgress,
  retry_at_ms: Option<i64>,
  worker_error: Option<IndexWorkerError>,
) {
  progress.is_refreshing = false;
  progress.activity = IndexActivity::WaitingToRetry;
  progress.catalog.active_provider = None;
  progress.body.active_provider = None;
  progress.worker_error = worker_error;
  progress.retry_at_ms = retry_at_ms;
}

fn set_index_progress_idle(progress: &mut SessionIndexProgress) {
  progress.is_refreshing = false;
  progress.activity = IndexActivity::Idle;
  progress.catalog.active_provider = None;
  progress.body.active_provider = None;
  progress.worker_error = None;
  progress.retry_at_ms = None;
}

impl IndexProgressSettlement {
  fn apply(self, progress: &mut SessionIndexProgress) {
    match self {
      Self::Idle => set_index_progress_idle(progress),
      Self::WaitingToRetry {
        retry_at_ms,
        worker_error,
      } => set_index_progress_waiting_to_retry(progress, retry_at_ms, worker_error),
    }
  }
}

impl IndexProgressStore {
  fn new(body_batch_size: usize) -> Self {
    let snapshot = SessionIndexProgress::initial(body_batch_size);
    let (sender, _receiver) = watch::channel(snapshot.clone());
    Self {
      state: Arc::new(Mutex::new(IndexProgressState {
        next_revision: 0,
        manual_retry_generation: 0,
        latest_refresh_retry_generation: 0,
        snapshot,
      })),
      sender,
    }
  }

  fn snapshot(&self) -> SessionIndexProgress {
    self
      .state
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .snapshot
      .clone()
  }

  fn subscribe(&self) -> watch::Receiver<SessionIndexProgress> {
    self.sender.subscribe()
  }

  fn update_state(&self, update: impl FnOnce(&mut IndexProgressState)) -> SessionIndexProgress {
    let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = state.snapshot.clone();
    update(&mut state);
    if state.snapshot == previous {
      return previous;
    }
    state.next_revision = state.next_revision.saturating_add(1);
    state.snapshot.revision = state.next_revision.to_string();
    let snapshot = state.snapshot.clone();
    drop(state);
    // `send_replace` retains the latest state even before the first viewer
    // window subscribes, and it is intentionally harmless with no listeners.
    self.sender.send_replace(snapshot.clone());
    snapshot
  }

  fn update(&self, update: impl FnOnce(&mut SessionIndexProgress)) -> SessionIndexProgress {
    self.update_state(|state| update(&mut state.snapshot))
  }

  /// Begins one synchronous worker pass and captures the manual-retry state
  /// against which its later terminal transition must be checked.
  fn begin_refresh(&self, update: impl FnOnce(&mut SessionIndexProgress)) -> SessionIndexProgress {
    self.update_state(|state| {
      state.latest_refresh_retry_generation = state.manual_retry_generation;
      update(&mut state.snapshot);
    })
  }

  /// Applies a terminal transition only when no manual retry was accepted
  /// after the current refresh started. Both the synchronous refresh and the
  /// async scheduler use this gate, closing the handoff window between them.
  fn settle_after_latest_refresh(&self, settlement: IndexProgressSettlement) -> SessionIndexProgress {
    self.update_state(|state| {
      if state.manual_retry_generation != state.latest_refresh_retry_generation {
        set_index_progress_waiting_to_retry(&mut state.snapshot, None, None);
      } else {
        settlement.apply(&mut state.snapshot);
      }
    })
  }

  /// Records a manually requested retry. If a worker is currently active, the
  /// visible active state remains truthful while the internal generation still
  /// prevents that worker's stale terminal transition from hiding the request.
  fn request_manual_retry(&self) -> ManualRetryRequest {
    let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous_generation = state.manual_retry_generation;
    state.manual_retry_generation = state.manual_retry_generation.wrapping_add(1);
    let generation = state.manual_retry_generation;
    let previous = state.snapshot.clone();
    if previous.is_refreshing {
      return ManualRetryRequest {
        generation,
        previous_generation,
        snapshot: previous,
        previous_snapshot: None,
        published_revision: None,
      };
    }
    set_index_progress_waiting_to_retry(&mut state.snapshot, None, None);
    if state.snapshot == previous {
      return ManualRetryRequest {
        generation,
        previous_generation,
        snapshot: previous,
        previous_snapshot: None,
        published_revision: None,
      };
    }
    state.next_revision = state.next_revision.saturating_add(1);
    state.snapshot.revision = state.next_revision.to_string();
    let snapshot = state.snapshot.clone();
    drop(state);
    self.sender.send_replace(snapshot.clone());
    ManualRetryRequest {
      generation,
      previous_generation,
      snapshot: snapshot.clone(),
      previous_snapshot: Some(previous),
      published_revision: Some(snapshot.revision),
    }
  }

  /// Rolls back a retry that could not reach the scheduler. It never erases a
  /// newer manual request or a scheduler state transition.
  fn cancel_manual_retry_if_current(&self, request: ManualRetryRequest) {
    let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.manual_retry_generation != request.generation {
      return;
    }
    state.manual_retry_generation = request.previous_generation;
    let Some(previous) = request.previous_snapshot else {
      return;
    };
    let Some(published_revision) = request.published_revision else {
      return;
    };
    if state.snapshot.revision != published_revision {
      return;
    }
    state.snapshot = previous;
    state.next_revision = state.next_revision.saturating_add(1);
    state.snapshot.revision = state.next_revision.to_string();
    let snapshot = state.snapshot.clone();
    drop(state);
    self.sender.send_replace(snapshot);
  }
}

struct CachedSession {
  locator: SessionLocator,
  source_revision: SourceRevision,
  loaded: Arc<LoadedSession>,
  retained_start: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceRevision {
  paths: Vec<PathBuf>,
  files: Vec<Option<FileRevision>>,
}

impl SourceRevision {
  /// A loaded snapshot already resolved its physical dependencies. Revalidate
  /// those files without rediscovering and parsing the same history lineage
  /// for every page or detail request.
  fn is_current(&self) -> bool {
    self
      .paths
      .iter()
      .zip(&self.files)
      .all(|(path, previous)| file_revision(path) == *previous)
  }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileRevision {
  len: u64,
  modified: Option<SystemTime>,
  created: Option<SystemTime>,
}

#[derive(Clone)]
struct SessionListCandidate {
  provider: ViewerProvider,
  header: SessionHeader,
  child_count: usize,
  is_subagent: bool,
  attention: SessionAttention,
}

#[derive(Clone, Copy, Debug, Default)]
struct SessionAttention {
  has_unread: bool,
  unread_final_count: u64,
  is_running: bool,
  has_running_descendant: bool,
  question_attention: crate::model::QuestionAttention,
}

#[derive(Default)]
struct SessionHeaderInventory {
  headers: Vec<SessionHeader>,
  direct_attention: HashMap<SessionLocator, SessionAttention>,
}

/// One SQLite-only sidebar snapshot. A provider becomes visible only after
/// its catalog sentinel commits, so the UI never observes a partial first
/// catalog or falls back to provider storage while it is pending.
struct IndexedSessionInventories {
  by_provider: HashMap<ViewerProvider, SessionHeaderInventory>,
  pending_providers: Vec<ViewerProvider>,
  source_errors: Vec<SourceError>,
}

/// Stable source/session membership observed during one header catalog pass.
/// Presentation fields and source revision are intentionally absent: Codex can
/// update those while a large discovery pass is still running.
type SessionCatalogTopology = BTreeMap<String, BTreeSet<String>>;

/// Result of one background reconciliation pass. It deliberately carries no
/// session contents: the Tauri shell receives only a sidebar-refresh signal
/// and opaque keys for sessions with newly eligible attention.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct IndexRefresh {
  /// Whether the sidebar should reread compact index state, including source
  /// warnings that changed without a metadata row changing.
  pub changed: bool,
  /// False only when compact notifications cover every changed sidebar field.
  pub catalog_refresh_required: bool,
  /// Opaque session keys whose newly indexed eligible messages should refresh
  /// an already-visible newest event page. This deliberately excludes ordinary
  /// metadata updates so an unrelated source scan cannot reset the timeline a
  /// user is reading.
  pub attention_session_keys: Vec<String>,
  /// Successfully refreshed bodies, including progress/tool/lifecycle changes
  /// that do not produce unread attention. Never inferred from catalog mtime.
  pub updated_session_keys: Vec<String>,
  /// Internal scheduler hint. It deliberately stays out of Tauri events: the
  /// frontend only needs the existing compact refresh payload.
  #[serde(skip)]
  pub has_pending_body_jobs: bool,
  /// Internal scheduler hint for a catalog that changed structurally while it
  /// was being observed. This is not a provider-read error; the previous
  /// committed catalog remains safe to show.
  #[serde(skip)]
  pub retry_catalog_soon: bool,
  /// Internal scheduler hint for a provider catalog failure. It stays out of
  /// sidebar events because readable provider failures already travel through
  /// the durable index listing, but it prevents the scheduler from replacing
  /// the progress store's truthful waiting state with idle.
  #[serde(skip)]
  pub has_catalog_errors: bool,
  /// Whether the catalog providers attempted by this exact pass failed. This
  /// is narrower than [`Self::has_catalog_errors`], which includes retained
  /// warnings from providers not scanned by a provider-local refresh.
  #[serde(skip)]
  pub catalog_attempt_has_errors: bool,
  /// A known file whose revision changed while a direct header read was in
  /// flight. Retrying this bounded path is cheaper and more accurate than
  /// escalating an ordinary active-session append into a complete catalog.
  #[serde(skip)]
  pub retry_changed_file_paths: BTreeMap<ViewerProvider, BTreeSet<PathBuf>>,
}

/// Work requested from the single session-index scheduler.
///
/// A full catalog remains the correctness path for initial discovery and
/// filesystem topology changes. Ordinary writes to known file-backed sources
/// can instead update just those source rows, avoiding a provider-wide header
/// scan for every active rollout append.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionIndexWake {
  ProviderCatalog(BTreeSet<ViewerProvider>),
  FullCatalog,
  ChangedFiles(BTreeMap<ViewerProvider, BTreeSet<PathBuf>>),
}

#[derive(Default)]
struct ProviderIndexRefresh {
  changed: bool,
  presentation_changed: bool,
  attention_session_keys: Vec<String>,
  retry_catalog_soon: bool,
  retry_changed_file_paths: BTreeSet<PathBuf>,
}

/// The durable state needed to turn a bounded provider header read into safe
/// source replacements. Both complete provider catalogs and targeted file
/// refreshes use this snapshot so their cursor and notification behavior stay
/// identical.
struct CatalogIndexSnapshot {
  provider_ready: bool,
  existing_sources: Vec<SourceState>,
  existing_sessions: Vec<IndexedSession>,
}

/// A source whose metadata catalog has committed but whose message body still
/// needs to be inspected. Its staged cursor prevents two viewer processes from
/// independently turning the same new message into two unread revisions.
#[derive(Clone)]
struct PendingBodyJob {
  provider: ViewerProvider,
  source: SourceState,
  raw_cursor: String,
  locator: SessionLocator,
  priority: i64,
  deprioritized: bool,
}

#[derive(Clone)]
struct FailedBodyJob {
  raw_cursor: String,
  source_generation: i64,
  message: String,
}

struct BodyBackfillRefresh {
  refresh: IndexRefresh,
  errors: HashMap<ViewerProvider, String>,
}

fn ordered_providers(providers: &[ViewerProvider]) -> Vec<ViewerProvider> {
  ViewerProvider::ALL
    .into_iter()
    .filter(|provider| providers.contains(provider))
    .collect()
}

fn ordered_error_providers(errors: &HashMap<ViewerProvider, String>) -> Vec<ViewerProvider> {
  ViewerProvider::ALL
    .into_iter()
    .filter(|provider| errors.contains_key(provider))
    .collect()
}

fn body_queue_progress(jobs: &[PendingBodyJob]) -> (usize, usize, Vec<ProviderBody>) {
  let mut providers = ViewerProvider::ALL
    .into_iter()
    .map(|provider| ProviderBody {
      provider,
      total_jobs: 0,
      completed_jobs: 0,
      pending_jobs: 0,
      failed_jobs: 0,
    })
    .collect::<Vec<_>>();
  for job in jobs {
    let current = providers
      .iter_mut()
      .find(|current| current.provider == job.provider)
      .expect("every pending body job should use a known viewer provider");
    current.pending_jobs = current.pending_jobs.saturating_add(1);
    if job.deprioritized {
      current.failed_jobs = current.failed_jobs.saturating_add(1);
    }
  }
  let pending_jobs = providers.iter().map(|provider| provider.pending_jobs).sum();
  let failed_jobs = providers.iter().map(|provider| provider.failed_jobs).sum();
  (pending_jobs, failed_jobs, providers)
}

/// Builds a durable per-provider body-progress summary from staged source
/// aggregates. The v3 cursor generation keeps the fraction stable across
/// bounded batches and process restarts without loading or sorting historical
/// session metadata.
fn indexed_body_queue_progress(counts: &[StagedSessionBaselineSourceCount]) -> (usize, Vec<ProviderBody>) {
  let mut providers = ViewerProvider::ALL
    .into_iter()
    .map(|provider| ProviderBody {
      provider,
      total_jobs: 0,
      completed_jobs: 0,
      pending_jobs: 0,
      failed_jobs: 0,
    })
    .collect::<Vec<_>>();
  for count in counts {
    let Some(provider) = providers
      .iter_mut()
      .find(|provider| provider.provider.as_str() == count.provider)
    else {
      // The shared index can retain rows from a provider this viewer no
      // longer supports. Its scheduler would ignore the row too.
      continue;
    };
    let completed_jobs = staged_body_cursor_parts(&count.source_cursor, INDEX_PENDING_BODY_CURSOR_PREFIX)
      .and_then(|(generation, _)| usize::try_from(generation).ok())
      // Legacy staged cursors have no durable completion generation. They are
      // only retained for an upgrade path, so use their durable body state as
      // the conservative fallback.
      .unwrap_or_else(|| count.present_sessions.saturating_sub(count.pending_sessions));
    provider.completed_jobs = provider.completed_jobs.saturating_add(completed_jobs);
    provider.pending_jobs = provider.pending_jobs.saturating_add(count.pending_sessions);
    provider.total_jobs = provider.completed_jobs.saturating_add(provider.pending_jobs);
  }
  let pending_jobs = providers.iter().map(|provider| provider.pending_jobs).sum();
  (pending_jobs, providers)
}

fn provider_body_mut(providers: &mut [ProviderBody], provider: ViewerProvider) -> &mut ProviderBody {
  providers
    .iter_mut()
    .find(|current| current.provider == provider)
    .expect("every body progress snapshot should include every viewer provider")
}

fn update_body_progress_totals(body: &mut crate::model::BodyIndexProgress) {
  body.pending_jobs = body.providers.iter().map(|provider| provider.pending_jobs).sum();
  body.failed_jobs = body.providers.iter().map(|provider| provider.failed_jobs).sum();
}

fn record_body_progress_handled(body: &mut crate::model::BodyIndexProgress, job: &PendingBodyJob) {
  let current = provider_body_mut(&mut body.providers, job.provider);
  current.completed_jobs = current.completed_jobs.saturating_add(1);
  current.pending_jobs = current.pending_jobs.saturating_sub(1);
  if job.deprioritized {
    current.failed_jobs = current.failed_jobs.saturating_sub(1);
  }
  // A valid durable snapshot has `total = completed + pending`. Keep that
  // denominator stable while a bounded batch reports each completed job, but
  // remain defensive if another process made the local snapshot stale.
  current.total_jobs = current
    .total_jobs
    .max(current.completed_jobs.saturating_add(current.pending_jobs));
  update_body_progress_totals(body);
}

/// Result of a complete provider-header pass before any session body is read.
/// The scheduler emits this immediately so a newly committed catalog becomes
/// visible even when later body work is slow or malformed.
struct CatalogRefresh {
  refresh: IndexRefresh,
  errors: HashMap<ViewerProvider, String>,
  #[cfg_attr(not(test), allow(dead_code))]
  unavailable: HashSet<ViewerProvider>,
}

enum BodyJobRefresh {
  /// The source is still changing. Leave it queued until it has been quiet.
  Deferred,
  /// The job no longer matches the current catalog or provider source. A
  /// subsequent catalog pass will create the next safe job.
  Stale,
  /// The source body committed successfully.
  Updated {
    provider_refresh: ProviderIndexRefresh,
    next_source: SourceState,
  },
}

struct SessionRelationIndex {
  headers: Vec<SessionHeader>,
  parent_indices: Vec<Option<usize>>,
  child_counts: Vec<usize>,
}

/// One visible historical timeline row. Tool operations intentionally retain
/// their source event index as the stable detail key while hiding intermediate
/// invocation/progress/result fragments from the presentation timeline.
#[derive(Clone, Debug)]
enum TimelineEntry {
  Event {
    source_event_index: usize,
  },
  ToolOperation {
    source_event_index: usize,
    operation: ToolOperation,
  },
  /// A synthetic high-level item over a maximal run of visible work entries,
  /// including intermediate assistant messages. Its source children remain
  /// addressable through their own ordinary `event.v1.*` keys.
  Trajectory {
    trajectory: Trajectory,
  },
}

#[derive(Clone, Debug)]
struct Trajectory {
  /// The first base-timeline source position, used only in the opaque
  /// `trajectory.v1.*` key. Unlike the run's final position, this remains
  /// stable while an active trajectory gains appended events.
  start_source_event_index: usize,
  entries: Vec<TimelineEntry>,
}

impl ViewerService {
  pub fn native(index_path: impl AsRef<Path>) -> Result<Self, String> {
    let session_index = SessionIndex::open(index_path.as_ref())
      .map_err(|error| format!("failed to open the viewer session index: {error}"))?;
    let mut service = Self::with_repository(Arc::new(NativeRepository::default()), Arc::new(session_index));
    service.indexer_lock = Some(crate::indexer::IndexerLock::new(index_path.as_ref())?);
    Ok(service)
  }

  #[cfg(test)]
  fn new(repository: Arc<dyn ViewerRepository>) -> Self {
    let session_index = SessionIndex::open_in_memory().expect("test session index should open");
    Self::with_repository(repository, Arc::new(session_index))
  }

  #[cfg(test)]
  fn new_with_index(repository: Arc<dyn ViewerRepository>, session_index: Arc<SessionIndex>) -> Self {
    Self::with_repository(repository, session_index)
  }

  fn with_repository(repository: Arc<dyn ViewerRepository>, session_index: Arc<SessionIndex>) -> Self {
    // Capture the shared-index revision before the first IPC list request.
    // If another viewer commits after that list but before our first catalog
    // pass, the pass will detect the revision change and ask this UI to read
    // the durable catalog again.
    let observed_index_data_version = session_index.data_version().ok();
    let service = Self {
      relay: crate::relay::ViewerRelay::with_index(Some(session_index.clone())),
      indexer_lock: None,
      repository,
      session_index,
      index_refresh_gate: Arc::new(Mutex::new(())),
      index_progress: IndexProgressStore::new(INDEX_BODY_SCAN_BATCH_SIZE),
      index_retry_sender: Arc::new(Mutex::new(None)),
      catalog_errors: Arc::new(Mutex::new(HashMap::new())),
      index_errors: Arc::new(Mutex::new(HashMap::new())),
      observed_index_data_version: Arc::new(Mutex::new(observed_index_data_version)),
      failed_body_jobs: Arc::new(Mutex::new(HashMap::new())),
      loaded_session_cache: Arc::new(Mutex::new(None)),
      updates: Arc::new(Mutex::new(crate::updates::UpdateStore::default())),
      input_broker: crate::input::InputBroker::default(),
      session_scope: None,
    };
    // Existing durable rows should be reflected immediately when a viewer is
    // reopened. This is SQLite-only bookkeeping; the progress snapshot itself
    // remains a pure in-memory clone.
    service.refresh_index_progress_from_index();
    service
  }

  /// Project catalogs and navigation onto an explicit set of complete sessions.
  /// Filtering precedes tree construction, search, attention, and pagination.
  pub fn scoped_to_sessions(&self, session_keys: &[String]) -> Result<Self, String> {
    if session_keys.is_empty() || session_keys.len() > 128 {
      return Err("A session share requires 1–128 explicit session keys".into());
    }
    for key in session_keys {
      if key.len() > 8192 {
        return Err("Shared session key is too long".into());
      }
      decode_session_key(key)?;
      self.check_session_scope(key)?;
    }
    let mut service = self.clone();
    service.session_scope = Some(Arc::new(session_keys.iter().cloned().collect()));
    Ok(service)
  }

  fn check_session_scope(&self, key: &str) -> Result<(), String> {
    if self.session_scope.as_ref().is_some_and(|scope| !scope.contains(key)) {
      return Err("Session is not included in this share".into());
    }
    Ok(())
  }

  fn page_follow_error(&self, locator: &SessionLocator) -> Option<String> {
    self.relay.follow_error(locator).map(|error| {
      if self.session_scope.is_some() {
        // Reader diagnostics can include local paths outside the share.
        "Live updates are temporarily unavailable; retrying.".into()
      } else {
        error
      }
    })
  }

  fn scope_inventory(&self, provider: ViewerProvider, mut inventory: SessionHeaderInventory) -> SessionHeaderInventory {
    if let Some(scope) = &self.session_scope {
      inventory.headers.retain(|header| {
        encode_session_key(&locator_for_header(provider, header)).is_ok_and(|key| scope.contains(&key))
      });
      let ids: HashSet<_> = inventory.headers.iter().map(|header| header.id.clone()).collect();
      for header in &mut inventory.headers {
        if header.parent_session_id.as_ref().is_some_and(|id| !ids.contains(id)) {
          header.parent_session_id = None;
        }
      }
      // Owner read state is neither inherited by guests nor affected by them.
      inventory.direct_attention.clear();
    }
    inventory
  }

  /// Returns the latest in-memory index worker state without touching SQLite
  /// or provider storage. The Tauri command and event bridge both use this.
  pub fn session_index_progress(&self) -> SessionIndexProgress {
    self.index_progress.snapshot()
  }

  /// Subscribes a window-level bridge to compact progress replacements.
  pub fn subscribe_session_index_progress(&self) -> watch::Receiver<SessionIndexProgress> {
    self.index_progress.subscribe()
  }

  /// Resolves the effective file roots that can safely use watcher-driven
  /// targeted cataloging. Unsupported or temporarily unavailable providers
  /// remain on the scheduler's provider-local catalog cadence.
  pub(crate) fn watched_file_catalog_roots(&self) -> Vec<(ViewerProvider, PathBuf)> {
    let mut roots = Vec::new();
    for provider in [ViewerProvider::Codex, ViewerProvider::Pi] {
      match self.repository.file_session_roots(provider) {
        Ok(provider_roots) => roots.extend(provider_roots.into_iter().map(|path| (provider, path))),
        Err(error) => eprintln!("viewer session watcher could not resolve {provider:?} roots: {error}"),
      }
    }
    roots
  }

  /// Supplies the scheduler wake channel created during Tauri setup. The
  /// service never owns the scheduler task and therefore cannot refresh a
  /// provider directly from a UI command.
  pub(crate) fn set_session_index_retry_sender(&self, sender: mpsc::UnboundedSender<SessionIndexWake>) {
    let mut current = self
      .index_retry_sender
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    *current = Some(sender);
  }

  /// Enqueues an immediate scheduler wake for a user-requested retry. The
  /// resulting snapshot intentionally says `waiting_to_retry`: acceptance by
  /// the queue is not evidence that a provider scan has begun.
  pub fn request_session_index_retry(&self) -> Result<SessionIndexProgress, String> {
    let sender = self
      .index_retry_sender
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
      .ok_or_else(|| "session index scheduler is not available".to_string())?;
    // An explicit retry makes unchanged failed bodies eligible again. Automatic
    // passes retain those failures until the source changes.
    if let Ok(mut failures) = self.failed_body_jobs.lock() {
      failures.clear();
    }
    // Publish before waking an idle scheduler. If work is already active, do
    // not overwrite its truthful catalog/body state; the internal generation
    // still ensures the queued wake cannot be erased by that pass finishing.
    if self.relay.status().settings.mode != crate::relay::RelayMode::External
      && let Some(lock) = &self.indexer_lock
    {
      lock.request_retry()?;
    }
    let request = self.index_progress.request_manual_retry();
    if sender.send(SessionIndexWake::FullCatalog).is_ok() {
      return Ok(request.snapshot);
    }
    self.index_progress.cancel_manual_retry_if_current(request);
    Err("session index scheduler is no longer available".to_string())
  }

  /// Applies the scheduler's exact delayed retry only if a manual retry was
  /// not accepted after the just-finished worker pass began. A manual wake
  /// remains an immediate, visible `waiting_to_retry` state instead.
  pub(crate) fn settle_session_index_waiting_to_retry_after_refresh(
    &self,
    retry_at_ms: Option<i64>,
  ) -> SessionIndexProgress {
    self
      .index_progress
      .settle_after_latest_refresh(IndexProgressSettlement::WaitingToRetry {
        retry_at_ms,
        worker_error: None,
      })
  }

  /// Applies the scheduler's idle state only if it is still newer than every
  /// accepted manual retry.
  pub(crate) fn settle_session_index_idle_after_refresh(&self) -> SessionIndexProgress {
    self
      .index_progress
      .settle_after_latest_refresh(IndexProgressSettlement::Idle)
  }

  /// Applies a scheduler-level error only if a user did not explicitly queue
  /// another attempt while that worker pass was completing.
  pub(crate) fn settle_session_index_worker_error_after_refresh(
    &self,
    worker_error: IndexWorkerError,
    retry_at_ms: Option<i64>,
  ) -> SessionIndexProgress {
    self
      .index_progress
      .settle_after_latest_refresh(IndexProgressSettlement::WaitingToRetry {
        retry_at_ms,
        worker_error: Some(worker_error),
      })
  }

  fn begin_session_index_catalog_refresh(&self, scope: CatalogRefreshScope, total_providers: usize) {
    let catalog_errors = self
      .catalog_errors
      .lock()
      .map(|errors| ordered_error_providers(&errors))
      .unwrap_or_default();
    self.index_progress.begin_refresh(|progress| {
      progress.is_refreshing = true;
      progress.activity = IndexActivity::Catalog;
      progress.worker_error = None;
      progress.retry_at_ms = None;
      progress.catalog.active_provider = None;
      progress.catalog.processed_providers = 0;
      progress.catalog.total_providers = total_providers;
      progress.catalog.scope = scope;
      progress.catalog.error_providers = catalog_errors;
      progress.body.active_provider = None;
      progress.body.completed_in_run = 0;
      progress.body.stale_in_run = 0;
    });
  }

  fn begin_session_index_body_refresh(&self) {
    self.index_progress.begin_refresh(|progress| {
      progress.is_refreshing = true;
      progress.activity = IndexActivity::Body;
      progress.worker_error = None;
      progress.retry_at_ms = None;
      progress.catalog.active_provider = None;
      progress.body.active_provider = None;
      progress.body.completed_in_run = 0;
      progress.body.stale_in_run = 0;
      progress.body.batch_size = INDEX_BODY_SCAN_BATCH_SIZE;
    });
  }

  /// Moves a combined catalog-and-body refresh to its body phase without
  /// replacing the retry generation captured at the start of that top-level
  /// refresh. A manual retry during catalog still belongs to that same pass.
  #[cfg(test)]
  fn continue_session_index_body_refresh(&self) {
    self.index_progress.update(|progress| {
      progress.is_refreshing = true;
      progress.activity = IndexActivity::Body;
      progress.worker_error = None;
      progress.retry_at_ms = None;
      progress.catalog.active_provider = None;
      progress.body.active_provider = None;
      progress.body.completed_in_run = 0;
      progress.body.stale_in_run = 0;
      progress.body.batch_size = INDEX_BODY_SCAN_BATCH_SIZE;
    });
  }

  /// Settles the synchronous worker transition without producing a false
  /// up-to-date state between a completed pass and the scheduler choosing its
  /// next delay. The scheduler replaces a `None` retry time with its exact
  /// deadline immediately after it receives the refresh result.
  fn finish_session_index_refresh(&self, result: &Result<IndexRefresh, String>) {
    let catalog_has_errors = self
      .catalog_errors
      .lock()
      .map(|errors| !errors.is_empty())
      .unwrap_or(true);
    let needs_retry = match result {
      Ok(refresh) => {
        refresh.has_pending_body_jobs
          || refresh.retry_catalog_soon
          || !refresh.retry_changed_file_paths.is_empty()
          || catalog_has_errors
      }
      Err(_) => true,
    };
    let settlement = if needs_retry {
      IndexProgressSettlement::WaitingToRetry {
        retry_at_ms: None,
        worker_error: None,
      }
    } else {
      IndexProgressSettlement::Idle
    };
    self.index_progress.settle_after_latest_refresh(settlement);
  }

  pub(crate) fn pause_native_index(&self) {
    self.refresh_index_progress_from_index();
    self.index_progress.update(set_index_progress_idle);
  }

  pub(crate) fn follow_shared_index(&self) -> Result<bool, String> {
    let changed = self.observe_shared_index_change()?;
    // The constructor already recovered the durable progress snapshot. An
    // index follower wakes once per second, so rebuilding the grouped body
    // totals while SQLite's data_version is unchanged turns an idle second
    // viewer into a persistent whole-index scan.
    if changed {
      self.refresh_index_progress_from_index();
    }
    self.index_progress.update(|progress| {
      progress.activity = IndexActivity::WaitingForIndexer;
      progress.is_refreshing = false;
      progress.catalog.active_provider = None;
      progress.body.active_provider = None;
      progress.worker_error = None;
      progress.retry_at_ms = None;
    });
    Ok(changed)
  }

  fn refresh_index_progress_from_index(&self) {
    if let Ok(pending_providers) = self.pending_catalog_providers() {
      self.index_progress.update(|progress| {
        progress.catalog.pending_providers = pending_providers;
      });
    }
    if let Ok(counts) = self.staged_body_progress_counts() {
      self.replace_indexed_body_progress_counts(&counts);
    }
  }

  fn staged_body_progress_counts(&self) -> Result<Vec<StagedSessionBaselineSourceCount>, String> {
    self
      .session_index
      .staged_session_baseline_source_counts(&[INDEX_PENDING_BODY_CURSOR_PREFIX, LEGACY_PENDING_BODY_CURSOR_PREFIX])
      .map(|counts| {
        counts
          .into_iter()
          .filter(|count| {
            !ViewerProvider::ALL
              .into_iter()
              .any(|provider| provider.as_str() == count.provider && self.relay.external_catalog_covers(provider))
          })
          .collect()
      })
      .map_err(|error| format!("failed to read staged session-index progress: {error}"))
  }

  fn pending_catalog_providers(&self) -> Result<Vec<ViewerProvider>, String> {
    ViewerProvider::ALL
      .into_iter()
      .filter(|provider| !self.relay.external_catalog_covers(*provider))
      .map(|provider| {
        self
          .session_index
          .source_state(&index_catalog_source_key(provider))
          .map_err(|error| format!("failed to read the local {provider:?} session catalog: {error}"))
          .map(|catalog| catalog.is_none().then_some(provider))
      })
      .collect::<Result<Vec<_>, _>>()
      .map(|providers| providers.into_iter().flatten().collect())
  }

  fn set_catalog_active_provider(&self, provider: ViewerProvider) {
    self.index_progress.update(|progress| {
      progress.is_refreshing = true;
      progress.activity = IndexActivity::Catalog;
      progress.worker_error = None;
      progress.retry_at_ms = None;
      progress.catalog.active_provider = Some(provider);
    });
  }

  fn finish_catalog_provider(&self, provider: ViewerProvider, failed: bool, resolves_catalog_error: bool) {
    self.index_progress.update(|progress| {
      progress.catalog.active_provider = None;
      progress.catalog.processed_providers = progress
        .catalog
        .processed_providers
        .saturating_add(1)
        .min(progress.catalog.total_providers);
      if failed {
        if !progress.catalog.error_providers.contains(&provider) {
          progress.catalog.error_providers.push(provider);
        }
      } else if resolves_catalog_error {
        progress.catalog.error_providers.retain(|current| *current != provider);
      }
      progress.catalog.error_providers = ordered_providers(&progress.catalog.error_providers);
    });
  }

  fn replace_body_progress_queue(&self, jobs: &[PendingBodyJob]) -> Result<(), String> {
    let (_, _, queued_providers) = body_queue_progress(jobs);
    let counts = self.staged_body_progress_counts()?;
    let (_, mut providers) = indexed_body_queue_progress(&counts);
    for queued in queued_providers {
      let current = provider_body_mut(&mut providers, queued.provider);
      // Failed jobs are process-local retry state. Keep them out of durable
      // totals while ensuring the visible count remains a subset of the
      // durable remaining queue.
      current.failed_jobs = queued.failed_jobs.min(current.pending_jobs);
    }
    self.index_progress.update(|progress| {
      progress.body.active_provider = None;
      progress.body.batch_size = INDEX_BODY_SCAN_BATCH_SIZE;
      progress.body.providers = providers;
      update_body_progress_totals(&mut progress.body);
    });
    Ok(())
  }

  fn replace_body_progress_after_catalog(&self, jobs: &[PendingBodyJob], catalog_changed: bool) -> Result<(), String> {
    // A changed catalog can reset or remove a staged source, and a non-empty
    // queue needs current durable totals. When both are false, the existing
    // in-memory progress is already authoritative and rerunning the grouped
    // aggregate would scan every present indexed session on each idle pass.
    if catalog_changed || !jobs.is_empty() {
      self.replace_body_progress_queue(jobs)?;
    }
    Ok(())
  }

  fn replace_indexed_body_progress_counts(&self, counts: &[StagedSessionBaselineSourceCount]) {
    let (_, providers) = indexed_body_queue_progress(counts);
    self.index_progress.update(|progress| {
      progress.body.active_provider = None;
      progress.body.batch_size = INDEX_BODY_SCAN_BATCH_SIZE;
      progress.body.providers = providers;
      // Failed jobs are intentionally process-local. A fresh process has no
      // durable failure record to show until its own worker observes one.
      update_body_progress_totals(&mut progress.body);
    });
  }

  fn set_body_active_provider(&self, provider: ViewerProvider) {
    self.index_progress.update(|progress| {
      progress.is_refreshing = true;
      progress.activity = IndexActivity::Body;
      progress.worker_error = None;
      progress.retry_at_ms = None;
      progress.body.active_provider = Some(provider);
    });
  }

  fn record_body_progress_completion(&self, job: &PendingBodyJob) {
    self.index_progress.update(|progress| {
      progress.body.completed_in_run = progress.body.completed_in_run.saturating_add(1);
      record_body_progress_handled(&mut progress.body, job);
    });
  }

  fn record_body_progress_stale(&self, job: &PendingBodyJob) {
    self.index_progress.update(|progress| {
      progress.body.stale_in_run = progress.body.stale_in_run.saturating_add(1);
      record_body_progress_handled(&mut progress.body, job);
    });
  }

  fn record_body_progress_failure(&self, job: &PendingBodyJob) {
    if job.deprioritized {
      return;
    }
    self.index_progress.update(|progress| {
      let current = provider_body_mut(&mut progress.body.providers, job.provider);
      current.failed_jobs = current.failed_jobs.saturating_add(1).min(current.pending_jobs);
      update_body_progress_totals(&mut progress.body);
    });
  }

  pub fn list_sessions(&self, request: ListSessionsRequest) -> Result<ListSessionsResponse, String> {
    let limit = bounded_limit(request.limit)?;
    let offset = requested_offset(request.cursor.as_deref(), request.offset, decode_list_cursor)?.unwrap_or(0);
    let providers = selected_providers(&request.query.providers);
    let search = request
      .query
      .search
      .as_deref()
      .map(str::trim)
      .filter(|value| !value.is_empty())
      .map(str::to_lowercase);
    let mut candidates = Vec::new();
    let mut inventories = self.indexed_session_inventories(&providers)?;
    let mut source_errors = std::mem::take(&mut inventories.source_errors);

    for provider in providers {
      if let Some(message) = self.index_error_for(provider).filter(|_| self.session_scope.is_none()) {
        record_source_error(&mut source_errors, provider, message);
      }
      let Some(inventory) = inventories.by_provider.remove(&provider) else {
        continue;
      };

      let relations = session_relation_index(provider, inventory.headers, &mut source_errors);
      let attention = session_relation_attention(provider, &relations, &inventory.direct_attention);
      for (index, header) in relations.headers.into_iter().enumerate() {
        // A child is hidden from the root roster only when its parent is a
        // present, canonical header and its relation does not make a cycle.
        // That deliberately keeps orphaned and cyclic records discoverable.
        if relations.parent_indices[index].is_some() {
          continue;
        }
        candidates.push(SessionListCandidate {
          provider,
          header,
          child_count: relations.child_counts[index],
          is_subagent: false,
          attention: attention[index],
        });
      }
    }

    let project_keys = candidates
      .iter()
      .filter_map(|candidate| project_key(&candidate.header).map(str::to_owned))
      .collect::<Vec<_>>();
    let discovered_at_ms = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default()
      .as_millis()
      .min(i64::MAX as u128) as i64;
    let project_order = self
      .session_index
      .project_groups(&project_keys, discovered_at_ms)
      .map_err(|error| format!("failed to read project order: {error}"))?;

    if let Some(search) = search.as_deref() {
      let mut matches = Vec::new();
      for candidate in candidates {
        let summary = match session_summary_with_child_count(
          candidate.provider,
          candidate.header.clone(),
          candidate.child_count,
          candidate.is_subagent,
          candidate.attention,
        ) {
          Ok(mut summary) => {
            apply_project_group(&mut summary, &project_order);
            summary
          }
          Err(message) => {
            record_source_error(&mut source_errors, candidate.provider, message);
            continue;
          }
        };
        if matches_search(&summary, Some(search)) {
          matches.push(candidate);
        }
      }
      sort_root_candidates(&mut matches, request.query.order, &project_order);
      let start = offset.min(matches.len());
      let end = start.saturating_add(limit).min(matches.len());
      let next_cursor = (end < matches.len()).then(|| encode_list_cursor(end));
      let mut sessions = Vec::with_capacity(end - start);
      for candidate in matches[start..end].iter().cloned() {
        match session_summary_with_child_count(
          candidate.provider,
          candidate.header,
          candidate.child_count,
          candidate.is_subagent,
          candidate.attention,
        ) {
          Ok(mut summary) => {
            apply_project_group(&mut summary, &project_order);
            sessions.push(summary);
          }
          Err(message) => record_source_error(&mut source_errors, candidate.provider, message),
        }
      }
      return Ok(ListSessionsResponse {
        sessions,
        next_cursor,
        pending_providers: inventories.pending_providers,
        source_errors,
      });
    }

    sort_root_candidates(&mut candidates, request.query.order, &project_order);
    let start = offset.min(candidates.len());
    let end = start.saturating_add(limit).min(candidates.len());
    let next_cursor = (end < candidates.len()).then(|| encode_list_cursor(end));
    let mut sessions = Vec::with_capacity(end - start);
    for candidate in candidates[start..end].iter().cloned() {
      match session_summary_with_child_count(
        candidate.provider,
        candidate.header,
        candidate.child_count,
        candidate.is_subagent,
        candidate.attention,
      ) {
        Ok(mut summary) => {
          apply_project_group(&mut summary, &project_order);
          sessions.push(summary);
        }
        Err(message) => record_source_error(&mut source_errors, candidate.provider, message),
      }
    }

    Ok(ListSessionsResponse {
      sessions,
      next_cursor,
      pending_providers: inventories.pending_providers,
      source_errors,
    })
  }

  /// Lists a bounded page of direct child headers without reading any child
  /// conversation body. The opaque parent key binds the request to one
  /// provider and one source record, so raw provider-local IDs never connect
  /// sessions from different providers.
  pub fn list_session_children(
    &self,
    request: ListSessionChildrenRequest,
  ) -> Result<ListSessionChildrenResponse, String> {
    let limit = bounded_limit(request.limit)?;
    let offset = requested_offset(request.cursor.as_deref(), request.offset, decode_list_cursor)?.unwrap_or(0);
    let parent_locator = decode_session_key(&request.parent_session_key)?;
    let inventory = self
      .indexed_session_inventory(parent_locator.provider)?
      .ok_or_else(|| "session catalog is still being indexed".to_string())?;
    let mut ignored_errors = Vec::new();
    let relations = session_relation_index(parent_locator.provider, inventory.headers, &mut ignored_errors);
    let attention = session_relation_attention(parent_locator.provider, &relations, &inventory.direct_attention);
    let parent_index = relations
      .headers
      .iter()
      .position(|header| locator_for_header(parent_locator.provider, header) == parent_locator)
      .ok_or_else(|| "session key no longer matches its source record".to_string())?;

    let mut candidates = relations
      .headers
      .into_iter()
      .enumerate()
      .filter_map(|(index, header)| {
        (relations.parent_indices[index] == Some(parent_index)).then_some(SessionListCandidate {
          provider: parent_locator.provider,
          header,
          child_count: relations.child_counts[index],
          is_subagent: true,
          attention: attention[index],
        })
      })
      .collect::<Vec<_>>();
    sort_session_candidates(&mut candidates);

    let start = offset.min(candidates.len());
    let end = start.saturating_add(limit).min(candidates.len());
    let next_cursor = (end < candidates.len()).then(|| encode_list_cursor(end));
    let sessions = candidates[start..end]
      .iter()
      .cloned()
      .map(|candidate| {
        session_summary_with_child_count(
          candidate.provider,
          candidate.header,
          candidate.child_count,
          candidate.is_subagent,
          candidate.attention,
        )
      })
      .collect::<Result<Vec<_>, _>>()?;

    Ok(ListSessionChildrenResponse { sessions, next_cursor })
  }

  /// Admission check for untrusted remote keys. Decoding alone is insufficient:
  /// keys contain source paths, so only the committed catalog grants access.
  pub(crate) fn session_notifications(&self, keys: &[String]) -> Vec<serde_json::Value> {
    keys
      .iter()
      .filter_map(|key| {
        let locator = decode_session_key(key).ok()?;
        let indexed = self.session_index.session(&index_session_key(&locator).ok()?).ok()??;
        let attention = SessionAttention::from_index(&indexed);
        Some(serde_json::json!({
          "session_key": key, "has_unread": attention.has_unread,
          "unread_final_count": attention.unread_final_count, "is_running": attention.is_running,
          "question_attention": attention.question_attention,
        }))
      })
      .collect()
  }

  pub fn validate_session_key(&self, key: &str) -> Result<(), String> {
    self.check_session_scope(key)?;
    let locator = decode_session_key(key)?;
    if !self.relay.external_catalog_covers(locator.provider) {
      return crate::index_queries::snapshot_entry(&self.session_index, &locator)?
        .filter(|entry| entry.header.path == locator.source_path && entry.header.id == locator.session_id)
        .map(|_| ())
        .ok_or_else(|| "Session is not in this machine's catalog".into());
    }
    let inventory = self
      .indexed_session_inventory(locator.provider)?
      .ok_or("Session catalog is not ready")?;
    if inventory
      .headers
      .iter()
      .any(|header| locator_for_header(locator.provider, header) == locator)
    {
      Ok(())
    } else {
      Err("Session is not in this machine's catalog".into())
    }
  }

  /// Returns one provider's complete committed catalog from the durable index.
  /// A missing sentinel is an ordinary cold-start state, not permission to
  /// synchronously inspect provider storage on an IPC request.
  fn indexed_session_inventory(&self, provider: ViewerProvider) -> Result<Option<SessionHeaderInventory>, String> {
    if self.relay.external_catalog_covers(provider) {
      return Ok(self.relay.has_catalog().then(|| {
        self.scope_inventory(
          provider,
          SessionHeaderInventory {
            headers: self.relay.headers(provider),
            direct_attention: HashMap::new(),
          },
        )
      }));
    }
    let catalog = self
      .session_index
      .source_state(&index_catalog_source_key(provider))
      .map_err(|error| format!("failed to read the local {provider:?} session catalog: {error}"))?;
    if catalog.is_none() {
      return Ok(None);
    }

    let indexed = self
      .session_index
      .list_present_sessions_for_provider(provider.as_str())
      .map_err(|error| format!("failed to read the local {provider:?} session index: {error}"))?;
    let mut inventory = SessionHeaderInventory::default();
    for session in indexed {
      // An invalid row is not a reason to fall back to provider storage. The
      // root listing can report it as a source warning; an on-demand child or
      // delegation query simply excludes the unusable row.
      let Ok(header) = indexed_session_header(&session) else {
        continue;
      };
      inventory.direct_attention.insert(
        locator_for_header(provider, &header),
        SessionAttention::from_index(&session),
      );
      inventory.headers.push(header);
    }
    Ok(Some(self.scope_inventory(provider, inventory)))
  }

  /// Reads all sidebar-visible metadata from the durable index in one snapshot.
  /// The background reconciler is the only code path allowed to call provider
  /// header APIs, so the first viewer screen remains fast and deterministic
  /// even while a provider catalog is still pending.
  fn indexed_session_inventories(&self, providers: &[ViewerProvider]) -> Result<IndexedSessionInventories, String> {
    if providers
      .iter()
      .any(|provider| self.relay.external_catalog_covers(*provider))
    {
      let local: Vec<_> = providers
        .iter()
        .copied()
        .filter(|p| !self.relay.external_catalog_covers(*p))
        .collect();
      let mut result = self.indexed_session_inventories(&local)?;
      for provider in providers
        .iter()
        .copied()
        .filter(|p| self.relay.external_catalog_covers(*p))
      {
        match self.indexed_session_inventory(provider)? {
          Some(inventory) => {
            result.by_provider.insert(provider, inventory);
          }
          None => result.pending_providers.push(provider),
        }
      }
      if self.session_scope.is_some() {
        result.pending_providers.clear();
        result.source_errors.clear();
      }
      return Ok(result);
    }
    let mut committed_providers = HashSet::new();
    let mut pending_providers = Vec::new();
    for &provider in providers {
      let catalog = self
        .session_index
        .source_state(&index_catalog_source_key(provider))
        .map_err(|error| format!("failed to read the local {provider:?} session catalog: {error}"))?;
      if catalog.is_some() {
        committed_providers.insert(provider);
      } else {
        pending_providers.push(provider);
      }
    }

    let indexed = self
      .session_index
      .list_present_sessions()
      .map_err(|error| format!("failed to read the local session index: {error}"))?;
    let mut by_provider = committed_providers
      .iter()
      .copied()
      .map(|provider| (provider, SessionHeaderInventory::default()))
      .collect::<HashMap<_, _>>();
    let mut source_errors = Vec::new();

    for session in indexed {
      let Some(provider) = providers
        .iter()
        .copied()
        .find(|provider| session.key.provider == provider.as_str())
      else {
        continue;
      };
      let Some(inventory) = by_provider.get_mut(&provider) else {
        // Rows can remain from a previous catalog while this provider is
        // rebuilding. Hide them until its new complete sentinel commits.
        continue;
      };
      let header = match indexed_session_header(&session) {
        Ok(header) => header,
        Err(message) => {
          record_source_error(&mut source_errors, provider, message);
          continue;
        }
      };
      inventory.direct_attention.insert(
        locator_for_header(provider, &header),
        SessionAttention::from_index(&session),
      );
      inventory.headers.push(header);
    }

    Ok(IndexedSessionInventories {
      by_provider: by_provider
        .into_iter()
        .map(|(provider, inventory)| (provider, self.scope_inventory(provider, inventory)))
        .collect(),
      pending_providers: if self.session_scope.is_some() {
        Vec::new()
      } else {
        pending_providers
      },
      source_errors: if self.session_scope.is_some() {
        Vec::new()
      } else {
        source_errors
      },
    })
  }

  fn index_error_for(&self, provider: ViewerProvider) -> Option<String> {
    if self.relay.external_catalog_covers(provider) {
      return self.relay.status().error;
    }
    self.index_errors.lock().ok()?.get(&provider).cloned()
  }

  fn attention_revision_for_locator(&self, locator: &SessionLocator) -> Option<String> {
    if self.session_scope.is_some() {
      return None;
    }
    if self.relay.external_catalog_covers(locator.provider) {
      return None;
    }
    let catalog_key = index_catalog_source_key(locator.provider);
    if self.session_index.source_state(&catalog_key).ok().flatten().is_none() {
      return None;
    }
    let key = index_session_key(locator).ok()?;
    self
      .session_index
      .session(&key)
      .ok()
      .flatten()
      .filter(IndexedSession::has_unread)
      .map(|session| session.attention_revision.to_string())
  }

  /// Test helper that reconciles a complete provider-header pass and one
  /// bounded body batch in one call.
  #[cfg(test)]
  pub(crate) fn refresh_session_index(&self) -> Result<IndexRefresh, String> {
    let _refresh_gate = self
      .index_refresh_gate
      .lock()
      .map_err(|_| "session index refresh gate is poisoned".to_string())?;
    self.begin_session_index_catalog_refresh(CatalogRefreshScope::Full, ViewerProvider::ALL.len());
    let result = (|| {
      let catalog_refresh = self.refresh_session_catalogs();
      self.continue_session_index_body_refresh();
      let body_refresh = self.refresh_pending_body_jobs(&catalog_refresh.unavailable, true)?;
      let mut refresh = catalog_refresh.refresh;
      refresh.changed |= body_refresh.refresh.changed;
      refresh.catalog_refresh_required |= body_refresh.refresh.catalog_refresh_required;
      refresh
        .updated_session_keys
        .extend(body_refresh.refresh.updated_session_keys);
      refresh
        .attention_session_keys
        .extend(body_refresh.refresh.attention_session_keys);
      refresh.has_pending_body_jobs = body_refresh.refresh.has_pending_body_jobs;
      refresh.attention_session_keys.sort();
      refresh.attention_session_keys.dedup();
      self.finalize_index_refresh(
        refresh,
        combined_index_errors(catalog_refresh.errors, body_refresh.errors),
      )
    })();
    self.finish_session_index_refresh(&result);
    result
  }

  /// Commits only provider catalogs and returns before any session body is
  /// loaded. The Tauri scheduler uses this path on its normal cadence so a
  /// cold index can notify the sidebar as soon as each complete catalog
  /// sentinel is durable; body reconciliation follows in later one-second
  /// passes.
  pub(crate) fn refresh_session_catalog(&self) -> Result<IndexRefresh, String> {
    let _refresh_gate = self
      .index_refresh_gate
      .lock()
      .map_err(|_| "session index refresh gate is poisoned".to_string())?;
    self.begin_session_index_catalog_refresh(CatalogRefreshScope::Full, ViewerProvider::ALL.len());
    let result = (|| {
      let catalog_refresh = self.refresh_session_catalogs();
      let remaining_jobs = self.pending_body_jobs(&HashSet::new())?;
      self.replace_body_progress_after_catalog(&remaining_jobs, catalog_refresh.refresh.changed)?;
      let mut refresh = catalog_refresh.refresh;
      refresh.has_pending_body_jobs = !remaining_jobs.is_empty();
      self.finalize_index_refresh(
        refresh,
        combined_index_errors(
          catalog_refresh.errors,
          self.body_errors_for_pending_jobs(&remaining_jobs),
        ),
      )
    })();
    self.finish_session_index_refresh(&result);
    result
  }

  /// Reconciles a selected set of whole-provider catalogs without enumerating
  /// unrelated providers. The scheduler uses this for sources that do not yet
  /// have a native file watcher, preserving their short update cadence without
  /// repeatedly discovering large Codex or Pi rollout trees.
  pub(crate) fn refresh_session_catalog_providers(&self, providers: &[ViewerProvider]) -> Result<IndexRefresh, String> {
    let providers = ordered_providers(providers);
    if providers.is_empty() {
      return Ok(IndexRefresh::default());
    }
    let _refresh_gate = self
      .index_refresh_gate
      .lock()
      .map_err(|_| "session index refresh gate is poisoned".to_string())?;
    self.begin_session_index_catalog_refresh(CatalogRefreshScope::Full, providers.len());
    let result = (|| {
      let catalog_refresh = self.refresh_session_catalogs_for(&providers);
      let remaining_jobs = self.pending_body_jobs(&HashSet::new())?;
      self.replace_body_progress_after_catalog(&remaining_jobs, catalog_refresh.refresh.changed)?;
      let mut refresh = catalog_refresh.refresh;
      refresh.has_pending_body_jobs = !remaining_jobs.is_empty();
      let catalog_errors = self.catalog_errors_snapshot();
      refresh.has_catalog_errors = !catalog_errors.is_empty();
      self.finalize_index_refresh(
        refresh,
        combined_index_errors(catalog_errors, self.body_errors_for_pending_jobs(&remaining_jobs)),
      )
    })();
    self.finish_session_index_refresh(&result);
    result
  }

  /// Advances only the bounded body queue from an already committed catalog.
  /// The background scheduler calls this between normal catalog intervals so a
  /// large provider is not rediscovered for every eight-session body batch.
  #[cfg(test)]
  fn refresh_pending_session_index(&self) -> Result<IndexRefresh, String> {
    self.refresh_pending_session_index_with_failed_retries(true)
  }

  pub(crate) fn refresh_pending_session_index_automated(&self) -> Result<IndexRefresh, String> {
    self.refresh_pending_session_index_with_failed_retries(false)
  }

  fn refresh_pending_session_index_with_failed_retries(&self, retry_failed: bool) -> Result<IndexRefresh, String> {
    let _refresh_gate = self
      .index_refresh_gate
      .lock()
      .map_err(|_| "session index refresh gate is poisoned".to_string())?;
    self.begin_session_index_body_refresh();
    let result = (|| {
      let catalog_errors = self
        .catalog_errors
        .lock()
        .map(|current_errors| current_errors.clone())
        .unwrap_or_default();
      let body_refresh = self.refresh_pending_body_jobs(&HashSet::new(), retry_failed)?;
      let mut refresh = body_refresh.refresh;
      refresh.has_catalog_errors = !catalog_errors.is_empty();
      refresh.attention_session_keys.sort();
      refresh.attention_session_keys.dedup();
      self.finalize_index_refresh(refresh, combined_index_errors(catalog_errors, body_refresh.errors))
    })();
    self.finish_session_index_refresh(&result);
    result
  }

  /// Performs a complete header pass while the caller holds
  /// `index_refresh_gate`. Catalog failures are isolated per provider so a
  /// successful provider can publish its stable sentinel immediately.
  fn refresh_session_catalogs(&self) -> CatalogRefresh {
    self.refresh_session_catalogs_for(&ViewerProvider::ALL)
  }

  /// Performs a stable header catalog for just `providers` while preserving
  /// the last readable warning for every provider outside that set.
  fn refresh_session_catalogs_for(&self, providers: &[ViewerProvider]) -> CatalogRefresh {
    let mut refresh = IndexRefresh::default();
    let mut errors = HashMap::new();
    let mut unavailable = HashSet::new();
    let providers = ordered_providers(providers);
    for provider in providers.iter().copied() {
      if self.relay.external_catalog_covers(provider) {
        self.finish_catalog_provider(provider, false, true);
        continue;
      }
      self.set_catalog_active_provider(provider);
      match self.refresh_provider_catalog(provider) {
        Ok(provider_refresh) => {
          refresh.changed |= provider_refresh.changed;
          refresh.catalog_refresh_required |= provider_refresh.changed;
          refresh.retry_catalog_soon |= provider_refresh.retry_catalog_soon;
          refresh
            .attention_session_keys
            .extend(provider_refresh.attention_session_keys);
          self.finish_catalog_provider(provider, false, true);
        }
        Err(message) => {
          unavailable.insert(provider);
          errors.insert(provider, message);
          self.finish_catalog_provider(provider, true, true);
        }
      }
    }
    let catalog_errors = self.replace_catalog_errors_for(&providers, errors.clone());
    refresh.catalog_attempt_has_errors = !errors.is_empty();
    refresh.has_catalog_errors = !catalog_errors.is_empty();
    if let Ok(pending_providers) = self.pending_catalog_providers() {
      self.index_progress.update(|progress| {
        progress.catalog.pending_providers = pending_providers;
      });
    }
    CatalogRefresh {
      refresh,
      errors,
      unavailable,
    }
  }

  /// The catalog commits first, so even a large historical provider becomes
  /// listable before its message bodies are inspected. Process a bounded,
  /// global newest-first batch afterwards; a malformed rollout cannot starve
  /// older pending sources and no one refresh can become a whole-history
  /// parse again.
  fn refresh_pending_body_jobs(
    &self,
    unavailable: &HashSet<ViewerProvider>,
    retry_failed: bool,
  ) -> Result<BodyBackfillRefresh, String> {
    let mut refresh = IndexRefresh::default();
    let mut attention_session_keys = HashSet::new();
    let mut pending_jobs = self.pending_body_jobs(unavailable)?;
    self.replace_body_progress_queue(&pending_jobs)?;
    let mut processed = 0;
    for index in 0..pending_jobs.len() {
      if processed >= INDEX_BODY_SCAN_BATCH_SIZE {
        break;
      }
      let job = pending_jobs[index].clone();
      if job.deprioritized && !retry_failed {
        continue;
      }
      self.set_body_active_provider(job.provider);
      match self.refresh_pending_body_job(&job) {
        Ok(BodyJobRefresh::Deferred) => continue,
        Ok(BodyJobRefresh::Stale) => {
          processed += 1;
          self.clear_failed_body_job(&job);
          self.record_body_progress_stale(&job);
          // A stale body job commonly means another viewer process committed
          // the same source while this process was loading it. Request a
          // sidebar reread so this window observes that durable winner's
          // title, preview, or attention revision. Source-file churn can also
          // reach this path; an extra SQLite-only reread is harmless.
          refresh.changed = true;
          refresh.catalog_refresh_required = true;
        }
        Ok(BodyJobRefresh::Updated {
          provider_refresh,
          next_source,
        }) => {
          processed += 1;
          self.clear_failed_body_job(&job);
          self.record_body_progress_completion(&job);
          refresh.changed |= provider_refresh.changed;
          refresh.catalog_refresh_required |= provider_refresh.presentation_changed;
          refresh.updated_session_keys.push(encode_session_key(&job.locator)?);
          refresh.changed = true;
          attention_session_keys.extend(provider_refresh.attention_session_keys);
          Self::retarget_pending_body_jobs(&mut pending_jobs[index + 1..], &job.source, next_source);
        }
        Err(message) => {
          processed += 1;
          self.record_failed_body_job(&job, message);
          self.record_body_progress_failure(&job);
        }
      }
    }
    // A catalog failure skips that provider for this full pass, but its prior
    // staged work still keeps the one-second body scheduler alive. The next
    // body-only pass can make progress without waiting for another catalog.
    let remaining_jobs = self.pending_body_jobs(&HashSet::new())?;
    self.replace_body_progress_queue(&remaining_jobs)?;
    refresh.has_pending_body_jobs = remaining_jobs.iter().any(|job| !job.deprioritized);
    refresh.attention_session_keys = attention_session_keys.into_iter().collect();
    Ok(BodyBackfillRefresh {
      refresh,
      errors: self.body_errors_for_pending_jobs(&remaining_jobs),
    })
  }

  fn catalog_errors_snapshot(&self) -> HashMap<ViewerProvider, String> {
    self
      .catalog_errors
      .lock()
      .map(|errors| errors.clone())
      .unwrap_or_default()
  }

  /// Replaces catalog warnings for exactly the providers that were attempted,
  /// retaining warnings for sources outside a provider-local scan.
  fn replace_catalog_errors_for(
    &self,
    providers: &[ViewerProvider],
    errors: HashMap<ViewerProvider, String>,
  ) -> HashMap<ViewerProvider, String> {
    let catalog_errors = if let Ok(mut current_errors) = self.catalog_errors.lock() {
      current_errors.retain(|provider, _| !providers.contains(provider));
      current_errors.extend(errors);
      current_errors.clone()
    } else {
      errors
    };
    let error_providers = ordered_error_providers(&catalog_errors);
    self.index_progress.update(|progress| {
      progress.catalog.error_providers = error_providers;
    });
    catalog_errors
  }

  fn finish_index_refresh(&self, mut refresh: IndexRefresh, errors: HashMap<ViewerProvider, String>) -> IndexRefresh {
    // A provider warning is part of the sidebar state too. Notify when it is
    // added, removed, or changes even if no session row happened to change;
    // otherwise a recovered source could leave a stale warning on screen.
    if let Ok(mut current_errors) = self.index_errors.lock() {
      refresh.catalog_refresh_required |= *current_errors != errors;
      refresh.changed |= *current_errors != errors;
      *current_errors = errors;
    }
    refresh
  }

  fn finalize_index_refresh(
    &self,
    refresh: IndexRefresh,
    errors: HashMap<ViewerProvider, String>,
  ) -> Result<IndexRefresh, String> {
    let mut refresh = self.finish_index_refresh(refresh, errors);
    let shared_change = self.observe_shared_index_change()?;
    refresh.changed |= shared_change;
    refresh.catalog_refresh_required |= shared_change;
    Ok(refresh)
  }

  pub(crate) fn observe_shared_index_change(&self) -> Result<bool, String> {
    let current = self
      .session_index
      .data_version()
      .map_err(|error| format!("failed to observe shared session index changes: {error}"))?;
    let mut observed = self
      .observed_index_data_version
      .lock()
      .map_err(|_| "viewer index data-version state is poisoned".to_owned())?;
    // A constructor-time observation can fail transiently. In that case, use
    // the first successful pass as a conservative reread signal rather than
    // risking a stale sidebar after another process committed meanwhile.
    let changed = match *observed {
      Some(previous) => previous != current,
      None => true,
    };
    *observed = Some(current);
    Ok(changed)
  }

  fn catalog_index_snapshot(&self, provider: ViewerProvider) -> Result<CatalogIndexSnapshot, String> {
    let catalog_key = index_catalog_source_key(provider);
    let provider_ready = self
      .session_index
      .source_state(&catalog_key)
      .map_err(|error| format!("failed to read the {provider:?} index baseline: {error}"))?
      .is_some();
    let existing_sources = self
      .session_index
      .list_sources(provider.as_str())
      .map_err(|error| format!("failed to read indexed {provider:?} sources: {error}"))?;
    let existing_sessions = self
      .session_index
      .list_all_sessions_for_provider(provider.as_str())
      .map_err(|error| format!("failed to read indexed {provider:?} session metadata: {error}"))?;
    Ok(CatalogIndexSnapshot {
      provider_ready,
      existing_sources,
      existing_sessions,
    })
  }

  /// Reconciles an established file-backed catalog from directory membership
  /// and source revisions. Unchanged files reuse their bounded indexed header;
  /// only new or modified files are opened. This keeps API restarts cheap while
  /// still detecting changes that happened while no watcher was running.
  fn provider_catalog_headers(
    &self,
    provider: ViewerProvider,
    snapshot: &CatalogIndexSnapshot,
    apply_catalog_metadata: bool,
  ) -> Result<Vec<SessionHeader>, String> {
    if !snapshot.provider_ready || !matches!(provider, ViewerProvider::Codex | ViewerProvider::Pi) {
      return self.repository.list_session_headers(provider);
    }

    let existing_by_source = snapshot
      .existing_sources
      .iter()
      .map(|source| (source.key.source_key.as_str(), source))
      .collect::<HashMap<_, _>>();
    let present_by_source = snapshot
      .existing_sessions
      .iter()
      .filter(|session| session.present && session.key.provider == provider.as_str())
      .fold(HashMap::<&str, Vec<&IndexedSession>>::new(), |mut grouped, session| {
        grouped
          .entry(session.key.source_key.as_str())
          .or_default()
          .push(session);
        grouped
      });
    let paths = match self.repository.file_session_paths(provider) {
      Ok(paths) => paths,
      Err(_) => return self.repository.list_session_headers(provider),
    };
    let mut headers = Vec::new();
    for path in paths {
      let Ok(source_key) = index_source_key_for_path(provider, &path) else {
        continue;
      };
      let Ok(cursor) = source_cursor(provider, &path) else {
        continue;
      };
      let previous = existing_by_source.get(source_key.source_key.as_str()).copied();
      let existing = present_by_source
        .get(source_key.source_key.as_str())
        .and_then(|sessions| (sessions.len() == 1).then_some(sessions[0]));

      if previous.and_then(|state| indexed_source_raw_cursor(&state.cursor)) == Some(cursor.as_str())
        && let Some(existing) = existing
        && let Ok(header) = indexed_session_header(existing)
      {
        headers.push(header);
        continue;
      }

      let Ok(mut header) = self.repository.session_header_at_path(provider, &path) else {
        continue;
      };
      if let Some(existing) = existing.filter(|session| session.key.session_id == header.id) {
        header = retain_targeted_catalog_presentation(provider, header, existing);
      }
      headers.push(header);
    }
    if apply_catalog_metadata {
      self.repository.apply_catalog_metadata(provider, &mut headers)?;
    }
    Ok(headers)
  }

  /// Builds one provider's complete header catalog without reading any message
  /// bodies. A catalog sentinel becomes visible only after a stable complete
  /// pass commits atomically, so a first-run sidebar never observes a partial
  /// provider snapshot.
  fn refresh_provider_catalog(&self, provider: ViewerProvider) -> Result<ProviderIndexRefresh, String> {
    let snapshot = self.catalog_index_snapshot(provider)?;
    let headers = self.provider_catalog_headers(provider, &snapshot, true)?;
    let catalog_topology = session_catalog_topology(provider, &headers)?;
    let catalog_key = index_catalog_source_key(provider);
    let provider_ready = snapshot.provider_ready;
    let existing_by_key = snapshot
      .existing_sources
      .iter()
      .map(|source| (source.key.source_key.clone(), source))
      .collect::<HashMap<_, _>>();
    let existing_by_session_key = snapshot
      .existing_sessions
      .iter()
      .map(|session| (session.key.clone(), session))
      .collect::<HashMap<_, _>>();
    let existing_by_session_id = snapshot
      .existing_sessions
      .iter()
      .filter(|session| session.key.provider == provider.as_str())
      .fold(
        HashMap::<String, Vec<&IndexedSession>>::new(),
        |mut grouped, session| {
          grouped.entry(session.key.session_id.clone()).or_default().push(session);
          grouped
        },
      );
    let present_session_counts_by_source = snapshot
      .existing_sessions
      .iter()
      .filter(|session| session.present && session.key.provider == provider.as_str())
      .fold(HashMap::<String, usize>::new(), |mut counts, session| {
        *counts.entry(session.key.source_key.clone()).or_default() += 1;
        counts
      });
    let mut headers_by_source = BTreeMap::<String, Vec<SessionHeader>>::new();
    let mut paths_by_source = HashMap::<String, PathBuf>::new();

    for header in headers {
      let source_key = index_source_key_for_path(provider, &header.path)?;
      let key = source_key.source_key.clone();
      paths_by_source
        .entry(key.clone())
        .or_insert_with(|| header.path.clone());
      headers_by_source.entry(key).or_default().push(header);
    }
    let current_source_keys = paths_by_source.keys().cloned().collect::<HashSet<_>>();
    let indexed_at_ms = current_time_ms();
    let mut replacements = Vec::new();
    let mut checked_sources = Vec::new();

    // File-backed sources each contain one session. OpenCode groups all rows
    // under its database path, so one catalog source can contain many rows.
    // This pass only stores bounded headers; its staged cursor records that a
    // later body job must establish notification state for the observed source
    // revision.
    for (source_key_string, source_headers) in headers_by_source {
      let source_path = paths_by_source
        .get(&source_key_string)
        .expect("source path should be collected with its headers");
      let cursor = source_cursor(provider, source_path)?;
      let previous = existing_by_key.get(&source_key_string).copied();
      let source_key = SourceKey::new(provider.as_str(), source_key_string);
      let source_headers = canonical_session_headers(source_headers);
      let source_changed = previous.and_then(|state| indexed_source_raw_cursor(&state.cursor)) != Some(cursor.as_str());
      if provider_ready && previous.is_some() && source_changed {
        self.relay.changed_source(provider, source_path);
      }
      let source_has_pending_session = source_headers.iter().any(|header| {
        let key = IndexedSessionKey::new(
          source_key.provider.clone(),
          source_key.source_key.clone(),
          header.id.clone(),
        );
        existing_by_session_key
          .get(&key)
          .is_some_and(|session| !session.attention_baselined || needs_activity_upgrade(session))
      });
      let source_has_new_session = source_headers.iter().any(|header| {
        let key = IndexedSessionKey::new(
          source_key.provider.clone(),
          source_key.source_key.clone(),
          header.id.clone(),
        );
        !existing_by_session_key.contains_key(&key)
      });
      let body_pending = source_changed || source_has_pending_session || source_has_new_session;
      let staged_cursor = if body_pending {
        previous
          .filter(|state| !source_changed && pending_body_raw_cursor(&state.cursor) == Some(cursor.as_str()))
          .map(|state| state.cursor.clone())
          .unwrap_or_else(|| pending_body_cursor(&cursor))
      } else {
        completed_body_cursor(&cursor)
      };

      let sessions = source_headers
        .iter()
        .map(|header| {
          let key = IndexedSessionKey::new(
            source_key.provider.clone(),
            source_key.source_key.clone(),
            header.id.clone(),
          );
          let existing = existing_by_session_key.get(&key).copied();
          let mut metadata =
            catalog_session_metadata(&source_key, header.clone(), existing, provider_ready, source_changed)?;
          // Codex commonly moves a completed rollout to the archive directory.
          // The new path must not masquerade as a freshly-created conversation.
          if existing.is_none()
            && let Some(relocated) =
              relocated_session_reference(&source_key, &metadata, &current_source_keys, &existing_by_session_id)
          {
            // A move replaces the old source inventory before the archive body
            // can be read. Preserve an existing unread dot in the staged target
            // so a transient archive read failure never makes it disappear.
            // The later body completion compares against this retained marker
            // and therefore does not create a duplicate revision for the move.
            metadata.notify_on_baseline = false;
            // The archive header is often blank. Retain the old body's
            // fallback separately from catalog presentation until this new
            // source can be loaded, so a transient archive error does not
            // make a known session look untitled.
            metadata.body_title = relocated.body_title.clone();
            metadata.body_preview = relocated.body_preview.clone();
            if relocated.has_unread() {
              metadata.attention_marker = known_attention_marker(relocated);
              metadata.has_new_attention = true;
              metadata.new_attention_count = (relocated.attention_revision - relocated.seen_attention_revision) as u64;
            }
          }
          Ok(metadata)
        })
        .collect::<Result<Vec<_>, String>>()?;
      let source_matches_catalog = indexed_source_matches_catalog(
        &sessions,
        &existing_by_session_key,
        present_session_counts_by_source
          .get(&source_key.source_key)
          .copied()
          .unwrap_or_default(),
      );
      let cursor_matches_catalog = previous.is_some_and(|state| state.cursor == staged_cursor);
      if source_matches_catalog && cursor_matches_catalog {
        continue;
      }

      checked_sources.push((source_key.source_key.clone(), source_path.clone(), cursor.clone()));
      replacements.push(
        SourceReplacement::new(SourceState::new(source_key, staged_cursor, indexed_at_ms), sessions)
          .with_source_cursor_precondition(source_cursor_precondition(previous)),
      );
    }

    for source in &snapshot.existing_sources {
      if source.key.source_key == INDEX_CATALOG_SOURCE_KEY
        || current_source_keys.contains(&source.key.source_key)
        || source.cursor == INDEX_MISSING_SOURCE_CURSOR
      {
        continue;
      }
      replacements.push(SourceReplacement {
        source: SourceState::new(source.key.clone(), INDEX_MISSING_SOURCE_CURSOR, indexed_at_ms),
        sessions: Vec::new(),
        attention_mode: Default::default(),
        source_cursor_precondition: SourceCursorPrecondition::existing(source),
      });
    }

    if replacements.is_empty() && provider_ready {
      return Ok(ProviderIndexRefresh::default());
    }

    // A second header pass proves source/session membership stayed stable while
    // the catalog was built. Presentation metadata and file revisions are
    // intentionally not part of this comparison: Codex updates both while a
    // rollout is active, and that is ordinary provider activity rather than a
    // read failure.
    let confirmed_headers = self.provider_catalog_headers(provider, &snapshot, false)?;
    let confirmed_topology = session_catalog_topology(provider, &confirmed_headers)?;
    let mut retry_catalog_soon = catalog_topology != confirmed_topology;

    if retry_catalog_soon {
      // Commit only source snapshots whose complete session membership appears
      // in both passes. A newly created, removed, or moved source waits for a
      // stable retry; cached rows remain visible and no provider error is
      // shown. Tombstones are safe only when the source is absent in both
      // inventories.
      replacements.retain(|replacement| {
        let source_key = &replacement.source.key.source_key;
        if source_key == INDEX_CATALOG_SOURCE_KEY {
          return false;
        }
        if current_source_keys.contains(source_key) {
          return catalog_topology.get(source_key) == confirmed_topology.get(source_key);
        }
        !confirmed_topology.contains_key(source_key)
      });
      checked_sources
        .retain(|(source_key, _, _)| catalog_topology.get(source_key) == confirmed_topology.get(source_key));
    }

    // Per-source cursor verification is deliberately granular. One active
    // rollout should not prevent thousands of unrelated catalog rows from
    // committing; its previous row remains until the next catalog observes a
    // stable revision.
    let changed_sources = checked_sources
      .iter()
      .map(|(source_key, source_path, cursor)| {
        let confirmed_cursor = source_cursor(provider, source_path)?;
        Ok((confirmed_cursor != *cursor).then_some(source_key.clone()))
      })
      .collect::<Result<Vec<_>, String>>()?
      .into_iter()
      .flatten()
      .collect::<HashSet<_>>();
    if !changed_sources.is_empty() {
      retry_catalog_soon = true;
      replacements.retain(|replacement| !changed_sources.contains(&replacement.source.key.source_key));
    }

    // The sentinel means a complete header catalog is available, not that every
    // source body has been read. It also makes an empty provider catalog
    // explicit, so a later first session is correctly identified as new.
    if !provider_ready && !retry_catalog_soon {
      replacements.push(
        SourceReplacement::new(SourceState::new(catalog_key, "headers.v2", indexed_at_ms), Vec::new())
          .with_source_cursor_precondition(SourceCursorPrecondition::missing()),
      );
    }

    // Every safe source selected above commits in one transaction. The
    // readiness sentinel is included only for a stable complete pass; a
    // cursor race therefore cannot publish a partial first-run provider tree.
    if replacements.is_empty() {
      return Ok(ProviderIndexRefresh {
        retry_catalog_soon,
        ..Default::default()
      });
    }
    match self.session_index.replace_sources(&replacements) {
      Ok(_) => {}
      // Another viewer process won the race after this process completed its
      // stable header scan. Its committed catalog remains valid; retry the
      // full catalog soon rather than surfacing a spurious source warning.
      Err(SessionIndexError::SourceCursorConflict { .. }) => {
        return Ok(ProviderIndexRefresh {
          // The other process committed a new shared SQLite snapshot. Tell
          // this process's sidebar to reread it even though this writer did
          // not apply a local replacement.
          changed: true,
          retry_catalog_soon: true,
          ..Default::default()
        });
      }
      Err(error) => return Err(format!("failed to update the {provider:?} session index: {error}")),
    }
    Ok(ProviderIndexRefresh {
      changed: true,
      attention_session_keys: Vec::new(),
      retry_catalog_soon,
      ..Default::default()
    })
  }

  /// Reconciles ordinary writes to already-indexed file-backed session
  /// sources. This deliberately does not create or tombstone sources: a
  /// create, remove, or move can change a provider's membership and must go
  /// through [`Self::refresh_provider_catalog`] so relocation and unread state
  /// remain atomic. A target that cannot prove its old source is still the
  /// same source simply asks the scheduler for that full path.
  fn refresh_changed_file_provider_catalog(
    &self,
    provider: ViewerProvider,
    paths: &BTreeSet<PathBuf>,
  ) -> Result<ProviderIndexRefresh, String> {
    if self.relay.external_catalog_covers(provider) {
      return Ok(ProviderIndexRefresh::default());
    }
    if !matches!(provider, ViewerProvider::Codex | ViewerProvider::Pi) {
      return Ok(ProviderIndexRefresh {
        retry_catalog_soon: true,
        ..Default::default()
      });
    }

    // A targeted write can only refine an already complete provider catalog.
    // Publishing a partial initial catalog would make the sidebar look empty
    // or incomplete while a root is still being discovered.
    if self
      .session_index
      .source_state(&index_catalog_source_key(provider))
      .map_err(|error| format!("failed to read the {provider:?} index baseline: {error}"))?
      .is_none()
    {
      return Ok(ProviderIndexRefresh {
        retry_catalog_soon: true,
        ..Default::default()
      });
    }

    let mut replacements = Vec::new();
    let mut retry_catalog_soon = false;
    let mut retry_changed_file_paths = BTreeSet::new();
    let indexed_at_ms = current_time_ms();

    for path in paths {
      if !is_targeted_file_path(path) {
        retry_catalog_soon = true;
        continue;
      }
      let source_key = match index_source_key_for_path(provider, path) {
        Ok(source_key) => source_key,
        Err(_) => {
          retry_catalog_soon = true;
          continue;
        }
      };
      let Some(previous) = self
        .session_index
        .source_state(&source_key)
        .map_err(|error| format!("failed to read indexed {provider:?} source: {error}"))?
      else {
        // A watcher can race initial discovery, a session move, or a previous
        // full scan. Do not turn an unfamiliar path into a new source here.
        retry_catalog_soon = true;
        continue;
      };
      if previous.cursor == INDEX_MISSING_SOURCE_CURSOR {
        retry_catalog_soon = true;
        continue;
      }
      // Codex and Pi JSONL sources each represent exactly one session. A
      // different cardinality means the path's old membership cannot safely
      // be replaced with one direct header read.
      let Some(existing) = self
        .session_index
        .unique_present_session_for_source(&source_key)
        .map_err(|error| format!("failed to read indexed {provider:?} session: {error}"))?
      else {
        retry_catalog_soon = true;
        continue;
      };

      let cursor = match source_cursor(provider, path) {
        Ok(cursor) => cursor,
        Err(_) => {
          retry_catalog_soon = true;
          continue;
        }
      };
      // Native watching and Relay can report the same append independently.
      // Once that revision is staged, its pending body state is already owned
      // by the body queue; a duplicate hint needs neither a header read nor a
      // catalog replacement.
      if indexed_source_raw_cursor(&previous.cursor) == Some(cursor.as_str()) {
        continue;
      }
      let header = match self.repository.session_header_at_path(provider, path) {
        Ok(header) => header,
        Err(_) => {
          // A write can race a close, rename, or delete. Leave the old index
          // row intact and let a full inventory establish the new topology.
          retry_catalog_soon = true;
          continue;
        }
      };
      let header_source_key = match index_source_key_for_path(provider, &header.path) {
        Ok(header_source_key) => header_source_key,
        Err(_) => {
          retry_catalog_soon = true;
          continue;
        }
      };
      if header_source_key != source_key {
        retry_catalog_soon = true;
        continue;
      }
      let session_key = IndexedSessionKey::new(
        source_key.provider.clone(),
        source_key.source_key.clone(),
        header.id.clone(),
      );
      if existing.key != session_key {
        // A changed session ID can be an archive/move sequence. Full catalog
        // owns the old-source tombstone and the new-source relocation state.
        retry_catalog_soon = true;
        continue;
      }
      if self
        .session_index
        .has_other_present_session_source(&session_key)
        .map_err(|error| format!("failed to check indexed {provider:?} session identity: {error}"))?
      {
        retry_catalog_soon = true;
        continue;
      }

      // Targeted Codex headers deliberately avoid private Desktop state and
      // legacy-index lookups. Keep presentation that the previous complete
      // catalog established when that raw header does not carry replacement
      // text; otherwise an ordinary append would briefly clear the sidebar
      // title/preview until the next recovery catalog.
      let header = retain_targeted_catalog_presentation(provider, header, &existing);

      // The header reader intentionally stops before the body, but we still
      // verify the file revision around it. A concurrent append retries this
      // one established path shortly; bounded retries escalate only if the
      // source never stays still long enough for a safe snapshot.
      let confirmed_cursor = match source_cursor(provider, path) {
        Ok(cursor) => cursor,
        Err(_) => {
          retry_catalog_soon = true;
          continue;
        }
      };
      if confirmed_cursor != cursor {
        retry_changed_file_paths.insert(path.clone());
        continue;
      }

      let metadata = catalog_session_metadata(&source_key, header, Some(&existing), true, true)?;
      replacements.push(
        SourceReplacement::new(
          SourceState::new(source_key, pending_body_cursor(&cursor), indexed_at_ms),
          vec![metadata],
        )
        .with_source_cursor_precondition(SourceCursorPrecondition::existing(&previous)),
      );
    }

    if replacements.is_empty() {
      return Ok(ProviderIndexRefresh {
        retry_catalog_soon,
        retry_changed_file_paths,
        ..Default::default()
      });
    }
    match self.session_index.replace_sources(&replacements) {
      Ok(_) => Ok(ProviderIndexRefresh {
        changed: true,
        presentation_changed: false,
        attention_session_keys: Vec::new(),
        retry_catalog_soon,
        retry_changed_file_paths,
      }),
      Err(SessionIndexError::SourceCursorConflict { .. }) => Ok(ProviderIndexRefresh {
        // A sibling viewer committed a newer indexed source. Reread SQLite
        // and use the full topology path before replacing anything else.
        changed: true,
        retry_catalog_soon: true,
        ..Default::default()
      }),
      Err(error) => Err(format!("failed to update the {provider:?} session index: {error}")),
    }
  }

  /// Refreshes only known file-backed source paths reported by the native
  /// watcher. The body queue stays shared and bounded exactly as it does after
  /// a complete catalog pass; this method merely avoids re-enumerating every
  /// rollout to stage work for an active session append.
  pub(crate) fn refresh_changed_file_catalogs(
    &self,
    changed_paths: BTreeMap<ViewerProvider, BTreeSet<PathBuf>>,
  ) -> Result<IndexRefresh, String> {
    for (provider, paths) in &changed_paths {
      for path in paths {
        self.relay.changed_source(*provider, path);
      }
    }
    let providers = ViewerProvider::ALL
      .into_iter()
      .filter(|provider| changed_paths.get(provider).is_some_and(|paths| !paths.is_empty()))
      .collect::<Vec<_>>();
    if providers.is_empty() {
      return Ok(IndexRefresh::default());
    }

    let _refresh_gate = self
      .index_refresh_gate
      .lock()
      .map_err(|_| "session index refresh gate is poisoned".to_string())?;
    self.begin_session_index_catalog_refresh(CatalogRefreshScope::Targeted, providers.len());
    let result = (|| {
      let mut refresh = IndexRefresh::default();
      for provider in providers {
        self.set_catalog_active_provider(provider);
        let provider_refresh = self.refresh_changed_file_provider_catalog(
          provider,
          changed_paths
            .get(&provider)
            .expect("targeted provider should retain its changed paths"),
        )?;
        refresh.changed |= provider_refresh.changed;
        refresh.catalog_refresh_required |= provider_refresh.changed;
        refresh.retry_catalog_soon |= provider_refresh.retry_catalog_soon;
        refresh
          .attention_session_keys
          .extend(provider_refresh.attention_session_keys);
        if !provider_refresh.retry_changed_file_paths.is_empty() {
          refresh
            .retry_changed_file_paths
            .entry(provider)
            .or_default()
            .extend(provider_refresh.retry_changed_file_paths);
        }
        // A targeted header read does not prove that a prior whole-provider
        // catalog failure recovered, so retain its durable warning until the
        // next full discovery succeeds.
        self.finish_catalog_provider(provider, false, false);
      }
      if let Ok(pending_providers) = self.pending_catalog_providers() {
        self.index_progress.update(|progress| {
          progress.catalog.pending_providers = pending_providers;
        });
      }
      let remaining_jobs = self.pending_body_jobs(&HashSet::new())?;
      self.replace_body_progress_after_catalog(&remaining_jobs, refresh.changed)?;
      refresh.has_pending_body_jobs = !remaining_jobs.is_empty();
      let catalog_errors = self
        .catalog_errors
        .lock()
        .map(|errors| errors.clone())
        .unwrap_or_default();
      refresh.has_catalog_errors = !catalog_errors.is_empty();
      self.finalize_index_refresh(
        refresh,
        combined_index_errors(catalog_errors, self.body_errors_for_pending_jobs(&remaining_jobs)),
      )
    })();
    self.finish_session_index_refresh(&result);
    result
  }

  /// Returns the bounded global queue of cataloged sources whose bodies still
  /// need inspection. All jobs are ordered by provider update time first, then
  /// source-file modification time, so recent conversations become fully
  /// actionable before old history.
  fn pending_body_jobs(&self, unavailable: &HashSet<ViewerProvider>) -> Result<Vec<PendingBodyJob>, String> {
    let sessions = self
      .session_index
      .list_unbaselined_present_sessions()
      .map_err(|error| format!("failed to read indexed session metadata: {error}"))?;
    let mut sessions_by_source = HashMap::<SourceKey, Vec<IndexedSession>>::new();
    for session in sessions {
      sessions_by_source
        .entry(session.key.source_key())
        .or_default()
        .push(session);
    }

    let mut jobs = Vec::new();
    let failed_jobs = self
      .failed_body_jobs
      .lock()
      .map(|jobs| jobs.clone())
      .unwrap_or_default();
    for (source_key, source_sessions) in sessions_by_source {
      let Some(provider) = ViewerProvider::ALL
        .into_iter()
        .find(|provider| provider.as_str() == source_key.provider)
      else {
        continue;
      };
      if unavailable.contains(&provider) || self.relay.external_catalog_covers(provider) {
        continue;
      }
      let Some(source) = self
        .session_index
        .source_state(&source_key)
        .map_err(|error| format!("failed to read indexed {provider:?} source: {error}"))?
      else {
        continue;
      };
      let Some(raw_cursor) = pending_body_raw_cursor(&source.cursor).map(str::to_owned) else {
        continue;
      };
      for session in source_sessions {
        let priority = session_body_priority(&session);
        let header = indexed_session_header(&session)?;
        let locator = locator_for_header(provider, &header);
        let deprioritized = failed_jobs
          .get(&(source.key.clone(), locator.session_id.clone()))
          .is_some_and(|failure| failure.raw_cursor == raw_cursor && failure.source_generation == source.generation);
        jobs.push(PendingBodyJob {
          provider,
          source: source.clone(),
          raw_cursor: raw_cursor.clone(),
          locator,
          priority,
          deprioritized,
        });
      }
    }
    jobs.sort_by(|left, right| {
      left
        .deprioritized
        .cmp(&right.deprioritized)
        .then_with(|| right.priority.cmp(&left.priority))
        .then_with(|| left.provider.as_str().cmp(right.provider.as_str()))
        .then_with(|| left.source.key.source_key.cmp(&right.source.key.source_key))
        .then_with(|| left.locator.session_id.cmp(&right.locator.session_id))
    });
    Ok(jobs)
  }

  fn record_failed_body_job(&self, job: &PendingBodyJob, message: String) {
    if let Ok(mut failed_jobs) = self.failed_body_jobs.lock() {
      failed_jobs.insert(
        (job.source.key.clone(), job.locator.session_id.clone()),
        FailedBodyJob {
          raw_cursor: job.raw_cursor.clone(),
          source_generation: job.source.generation,
          message,
        },
      );
    }
  }

  fn clear_failed_body_job(&self, job: &PendingBodyJob) {
    if let Ok(mut failed_jobs) = self.failed_body_jobs.lock() {
      let key = (job.source.key.clone(), job.locator.session_id.clone());
      if failed_jobs.get(&key).is_some_and(|failure| {
        failure.raw_cursor == job.raw_cursor && failure.source_generation == job.source.generation
      }) {
        failed_jobs.remove(&key);
      }
    }
  }

  fn body_errors_for_pending_jobs(&self, jobs: &[PendingBodyJob]) -> HashMap<ViewerProvider, String> {
    let Ok(failed_jobs) = self.failed_body_jobs.lock() else {
      return HashMap::new();
    };
    let mut errors = HashMap::new();
    for job in jobs {
      let key = (job.source.key.clone(), job.locator.session_id.clone());
      if let Some(failure) = failed_jobs
        .get(&key)
        .filter(|failure| failure.raw_cursor == job.raw_cursor && failure.source_generation == job.source.generation)
      {
        errors.entry(job.provider).or_insert_with(|| failure.message.clone());
      }
    }
    errors
  }

  /// A successful body completion advances the source cursor as an optimistic
  /// concurrency generation. Retarget still-queued siblings so a shared
  /// source (notably the OpenCode database) can use the rest of this bounded
  /// batch instead of treating every sibling as stale.
  fn retarget_pending_body_jobs(jobs: &mut [PendingBodyJob], previous_source: &SourceState, next_source: SourceState) {
    let Some(next_raw_cursor) = pending_body_raw_cursor(&next_source.cursor) else {
      return;
    };
    for job in jobs {
      if job.source.key == previous_source.key && job.source == *previous_source && job.raw_cursor == next_raw_cursor {
        job.source = next_source.clone();
      }
    }
  }

  /// Loads and commits one staged session body. Cataloging remains the only
  /// source-inventory writer, so OpenCode's shared database source can backfill
  /// its sessions independently without one job tombstoning its siblings.
  fn refresh_pending_body_job(&self, job: &PendingBodyJob) -> Result<BodyJobRefresh, String> {
    let current_source = self
      .session_index
      .source_state(&job.source.key)
      .map_err(|error| format!("failed to read indexed body job source: {error}"))?;
    if current_source.as_ref() != Some(&job.source) {
      return Ok(BodyJobRefresh::Stale);
    }
    if source_cursor(job.provider, &job.locator.source_path)? != job.raw_cursor {
      return Ok(BodyJobRefresh::Stale);
    }
    match self.repository.session_body_indexing(&job.locator)? {
      SessionBodyIndexing::Deferred => return Ok(BodyJobRefresh::Deferred),
      SessionBodyIndexing::CatalogOnly => return self.complete_catalog_only_body_job(job),
      SessionBodyIndexing::Ready => {}
    }

    let existing_sessions = self
      .session_index
      .list_all_sessions_for_provider(job.provider.as_str())
      .map_err(|error| format!("failed to read indexed body attention: {error}"))?;
    let existing_by_session_key = existing_sessions
      .iter()
      .map(|session| (session.key.clone(), session))
      .collect::<HashMap<_, _>>();
    let existing_by_session_id = existing_sessions
      .iter()
      .filter(|session| session.key.provider == job.provider.as_str())
      .fold(
        HashMap::<String, Vec<&IndexedSession>>::new(),
        |mut grouped, session| {
          grouped.entry(session.key.session_id.clone()).or_default().push(session);
          grouped
        },
      );
    let key = index_session_key(&job.locator)?;
    if key.source_key() != job.source.key {
      return Err("session index body job has a mismatched source path".to_string());
    }
    let Some(existing) = existing_by_session_key.get(&key).copied() else {
      return Ok(BodyJobRefresh::Stale);
    };
    if !existing.present || existing.attention_baselined {
      return Ok(BodyJobRefresh::Stale);
    }
    let activity = self.repository.load_activity(&job.locator)?;
    let presentation = activity.presentation.clone();
    let presentation_changed = existing.title != existing.catalog_title.clone().or(presentation.title.clone())
      || existing.preview != existing.catalog_preview.clone().or(presentation.preview.clone());
    let attention_marker = Some(activity.marker());
    if source_cursor(job.provider, &job.locator.source_path)? != job.raw_cursor {
      return Ok(BodyJobRefresh::Stale);
    }

    // Completion writes attention plus bounded presentation captured from the
    // same body snapshot. Use the committed catalog row for relocation
    // identity, so this bounded job never needs to walk the whole provider
    // header catalog again or overwrite newer relationship metadata.
    let metadata = session_metadata_from_header(
      &job.source.key,
      indexed_session_header(existing)?,
      attention_marker.clone(),
      false,
    )?;
    let current_source_keys = existing_sessions
      .iter()
      .filter(|session| session.present && session.key.provider == job.provider.as_str())
      .map(|session| session.key.source_key.clone())
      .collect::<HashSet<_>>();
    let relocated = relocated_session_reference(
      &job.source.key,
      &metadata,
      &current_source_keys,
      &existing_by_session_id,
    );
    let has_new_attention = body_has_new_attention(existing, relocated, attention_marker.as_deref());
    let mut completion = SessionBaselineCompletionRequest::new(key, attention_marker);
    completion.presentation = Some(presentation);
    completion.has_new_attention = has_new_attention;
    completion.new_attention_count =
      body_new_attention_count(existing, relocated, completion.attention_marker.as_deref());
    completion.retired_attention_count = Activity::from_marker(existing.attention_marker.as_deref())
      .map(|before| before.final_count.saturating_sub(activity.final_count))
      .unwrap_or_default();
    completion.reset_attention = existing
      .attention_marker
      .as_deref()
      .is_some_and(|marker| marker.starts_with("visible-message-count.v1."));
    let next_source = SourceState::new(
      job.source.key.clone(),
      advance_pending_body_cursor(&job.source.cursor)?,
      current_time_ms(),
    );
    let completion = self
      .session_index
      .complete_session_baseline(&job.source, &next_source, completion)
      .map_err(|error| format!("failed to update the {:?} session index: {error}", job.provider))?;
    if completion == tokn_session_index::SessionBaselineCompletion::Stale {
      return Ok(BodyJobRefresh::Stale);
    }
    let next_source = completion
      .committed_source()
      .cloned()
      .expect("an applied session baseline completion must return its source state");
    let attention_session_keys = completion
      .attention_changed()
      .then(|| encode_session_key(&job.locator))
      .transpose()?
      .into_iter()
      .collect();
    Ok(BodyJobRefresh::Updated {
      provider_refresh: ProviderIndexRefresh {
        changed: completion.was_applied(),
        presentation_changed,
        attention_session_keys,
        retry_catalog_soon: false,
        ..Default::default()
      },
      next_source,
    })
  }

  fn complete_catalog_only_body_job(&self, job: &PendingBodyJob) -> Result<BodyJobRefresh, String> {
    let key = index_session_key(&job.locator)?;
    if key.source_key() != job.source.key {
      return Err("session index body job has a mismatched source path".to_string());
    }
    let next_source = SourceState::new(
      job.source.key.clone(),
      advance_pending_body_cursor(&job.source.cursor)?,
      current_time_ms(),
    );
    let existing = self.session_index.session(&key).map_err(|e| e.to_string())?;
    let marker = existing
      .as_ref()
      .filter(|row| !needs_unread_upgrade(row))
      .and_then(|row| Activity::from_marker(row.attention_marker.as_deref()).map(|activity| activity.marker()));
    let mut request = SessionBaselineCompletionRequest::new(key, marker);
    request.reset_attention = existing.as_ref().is_some_and(needs_unread_upgrade);
    let completion = self
      .session_index
      .complete_session_baseline(&job.source, &next_source, request)
      .map_err(|error| format!("failed to update the {:?} session index: {error}", job.provider))?;
    if completion == tokn_session_index::SessionBaselineCompletion::Stale {
      return Ok(BodyJobRefresh::Stale);
    }
    let next_source = completion
      .committed_source()
      .cloned()
      .expect("an applied session baseline completion must return its source state");
    Ok(BodyJobRefresh::Updated {
      provider_refresh: ProviderIndexRefresh {
        changed: completion.was_applied(),
        ..Default::default()
      },
      next_source,
    })
  }

  pub fn load_event_page(&self, request: EventPageRequest) -> Result<EventPage, String> {
    self.check_session_scope(&request.session_key)?;
    if request.window_mode.is_some() {
      return self.load_retained_event_page(request);
    }
    let limit = bounded_limit(request.limit)?;
    let locator = decode_session_key(&request.session_key)?;
    // Capture before parsing the provider source. A later index refresh can
    // only add attention beyond this revision, which a page acknowledgement
    // will deliberately leave unread.
    let attention_revision = self.attention_revision_for_locator(&locator);
    let loaded = if self.relay.covers(locator.provider) && request.cursor.is_none() && request.offset.is_none() {
      self.relay.advance(&locator)?
    } else {
      self.load_verified(&locator)?
    };
    // Preserve the original full-history row API for older clients. Modern
    // viewers explicitly request retained turn windows above.
    let loaded = if self
      .relay
      .window_info(&locator, &loaded)
      .is_some_and(|info| info.has_earlier)
    {
      self.relay.load_earlier(&locator, 0)?
    } else {
      loaded
    };
    let timeline = timeline_entries(&loaded.events);
    let identity = self.window_identity(&locator, &loaded, false)?;
    let total_events = timeline.len();
    let requested = requested_offset(request.cursor.as_deref(), request.offset, decode_event_cursor)?;
    let boundary = requested.unwrap_or(match request.direction {
      PageDirection::Forward => 0,
      PageDirection::Backward => total_events,
    });
    let (start, end) = event_page_bounds(total_events, boundary, request.direction, limit)?;
    let has_targeted_agent_activity = timeline[start..end]
      .iter()
      .any(|entry| timeline_entry_has_targeted_agent_activity(entry, &loaded.events));
    let delegation_targets = has_targeted_agent_activity
      .then(|| self.delegation_targets_for_parent(&locator))
      .unwrap_or_default();
    let intermediate_usage = usage_filter::intermediate_usage(&loaded.events);
    let events = timeline[start..end]
      .iter()
      .map(|entry| {
        identity.summary(timeline_entry_event_summary(
          entry,
          &loaded.events,
          &delegation_targets,
          &intermediate_usage,
        ))
      })
      .collect();

    Ok(EventPage {
      events,
      next_cursor: (end < total_events).then(|| encode_event_cursor(end)),
      previous_cursor: (start > 0).then(|| encode_event_cursor(start)),
      total_events,
      history_status: loaded.history_status.into(),
      follow_error: self.page_follow_error(&locator),
      attention_revision,
      outstanding_questions: outstanding_questions(&loaded.events, &identity),
    })
  }

  /// Advances the seen cursor only through an attention revision the frontend
  /// received in, and accepted for, a successful initial event page.
  pub fn acknowledge_session_attention(
    &self,
    request: AcknowledgeSessionAttentionRequest,
  ) -> Result<AcknowledgeSessionAttentionResponse, String> {
    self.check_session_scope(&request.session_key)?;
    if self.session_scope.is_some() {
      return Ok(AcknowledgeSessionAttentionResponse { changed: false });
    }
    let locator = decode_session_key(&request.session_key)?;
    let attention_revision = request
      .attention_revision
      .parse::<i64>()
      .ok()
      .filter(|revision| *revision >= 0)
      .ok_or_else(|| "invalid attention revision".to_string())?;
    let key = index_session_key(&locator)?;
    let changed = self
      .session_index
      .mark_seen_through(&key, attention_revision, current_time_ms())
      .map_err(|error| format!("failed to acknowledge session attention: {error}"))?;
    Ok(AcknowledgeSessionAttentionResponse { changed })
  }

  /// Returns the ordinary normalized rows represented by one synthetic
  /// trajectory. The opaque trajectory key pins the request to the run's
  /// first source position, while a trajectory-specific cursor prevents a
  /// cursor from one item being replayed against another.
  pub fn load_trajectory_event_page(
    &self,
    request: LoadTrajectoryEventPageRequest,
  ) -> Result<TrajectoryEventPage, String> {
    let limit = bounded_limit(request.limit)?;
    let locator = decode_session_key(&request.session_key)?;
    let loaded = self.load_verified(&locator)?;
    let identity = self.window_identity(&locator, &loaded, request.trajectory_key.starts_with("window.v1."))?;
    let local_key = identity.local_key(&request.trajectory_key)?;
    let start_source_event_index = decode_trajectory_key(&local_key)?;
    let cursor = request
      .cursor
      .as_deref()
      .map(|value| identity.local_key(value))
      .transpose()?;
    let trajectory = trajectory_for_start(&loaded.events, start_source_event_index)
      .ok_or_else(|| "trajectory key is outside the session".to_string())?;
    let total_events = trajectory.entries.len();
    let requested = requested_trajectory_offset(cursor.as_deref(), request.offset, start_source_event_index)?;
    let boundary = requested.unwrap_or(match request.direction {
      PageDirection::Forward => 0,
      PageDirection::Backward => total_events,
    });
    let (start, end) = event_page_bounds(total_events, boundary, request.direction, limit)?;
    let has_targeted_agent_activity = trajectory.entries[start..end]
      .iter()
      .any(|entry| timeline_entry_has_targeted_agent_activity(entry, &loaded.events));
    let delegation_targets = has_targeted_agent_activity
      .then(|| self.delegation_targets_for_parent(&locator))
      .unwrap_or_default();
    let intermediate_usage = usage_filter::intermediate_usage(&loaded.events);
    let events = trajectory.entries[start..end]
      .iter()
      .map(|entry| {
        identity.summary(timeline_entry_event_summary(
          entry,
          &loaded.events,
          &delegation_targets,
          &intermediate_usage,
        ))
      })
      .collect();

    Ok(TrajectoryEventPage {
      events,
      next_cursor: (end < total_events)
        .then(|| identity.key(&encode_trajectory_event_cursor(start_source_event_index, end))),
      previous_cursor: (start > 0)
        .then(|| identity.key(&encode_trajectory_event_cursor(start_source_event_index, start))),
      total_events,
    })
  }

  pub fn load_event_detail(&self, request: LoadEventDetailRequest) -> Result<EventDetail, String> {
    let locator = decode_session_key(&request.session_key)?;
    let loaded = self.load_verified(&locator)?;
    let identity = self.window_identity(&locator, &loaded, request.event_key.starts_with("window.v1."))?;
    let local_key = identity.local_key(&request.event_key)?;
    let (detail, native_envelope) = self.load_event_detail_local(&locator, &loaded, local_key)?;
    Ok(identity.detail(detail, native_envelope))
  }

  fn load_event_detail_local(
    &self,
    locator: &SessionLocator,
    loaded: &Arc<LoadedSession>,
    event_key: String,
  ) -> Result<(EventDetail, bool), String> {
    if event_key.starts_with("trajectory.v1.") {
      let start_source_event_index = decode_trajectory_key(&event_key)?;
      let trajectory = trajectory_for_start(&loaded.events, start_source_event_index)
        .ok_or_else(|| "trajectory key is outside the session".to_string())?;
      let mut detail = trajectory_detail(event_key, &trajectory, &loaded.events)?;
      if let Some(native) = self.relay_native_detail(locator, loaded, &trajectory_source_event_indices(&trajectory))? {
        detail.native = Some(native);
      }
      return Ok((detail, true));
    }

    let source_event_index = decode_event_key(&event_key)?;
    if let Some(operation) = compaction::for_source(&loaded.events, source_event_index) {
      let mut detail = event_detail(
        encode_event_key(source_event_index),
        &AgentEvent::Compaction(operation.event),
        &loaded.events,
        source_event_index,
      )?;
      let mut native_envelope = false;
      if let Some(native) = self.relay_native_detail(locator, loaded, &operation.source_event_indices)? {
        detail.native = Some(native);
        native_envelope = true;
      }
      return Ok((detail, native_envelope));
    }
    let entry = base_timeline_entry_for_source(&loaded.events, source_event_index)
      .ok_or_else(|| "event key is outside the session".to_string())?;

    self.timeline_entry_detail(&locator, &loaded, entry)
  }

  fn timeline_entry_detail(
    &self,
    locator: &SessionLocator,
    loaded: &Arc<LoadedSession>,
    entry: TimelineEntry,
  ) -> Result<(EventDetail, bool), String> {
    match entry {
      TimelineEntry::Event { source_event_index } => {
        let event = &loaded.events[source_event_index];
        let mut detail = event_detail(
          encode_event_key(source_event_index),
          event,
          &loaded.events,
          source_event_index,
        )?;
        if self.relay.covers(locator.provider)
          && !event.is_hidden()
          && !matches!(event, AgentEvent::Reasoning(reasoning) if reasoning.redacted == Some(true))
        {
          if let Some(native) = self.relay.native(locator, source_event_index, loaded) {
            detail.native = Some(bounded_detail_value(native, "native")?);
          }
        }
        Ok((detail, false))
      }
      TimelineEntry::ToolOperation {
        source_event_index,
        operation,
      } => {
        let mut detail = tool_operation_detail(encode_event_key(source_event_index), &operation, &loaded.events)?;
        if let Some(native) = self.relay_native_detail(locator, loaded, &operation.source_event_indices)? {
          detail.native = Some(native);
        }
        Ok((detail, true))
      }
      TimelineEntry::Trajectory { trajectory } => {
        let mut detail = trajectory_detail(
          encode_trajectory_key(trajectory.start_source_event_index),
          &trajectory,
          &loaded.events,
        )?;
        if let Some(native) =
          self.relay_native_detail(locator, loaded, &trajectory_source_event_indices(&trajectory))?
        {
          detail.native = Some(native);
        }
        Ok((detail, true))
      }
    }
  }

  fn relay_native_detail(
    &self,
    locator: &SessionLocator,
    loaded: &Arc<LoadedSession>,
    indices: &[usize],
  ) -> Result<Option<Value>, String> {
    if !self.relay.covers(locator.provider) {
      return Ok(None);
    }
    let mut records = Vec::new();
    let mut bytes: usize = 0;
    let mut truncated = indices.len() > MAX_TRAJECTORY_DETAIL_SOURCE_RECORDS;
    for &index in indices.iter().take(MAX_TRAJECTORY_DETAIL_SOURCE_RECORDS) {
      let Some(native) = self.relay.native(locator, index, loaded) else {
        continue;
      };
      let record = bounded_detail_value_with_limit(
        json!({ "event_key": encode_event_key(index), "native": native }),
        "relay_native_record",
        MAX_TRAJECTORY_DETAIL_SOURCE_RECORD_BYTES,
      )?;
      bytes = bytes.saturating_add(serialized_value_size(&record, "relay native record")?);
      if bytes > trajectory_detail_record_budget() {
        truncated = true;
        break;
      }
      records.push(record);
    }
    if records.is_empty() {
      return Ok(None);
    }
    bounded_detail_value(
      json!({ "source_event_count": indices.len(), "source_records": records, "truncated": truncated }),
      "relay_native_records",
    )
    .map(Some)
  }

  fn load_verified(&self, locator: &SessionLocator) -> Result<Arc<LoadedSession>, String> {
    self.check_session_scope(&encode_session_key(locator)?)?;
    if self.relay.covers(locator.provider) {
      return self.relay.load(locator);
    }
    {
      let cache = self
        .loaded_session_cache
        .lock()
        .map_err(|_| "loaded session cache lock is poisoned".to_string())?;
      if let Some(cached) = cache
        .as_ref()
        .filter(|cached| cached.locator == *locator && cached.source_revision.is_current())
      {
        return Ok(Arc::clone(&cached.loaded));
      }
    }

    let revision_before = source_revision(locator);
    let loaded = Arc::new(self.repository.load_session(locator)?);
    if loaded.reference.id != locator.session_id {
      return Err("session key no longer matches its source record".to_string());
    }

    // Routing changes also change the owning file revision. A second lineage
    // discovery adds no protection once every resolved dependency is checked.
    let stable_revision = revision_before.filter(SourceRevision::is_current);
    let mut cache = self
      .loaded_session_cache
      .lock()
      .map_err(|_| "loaded session cache lock is poisoned".to_string())?;
    *cache = match stable_revision {
      Some(revision) => Some(CachedSession {
        locator: locator.clone(),
        source_revision: revision,
        loaded: Arc::clone(&loaded),
        retained_start: None,
      }),
      _ => None,
    };
    Ok(loaded)
  }
}

fn event_detail(
  event_key: String,
  event: &AgentEvent,
  events: &[AgentEvent],
  source_event_index: usize,
) -> Result<EventDetail, String> {
  let mut detail = source_event_detail(event_key, event)?;
  if !detail.is_hidden && detail.event.get("redacted") != Some(&Value::Bool(true)) {
    detail.tool_output = tool_output_preview(events, source_event_index);
  }
  Ok(detail)
}

fn source_event_detail(event_key: String, event: &AgentEvent) -> Result<EventDetail, String> {
  let is_hidden = event.is_hidden();
  if is_hidden {
    return Ok(EventDetail {
      event_key,
      event: json!({
        "type": normalized_event_type(event),
        "provider": provider_for_event(event).as_str(),
        "redacted": true,
      }),
      native: None,
      is_hidden: true,
      tool_output: None,
    });
  }
  if matches!(event, AgentEvent::Reasoning(reasoning) if reasoning.redacted == Some(true)) {
    return Ok(EventDetail {
      event_key,
      event: json!({
        "type": "reasoning",
        "provider": provider_for_event(event).as_str(),
        "redacted": true,
      }),
      native: None,
      is_hidden: false,
      tool_output: None,
    });
  }

  let native = native_detail(event)
    .map(|value| bounded_detail_value(value, "provider_native"))
    .transpose()?;
  let mut normalized =
    serde_json::to_value(event).map_err(|error| format!("failed to serialize normalized event: {error}"))?;
  remove_embedded_native(&mut normalized);
  let normalized = bounded_detail_value(normalized, "normalized_event")?;
  Ok(EventDetail {
    event_key,
    event: normalized,
    native,
    is_hidden: false,
    tool_output: None,
  })
}

fn tool_operation_detail(
  event_key: String,
  operation: &ToolOperation,
  events: &[AgentEvent],
) -> Result<EventDetail, String> {
  let mut normalized = serde_json::to_value(operation)
    .map_err(|error| format!("failed to serialize normalized tool operation: {error}"))?;
  remove_embedded_native(&mut normalized);
  let normalized = bounded_detail_value(normalized, "normalized_tool_operation")?;
  let native = tool_operation_native_detail(operation, events)
    .map(|value| bounded_detail_value(value, "provider_native"))
    .transpose()?;
  let tool_output = tool_operation_output_preview(operation);

  Ok(EventDetail {
    event_key,
    event: normalized,
    native,
    is_hidden: false,
    tool_output,
  })
}

/// Builds one bounded inspector payload over all raw source records represented
/// by a synthetic trajectory. Source rows retain their own ordinary event keys
/// so this aggregate never becomes a second identity layer for those records.
fn trajectory_detail(event_key: String, trajectory: &Trajectory, events: &[AgentEvent]) -> Result<EventDetail, String> {
  let source_event_indices = trajectory_source_event_indices(trajectory);
  let source_event_count = source_event_indices.len();
  let mut normalized_records = Vec::new();
  let mut native_records = Vec::new();
  let mut normalized_size_bytes: usize = 0;
  let mut native_size_bytes: usize = 0;
  let mut normalized_truncated = false;
  let mut native_truncated = false;

  for (position, source_event_index) in source_event_indices.iter().copied().enumerate() {
    if position >= MAX_TRAJECTORY_DETAIL_SOURCE_RECORDS {
      normalized_truncated = true;
      native_truncated = true;
      break;
    }
    let Some(event) = events.get(source_event_index) else {
      normalized_truncated = true;
      native_truncated = true;
      continue;
    };
    let detail = event_detail(encode_event_key(source_event_index), event, events, source_event_index)?;
    let normalized_record = bounded_detail_value_with_limit(
      json!({
        "event_key": detail.event_key,
        "event": detail.event,
        "is_hidden": detail.is_hidden,
      }),
      "trajectory_normalized_source_record",
      MAX_TRAJECTORY_DETAIL_SOURCE_RECORD_BYTES,
    )?;
    let normalized_record_size = serialized_value_size(&normalized_record, "trajectory normalized source record")?;
    if normalized_size_bytes.saturating_add(normalized_record_size) <= trajectory_detail_record_budget() {
      normalized_size_bytes = normalized_size_bytes.saturating_add(normalized_record_size);
      normalized_records.push(normalized_record);
    } else {
      normalized_truncated = true;
    }

    // Direct ToolCall details intentionally aggregate through a logical tool
    // operation and therefore omit native payloads. A trajectory inspector is
    // explicitly a source-record view, so retain that raw tool record here.
    let native = detail.native.or_else(|| match event {
      AgentEvent::ToolCall(event) => event.native.clone(),
      _ => None,
    });
    let Some(native) = native else {
      continue;
    };
    let native_record = bounded_detail_value_with_limit(
      json!({
        "event_key": encode_event_key(source_event_index),
        "native": native,
      }),
      "trajectory_native_source_record",
      MAX_TRAJECTORY_DETAIL_SOURCE_RECORD_BYTES,
    )?;
    let native_record_size = serialized_value_size(&native_record, "trajectory native source record")?;
    if native_size_bytes.saturating_add(native_record_size) <= trajectory_detail_record_budget() {
      native_size_bytes = native_size_bytes.saturating_add(native_record_size);
      native_records.push(native_record);
    } else {
      native_truncated = true;
    }
  }

  let card = serde_json::to_value(trajectory_card_summary(trajectory, events))
    .map_err(|error| format!("failed to serialize trajectory summary: {error}"))?;
  let normalized = bounded_detail_value(
    json!({
      "type": "trajectory",
      "anchor_event_key": encode_event_key(trajectory.start_source_event_index),
      "summary": card,
      "source_event_count": source_event_count,
      "source_records": normalized_records,
      "truncated": normalized_truncated,
    }),
    "trajectory_normalized_records",
  )?;
  let native = (!native_records.is_empty() || native_truncated).then(|| {
    bounded_detail_value(
      json!({
        "source_event_count": source_event_count,
        "source_records": native_records,
        "truncated": native_truncated,
      }),
      "trajectory_native_records",
    )
  });
  let native = native.transpose()?;

  Ok(EventDetail {
    event_key,
    event: normalized,
    native,
    is_hidden: false,
    tool_output: None,
  })
}

fn trajectory_detail_record_budget() -> usize {
  // Leave room for aggregate framing and a final bounded-detail fallback.
  MAX_DETAIL_VALUE_BYTES.saturating_sub(8 * 1024)
}

fn serialized_value_size(value: &Value, representation: &'static str) -> Result<usize, String> {
  serde_json::to_vec(value)
    .map(|value| value.len())
    .map_err(|error| format!("failed to size {representation}: {error}"))
}

fn tool_operation_native_detail(operation: &ToolOperation, events: &[AgentEvent]) -> Option<Value> {
  let records = operation
    .source_event_indices
    .iter()
    .filter_map(|&source_event_index| {
      let AgentEvent::ToolCall(event) = events.get(source_event_index)? else {
        return None;
      };
      event.native.as_ref().map(|native| {
        json!({
          "event_key": encode_event_key(source_event_index),
          "record_kind": serialized_label(event.record_kind),
          "timestamp": event.timestamp,
          "native": native,
        })
      })
    })
    .collect::<Vec<_>>();
  (!records.is_empty()).then(|| json!({ "source_records": records }))
}

fn tool_operation_output_preview(operation: &ToolOperation) -> Option<ToolOutputPreview> {
  let output = operation.output.as_ref().filter(|value| !value.is_null())?;
  let sections = project_output(output, 0);
  let source_event_index = *operation.source_event_indices.first()?;
  (!sections.is_empty()).then(|| bound_tool_output(sections, source_event_index))
}

pub(crate) fn index_catalog_source_key(provider: ViewerProvider) -> SourceKey {
  SourceKey::new(provider.as_str(), INDEX_CATALOG_SOURCE_KEY)
}

fn source_cursor_precondition(previous: Option<&SourceState>) -> SourceCursorPrecondition {
  previous
    .map(SourceCursorPrecondition::existing)
    .unwrap_or_else(SourceCursorPrecondition::missing)
}

/// Returns the provider revision embedded in an index cursor. Pre-staging
/// index versions stored the raw value directly and are treated as complete so
/// they upgrade lazily on the next catalog pass.
fn indexed_source_raw_cursor(cursor: &str) -> Option<&str> {
  if cursor == INDEX_MISSING_SOURCE_CURSOR {
    return None;
  }
  staged_body_cursor_parts(cursor, INDEX_PENDING_BODY_CURSOR_PREFIX)
    .map(|(_, raw_cursor)| raw_cursor)
    .or_else(|| staged_body_cursor_parts(cursor, INDEX_COMPLETED_BODY_CURSOR_PREFIX).map(|(_, raw_cursor)| raw_cursor))
    .or_else(|| cursor.strip_prefix(LEGACY_PENDING_BODY_CURSOR_PREFIX))
    .or_else(|| cursor.strip_prefix(LEGACY_COMPLETED_BODY_CURSOR_PREFIX))
    .or(Some(cursor))
}

fn pending_body_raw_cursor(cursor: &str) -> Option<&str> {
  staged_body_cursor_parts(cursor, INDEX_PENDING_BODY_CURSOR_PREFIX)
    .map(|(_, raw_cursor)| raw_cursor)
    .or_else(|| cursor.strip_prefix(LEGACY_PENDING_BODY_CURSOR_PREFIX))
}

fn pending_body_cursor(raw_cursor: &str) -> String {
  format!("{INDEX_PENDING_BODY_CURSOR_PREFIX}0.{raw_cursor}")
}

fn completed_body_cursor(raw_cursor: &str) -> String {
  format!("{INDEX_COMPLETED_BODY_CURSOR_PREFIX}0.{raw_cursor}")
}

/// Bumps the opaque staged cursor each time one session body finishes. The
/// source-level compare-and-swap then prevents a stale catalog replacement or
/// a second viewer from restoring the older pending attention state.
fn advance_pending_body_cursor(cursor: &str) -> Result<String, String> {
  let (generation, raw_cursor) = staged_body_cursor_parts(cursor, INDEX_PENDING_BODY_CURSOR_PREFIX)
    .or_else(|| {
      cursor
        .strip_prefix(LEGACY_PENDING_BODY_CURSOR_PREFIX)
        .map(|raw_cursor| (0, raw_cursor))
    })
    .ok_or_else(|| "session body job does not have a staged cursor".to_string())?;
  let generation = generation
    .checked_add(1)
    .ok_or_else(|| "session body cursor generation overflowed".to_string())?;
  Ok(format!("{INDEX_PENDING_BODY_CURSOR_PREFIX}{generation}.{raw_cursor}"))
}

fn staged_body_cursor_parts<'a>(cursor: &'a str, prefix: &str) -> Option<(u64, &'a str)> {
  let remainder = cursor.strip_prefix(prefix)?;
  let (generation, raw_cursor) = remainder.split_once('.')?;
  (!raw_cursor.is_empty())
    .then(|| {
      generation
        .parse::<u64>()
        .ok()
        .map(|generation| (generation, raw_cursor))
    })
    .flatten()
}

pub(crate) fn index_source_key_for_path(provider: ViewerProvider, path: &Path) -> Result<SourceKey, String> {
  let source_path = index_path_string(path)?;
  Ok(SourceKey::new(provider.as_str(), format!("path.v1.{source_path}")))
}

fn is_targeted_file_path(path: &Path) -> bool {
  path
    .extension()
    .and_then(|extension| extension.to_str())
    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

fn index_session_key(locator: &SessionLocator) -> Result<IndexedSessionKey, String> {
  let source_key = index_source_key_for_path(locator.provider, &locator.source_path)?;
  Ok(IndexedSessionKey::new(
    source_key.provider,
    source_key.source_key,
    locator.session_id.clone(),
  ))
}

fn index_path_string(path: &Path) -> Result<String, String> {
  let value = path
    .to_str()
    .filter(|value| !value.is_empty())
    .ok_or_else(|| "session path is not valid UTF-8".to_string())?;
  Ok(value.to_owned())
}

fn session_body_priority(session: &IndexedSession) -> i64 {
  session
    .updated_at_ms
    .or_else(|| file_modified_at_ms(Path::new(&session.source_path)))
    .unwrap_or(i64::MIN)
}

fn session_catalog_topology(
  provider: ViewerProvider,
  headers: &[SessionHeader],
) -> Result<SessionCatalogTopology, String> {
  let mut topology = SessionCatalogTopology::new();
  for header in headers {
    let source_key = index_source_key_for_path(provider, &header.path)?;
    topology
      .entry(source_key.source_key)
      .or_default()
      .insert(header.id.clone());
  }
  Ok(topology)
}

pub(crate) fn indexed_session_header(session: &IndexedSession) -> Result<SessionHeader, String> {
  if session.source_path.is_empty() {
    return Err("indexed session has no source path".to_string());
  }
  Ok(SessionHeader {
    id: session.key.session_id.clone(),
    parent_session_id: session.parent_session_id.clone(),
    agent_path: session.agent_path.clone(),
    agent_nickname: session.agent_nickname.clone(),
    agent_role: session.agent_role.clone(),
    title: session.title.clone(),
    preview: session.preview.clone(),
    path: PathBuf::from(&session.source_path),
    cwd: session.cwd.clone(),
    timestamp: session.timestamp.clone(),
    updated_at: session.updated_at.clone(),
    updated_at_ms: session.updated_at_ms,
  })
}

fn session_metadata_from_header(
  source_key: &SourceKey,
  header: SessionHeader,
  attention_marker: Option<String>,
  has_new_attention: bool,
) -> Result<SessionMetadata, String> {
  let source_path = index_path_string(&header.path)?;
  let mut metadata = SessionMetadata::new(
    IndexedSessionKey::new(
      source_key.provider.clone(),
      source_key.source_key.clone(),
      header.id.clone(),
    ),
    source_path,
  );
  // Presentation fields are bounded here; session/parent IDs and agent paths
  // retain exact identity for relation matching. No event, reasoning, native
  // payload, tool input, or tool output enters this record.
  metadata.title = normalize_session_text(header.title, MAX_SESSION_TITLE_CHARS);
  metadata.preview = normalize_session_text(header.preview, MAX_SESSION_PREVIEW_CHARS);
  metadata.cwd = normalize_session_text(header.cwd, MAX_INDEXED_CWD_CHARS);
  metadata.timestamp = normalize_session_text(header.timestamp, MAX_INDEXED_TIMESTAMP_CHARS);
  metadata.updated_at = normalize_session_text(header.updated_at, MAX_INDEXED_TIMESTAMP_CHARS);
  metadata.updated_at_ms = header.updated_at_ms;
  metadata.parent_session_id = header.parent_session_id;
  metadata.agent_path = header.agent_path;
  metadata.agent_nickname = normalize_session_text(header.agent_nickname, MAX_AGENT_IDENTITY_CHARS);
  metadata.agent_role = normalize_session_text(header.agent_role, MAX_AGENT_IDENTITY_CHARS);
  metadata.attention_marker = attention_marker;
  metadata.has_new_attention = has_new_attention;
  Ok(metadata)
}

/// Builds a compact catalog row while preserving the prior body-derived
/// notification state. Presentation remains catalog-owned here; the index
/// separately retains any completed body fallback so a blank header does not
/// erase it or turn it into a stale catalog title. A changed source becomes
/// pending again, but a brand-new row only requests notification after a
/// completed provider catalog exists.
fn catalog_session_metadata(
  source_key: &SourceKey,
  header: SessionHeader,
  existing: Option<&IndexedSession>,
  provider_ready: bool,
  source_changed: bool,
) -> Result<SessionMetadata, String> {
  let attention_marker = existing.and_then(|indexed| {
    if source_changed && indexed.attention_baselined {
      known_attention_marker(indexed)
    } else {
      indexed.attention_marker.clone()
    }
  });
  let mut metadata = session_metadata_from_header(source_key, header, attention_marker, false)?;
  match existing {
    Some(indexed) if source_changed || needs_activity_upgrade(indexed) => {
      metadata.attention_baselined = false;
      // A pending row that was already new must remain eligible when its body
      // eventually succeeds. A previously completed row instead compares its
      // retained count on the later body pass.
      metadata.notify_on_baseline = if indexed.attention_baselined {
        false
      } else {
        indexed.notify_on_baseline
      };
    }
    Some(indexed) => {
      metadata.attention_baselined = indexed.attention_baselined;
      metadata.notify_on_baseline = indexed.notify_on_baseline;
    }
    None => {
      metadata.attention_baselined = false;
      metadata.notify_on_baseline = provider_ready;
    }
  }
  Ok(metadata)
}

/// Targeted file reads are intentionally cheaper than a complete provider
/// catalog. In particular, Codex's direct header reader skips optional Desktop
/// state and legacy-index metadata, so an absent field means "not read here",
/// not "the provider cleared it". Preserve only the prior catalog fields in
/// that case; completed body fallbacks remain independent and continue to be
/// managed by the index store.
fn retain_targeted_catalog_presentation(
  provider: ViewerProvider,
  mut header: SessionHeader,
  existing: &IndexedSession,
) -> SessionHeader {
  if provider != ViewerProvider::Codex {
    return header;
  }
  if header.title.is_none() {
    header.title = existing.catalog_title.clone();
  }
  if header.preview.is_none() {
    header.preview = existing.catalog_preview.clone();
  }
  header
}

/// True only when the index already represents exactly this source's complete
/// header inventory. Body-derived fields are compared too, so a deferred row
/// remains eligible for a later body job even if its sidebar metadata itself
/// has not changed.
fn indexed_source_matches_catalog(
  sessions: &[SessionMetadata],
  existing_by_session_key: &HashMap<IndexedSessionKey, &IndexedSession>,
  existing_present_count: usize,
) -> bool {
  existing_present_count == sessions.len()
    && sessions.iter().all(|metadata| {
      existing_by_session_key
        .get(&metadata.key)
        .is_some_and(|indexed| indexed.present && indexed_session_matches_metadata(indexed, metadata))
    })
}

fn indexed_session_matches_metadata(indexed: &IndexedSession, metadata: &SessionMetadata) -> bool {
  indexed.key == metadata.key
    && indexed.source_path == metadata.source_path
    && indexed.catalog_title == metadata.title
    && indexed.catalog_preview == metadata.preview
    && indexed.cwd == metadata.cwd
    && indexed.timestamp == metadata.timestamp
    && indexed.updated_at == metadata.updated_at
    && indexed.updated_at_ms == metadata.updated_at_ms
    && indexed.parent_session_id == metadata.parent_session_id
    && indexed.agent_path == metadata.agent_path
    && indexed.agent_nickname == metadata.agent_nickname
    && indexed.agent_role == metadata.agent_role
    && indexed.attention_marker == metadata.attention_marker
    && indexed.attention_baselined == metadata.attention_baselined
    && indexed.notify_on_baseline == metadata.notify_on_baseline
}

/// Finds one retired source row that can safely carry attention state across a
/// session-file move. A provider/session ID alone is not enough: the previous
/// source must be absent from the current catalog, optional stable identity
/// fields must agree, and the candidate must be unambiguous.
fn relocated_session_reference<'a>(
  source_key: &SourceKey,
  metadata: &SessionMetadata,
  current_source_keys: &HashSet<String>,
  existing_by_session_id: &HashMap<String, Vec<&'a IndexedSession>>,
) -> Option<&'a IndexedSession> {
  let mut candidates = existing_by_session_id
    .get(&metadata.key.session_id)?
    .iter()
    .copied()
    .filter(|indexed| {
      indexed.key.provider == source_key.provider
        && indexed.key.source_key != source_key.source_key
        && !current_source_keys.contains(&indexed.key.source_key)
        && same_optional_identity(indexed.timestamp.as_deref(), metadata.timestamp.as_deref())
        && same_optional_identity(
          indexed.parent_session_id.as_deref(),
          metadata.parent_session_id.as_deref(),
        )
    });
  let candidate = candidates.next()?;
  candidates.next().is_none().then_some(candidate)
}

fn same_optional_identity(left: Option<&str>, right: Option<&str>) -> bool {
  match (present_string(left), present_string(right)) {
    (Some(left), Some(right)) => left == right,
    _ => true,
  }
}

fn needs_activity_upgrade(session: &IndexedSession) -> bool {
  needs_unread_upgrade(session)
    || session
      .attention_marker
      .as_deref()
      .is_some_and(|marker| marker.starts_with("final-replies.v2."))
}

fn needs_unread_upgrade(session: &IndexedSession) -> bool {
  session
    .attention_marker
    .as_deref()
    .is_some_and(|marker| marker.starts_with("visible-message-count.v1."))
}

fn known_attention_marker(indexed: &IndexedSession) -> Option<String> {
  indexed.attention_marker.clone()
}

fn body_new_attention_count(
  existing: &IndexedSession,
  relocated: Option<&IndexedSession>,
  marker: Option<&str>,
) -> u64 {
  if let Some(relocated) = relocated {
    if existing.attention_revision != 0 && existing.attention_marker == known_attention_marker(relocated) {
      return new_final_count(existing, marker);
    }
    return (relocated.attention_revision - relocated.seen_attention_revision) as u64
      + new_final_count(relocated, marker);
  }
  new_final_count(existing, marker)
}

fn new_final_count(existing: &IndexedSession, marker: Option<&str>) -> u64 {
  let Some(activity) = Activity::from_marker(marker) else {
    return 0;
  };
  match Activity::from_marker(existing.attention_marker.as_deref()) {
    Some(before) => activity.final_count.saturating_sub(before.final_count),
    None if existing.notify_on_baseline && existing.attention_marker.is_none() => activity.final_count,
    // First observation and older accounting formats establish a quiet baseline.
    None => 0,
  }
}

fn body_has_new_attention(existing: &IndexedSession, relocated: Option<&IndexedSession>, marker: Option<&str>) -> bool {
  body_new_attention_count(existing, relocated, marker) > 0
}

#[cfg(test)]
fn has_new_visible_attention(existing: Option<&IndexedSession>, marker: Option<&str>) -> bool {
  existing.is_some_and(|existing| new_final_count(existing, marker) > 0)
}

impl SessionAttention {
  fn from_index(session: &IndexedSession) -> Self {
    let activity = Activity::from_marker(session.attention_marker.as_deref());
    let count = if activity.is_some() {
      (session.attention_revision - session.seen_attention_revision) as u64
    } else {
      u64::from(session.has_unread())
    };
    Self {
      has_unread: count > 0,
      unread_final_count: count,
      is_running: activity.as_ref().is_some_and(|activity| activity.running),
      question_attention: activity.map(|activity| activity.question_attention).unwrap_or_default(),
      ..Default::default()
    }
  }
}

#[cfg(test)]
thread_local! {
  static HISTORY_DEPENDENCY_DISCOVERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn source_revision(locator: &SessionLocator) -> Option<SourceRevision> {
  let mut revision = source_revision_for(locator.provider, &locator.source_path)?;
  if locator.provider == ViewerProvider::Codex {
    // Local snapshots include immutable prefixes referenced by a resumed
    // rollout. A changed or unavailable prefix must not hit the head-only
    // cache and silently retain a transcript from a different source state.
    let source = tokn_session_codex::CodexSessionSource::new(None);
    #[cfg(test)]
    HISTORY_DEPENDENCY_DISCOVERIES.with(|count| count.set(count.get() + 1));
    for segment in source.history_segments(&locator.source_path).ok()? {
      if segment.path != locator.source_path {
        revision.files.push(Some(file_revision(&segment.path)?));
        revision.paths.push(segment.path);
      }
    }
  }
  Some(revision)
}

fn source_revision_for(provider: ViewerProvider, source_path: &Path) -> Option<SourceRevision> {
  let mut paths = vec![source_path.to_path_buf()];
  if matches!(provider, ViewerProvider::OpenCode | ViewerProvider::ZCode) {
    // The SHM sidecar is a reader-writable WAL index. It can change during our
    // own reads without any session content changing, so only track the WAL.
    let mut wal = source_path.as_os_str().to_os_string();
    wal.push("-wal");
    paths.push(wal.into());
  }

  let primary = file_revision(&paths[0])?;
  let mut files = vec![Some(primary)];
  files.extend(paths[1..].iter().map(|path| file_revision(path)));
  Some(SourceRevision { paths, files })
}

fn source_cursor(provider: ViewerProvider, source_path: &Path) -> Result<String, String> {
  let revision = source_revision_for(provider, source_path)
    .ok_or_else(|| "session source is unavailable while indexing".to_string())?;
  let files = revision
    .files
    .iter()
    .map(file_revision_cursor)
    .collect::<Vec<_>>()
    .join("|");
  Ok(format!("source-revision.v1.{files}"))
}

fn file_revision_cursor(revision: &Option<FileRevision>) -> String {
  let Some(revision) = revision else {
    return "missing".to_string();
  };
  format!(
    "{}:{}:{}",
    revision.len,
    system_time_cursor(revision.modified),
    system_time_cursor(revision.created),
  )
}

fn system_time_cursor(time: Option<SystemTime>) -> String {
  let Some(time) = time else {
    return "unknown".to_string();
  };
  match time.duration_since(UNIX_EPOCH) {
    Ok(duration) => format!("after-{}-{}", duration.as_secs(), duration.subsec_nanos()),
    Err(error) => {
      let duration = error.duration();
      format!("before-{}-{}", duration.as_secs(), duration.subsec_nanos())
    }
  }
}

fn current_time_ms() -> i64 {
  let milliseconds = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis();
  i64::try_from(milliseconds).unwrap_or(i64::MAX)
}

fn file_revision(path: &Path) -> Option<FileRevision> {
  let metadata = std::fs::metadata(path).ok()?;
  Some(FileRevision {
    len: metadata.len(),
    modified: metadata.modified().ok(),
    created: metadata.created().ok(),
  })
}

fn file_modified_at_ms(path: &Path) -> Option<i64> {
  let duration = std::fs::metadata(path)
    .ok()?
    .modified()
    .ok()?
    .duration_since(UNIX_EPOCH)
    .ok()?;
  i64::try_from(duration.as_millis()).ok()
}

fn selected_providers(requested: &[ViewerProvider]) -> Vec<ViewerProvider> {
  if requested.is_empty() {
    return ViewerProvider::ALL.to_vec();
  }
  ViewerProvider::ALL
    .into_iter()
    .filter(|provider| requested.contains(provider))
    .collect()
}

fn record_source_error(errors: &mut Vec<SourceError>, provider: ViewerProvider, message: String) {
  if errors.iter().all(|error| error.provider != provider) {
    errors.push(SourceError { provider, message });
  }
}

/// Catalog failures take precedence because a direct historical body read can
/// still succeed while current header discovery is unavailable. Body failures
/// disappear as soon as no pending job still carries that source revision.
fn combined_index_errors(
  catalog_errors: HashMap<ViewerProvider, String>,
  body_errors: HashMap<ViewerProvider, String>,
) -> HashMap<ViewerProvider, String> {
  let mut errors = body_errors;
  errors.extend(catalog_errors);
  errors
}

fn validate_session_header(provider: ViewerProvider, header: &SessionHeader) -> Result<(), String> {
  encode_session_key(&locator_for_header(provider, header)).map(|_| ())
}

fn session_relation_index(
  provider: ViewerProvider,
  headers: Vec<SessionHeader>,
  source_errors: &mut Vec<SourceError>,
) -> SessionRelationIndex {
  let mut valid_headers = Vec::with_capacity(headers.len());
  for header in headers {
    if let Err(message) = validate_session_header(provider, &header) {
      record_source_error(source_errors, provider, message);
      continue;
    }
    valid_headers.push(header);
  }
  let headers = canonical_session_headers(valid_headers);
  let indices_by_id = headers
    .iter()
    .enumerate()
    .map(|(index, header)| (header.id.as_str(), index))
    .collect::<HashMap<_, _>>();
  let mut parent_indices = vec![None; headers.len()];

  for (child_index, header) in headers.iter().enumerate() {
    let Some(parent_id) = header.parent_session_id.as_deref() else {
      continue;
    };
    let Some(&parent_index) = indices_by_id.get(parent_id) else {
      continue;
    };
    if parent_index == child_index || relation_would_cycle(&parent_indices, child_index, parent_index) {
      continue;
    }
    parent_indices[child_index] = Some(parent_index);
  }

  let mut child_counts = vec![0; headers.len()];
  for parent_index in parent_indices.iter().flatten() {
    child_counts[*parent_index] += 1;
  }

  SessionRelationIndex {
    headers,
    parent_indices,
    child_counts,
  }
}

/// Overlay direct unread state from the indexed rows on the existing canonical
/// relation graph. Keeping this calculation here preserves the viewer's
/// duplicate-ID, orphan, and cycle policy instead of asking SQLite to infer a
/// potentially different recursive tree.
fn session_relation_attention(
  provider: ViewerProvider,
  relations: &SessionRelationIndex,
  direct_attention: &HashMap<SessionLocator, SessionAttention>,
) -> Vec<SessionAttention> {
  let mut attention = relations
    .headers
    .iter()
    .map(|header| {
      direct_attention
        .get(&locator_for_header(provider, header))
        .copied()
        .unwrap_or_default()
    })
    .collect::<Vec<_>>();
  for child_index in 0..attention.len() {
    let child = attention[child_index];
    let mut parent_index = relations.parent_indices[child_index];
    while let Some(index) = parent_index {
      attention[index].has_running_descendant |= child.is_running;
      parent_index = relations.parent_indices[index];
    }
  }
  attention
}

fn canonical_session_headers(mut headers: Vec<SessionHeader>) -> Vec<SessionHeader> {
  // Match the client tree loader's policy: when a provider has more than one
  // header with the same ID, the newest provider timestamp owns that
  // provider-local identity. Filesystem mtime is deliberately not involved:
  // it can change long after the provider wrote the rollout. The source path
  // remains part of the opaque key, but cannot resolve an ambiguous parent ID
  // on its own.
  headers.sort_by(|left, right| {
    right
      .timestamp
      .cmp(&left.timestamp)
      .then_with(|| right.path.cmp(&left.path))
  });
  let mut canonical_ids = HashSet::new();
  headers.retain(|header| canonical_ids.insert(header.id.clone()));
  headers.sort_by(compare_session_headers);
  headers
}

fn relation_would_cycle(parent_indices: &[Option<usize>], child_index: usize, parent_index: usize) -> bool {
  let mut current = Some(parent_index);
  while let Some(index) = current {
    if index == child_index {
      return true;
    }
    current = parent_indices[index];
  }
  false
}

fn project_key(header: &SessionHeader) -> Option<&str> {
  header.cwd.as_deref().map(str::trim).filter(|key| !key.is_empty())
}

fn apply_project_group(summary: &mut SessionSummary, projects: &HashMap<String, tokn_session_index::ProjectGroup>) {
  if let Some(group) = summary.cwd.as_deref().and_then(|cwd| projects.get(cwd.trim())) {
    summary.project_key = Some(group.project_key.clone());
    summary.project = path_name(&group.project_key).map(str::to_owned);
    summary.project_order_ms = Some(group.order_ms);
  }
}

fn sort_root_candidates(
  candidates: &mut [SessionListCandidate],
  order: SessionOrder,
  projects: &HashMap<String, tokn_session_index::ProjectGroup>,
) {
  if order == SessionOrder::Time {
    sort_session_candidates(candidates);
    return;
  }
  candidates.sort_by(|left, right| {
    let left_group = project_key(&left.header).and_then(|key| projects.get(key));
    let right_group = project_key(&right.header).and_then(|key| projects.get(key));
    let left_key = left_group.map(|group| group.project_key.as_str());
    let right_key = right_group.map(|group| group.project_key.as_str());
    right_group
      .map(|group| group.order_ms)
      .cmp(&left_group.map(|group| group.order_ms))
      .then_with(|| left_key.cmp(&right_key))
      .then_with(|| compare_session_headers(&left.header, &right.header))
      .then_with(|| left.provider.as_str().cmp(right.provider.as_str()))
  });
}

fn sort_session_candidates(candidates: &mut [SessionListCandidate]) {
  candidates.sort_by(|left, right| {
    compare_session_headers(&left.header, &right.header)
      .then_with(|| left.provider.as_str().cmp(right.provider.as_str()))
  });
}

fn compare_session_headers(left: &SessionHeader, right: &SessionHeader) -> std::cmp::Ordering {
  right
    .updated_at_ms
    .cmp(&left.updated_at_ms)
    .then_with(|| right.timestamp.cmp(&left.timestamp))
    .then_with(|| left.id.cmp(&right.id))
    .then_with(|| left.path.cmp(&right.path))
}

fn locator_for_header(provider: ViewerProvider, header: &SessionHeader) -> SessionLocator {
  SessionLocator {
    version: 1,
    provider,
    session_id: header.id.clone(),
    source_path: header.path.clone(),
  }
}

#[cfg(test)]
fn session_summary(provider: ViewerProvider, header: SessionHeader) -> Result<SessionSummary, String> {
  session_summary_with_child_count(provider, header, 0, false, SessionAttention::default())
}

fn outstanding_questions(
  events: &[AgentEvent],
  identity: &windows::WindowIdentity,
) -> Vec<crate::model::OutstandingQuestion> {
  let mut questions = crate::questions::Questions::default();
  for (index, event) in events.iter().enumerate() {
    questions.observe(event, index);
  }
  let mut outstanding: Vec<_> = questions.outstanding().collect();
  outstanding.sort_by_key(|(index, required, _)| (!required, *index));
  outstanding
    .into_iter()
    .map(
      |(index, requires_input, unanswered_count)| crate::model::OutstandingQuestion {
        event_key: identity.key(&encode_event_key(index)),
        requires_input,
        unanswered_count,
      },
    )
    .collect()
}

fn session_summary_with_child_count(
  provider: ViewerProvider,
  header: SessionHeader,
  child_count: usize,
  is_subagent: bool,
  attention: SessionAttention,
) -> Result<SessionSummary, String> {
  let locator = locator_for_header(provider, &header);
  let project = header.cwd.as_deref().and_then(path_name).map(str::to_string);
  let title = normalize_session_text(header.title, MAX_SESSION_TITLE_CHARS);
  let preview = normalize_session_text(header.preview, MAX_SESSION_PREVIEW_CHARS);
  let agent_path = normalize_session_text(header.agent_path, MAX_AGENT_IDENTITY_CHARS);
  let agent_nickname = normalize_session_text(header.agent_nickname, MAX_AGENT_IDENTITY_CHARS);
  let agent_role = normalize_session_text(header.agent_role, MAX_AGENT_IDENTITY_CHARS);
  Ok(SessionSummary {
    session_key: encode_session_key(&locator)?,
    session_id: header.id,
    provider,
    title,
    preview,
    project,
    project_order_ms: None,
    project_key: None,
    cwd: header.cwd,
    updated_at_ms: header
      .updated_at_ms
      .or_else(|| parse_updated_at_ms(header.updated_at.as_deref())),
    timestamp: header.timestamp,
    parent_session_id: header.parent_session_id,
    is_subagent,
    agent_path,
    agent_nickname,
    agent_role,
    child_count,
    message_count: None,
    // The listing adapters deliberately inspect only headers. Loading every
    // normalized body here would make the bounded UI query unbounded.
    event_count: None,
    history_status: None,
    has_unread: attention.has_unread,
    has_unread_descendant: false,
    unread_final_count: attention.unread_final_count,
    unread_descendant_count: 0,
    is_running: attention.is_running,
    has_running_descendant: attention.has_running_descendant,
    question_attention: attention.question_attention,
  })
}

fn path_name(path: &str) -> Option<&str> {
  Path::new(path)
    .file_name()
    .and_then(|value| value.to_str())
    .filter(|value| !value.is_empty())
}

fn matches_search(session: &SessionSummary, search: Option<&str>) -> bool {
  let Some(search) = search else {
    return true;
  };
  [
    Some(session.session_id.as_str()),
    session.title.as_deref(),
    session.preview.as_deref(),
    session.project.as_deref(),
    session.cwd.as_deref(),
    session.agent_path.as_deref(),
    session.agent_nickname.as_deref(),
    session.agent_role.as_deref(),
  ]
  .into_iter()
  .flatten()
  .any(|value| value.to_lowercase().contains(search))
}

fn normalize_session_text(value: Option<String>, max_chars: usize) -> Option<String> {
  value
    .as_deref()
    .and_then(|value| normalize_one_line_text(value, max_chars))
}

fn normalize_one_line_text(value: &str, max_chars: usize) -> Option<String> {
  let mut normalized = String::with_capacity(value.len().min(max_chars.saturating_mul(4)));
  let mut characters = value.chars().peekable();
  let mut normalized_chars = 0;
  let mut needs_space = false;
  while let Some(character) = characters.next() {
    match character {
      '\u{001b}' => {
        match characters.next() {
          Some('[') => consume_control_sequence(&mut characters),
          Some(']') => consume_operating_system_command(&mut characters),
          Some(_) | None => {}
        }
        continue;
      }
      '\u{009b}' => {
        consume_control_sequence(&mut characters);
        continue;
      }
      _ => {}
    }
    if character.is_whitespace() {
      needs_space = !normalized.is_empty();
    } else if !is_unsafe_session_character(character) {
      if needs_space {
        if !push_bounded_character(&mut normalized, &mut normalized_chars, ' ', max_chars) {
          break;
        }
      }
      needs_space = false;
      if !push_bounded_character(&mut normalized, &mut normalized_chars, character, max_chars) {
        break;
      }
    }
  }
  if normalized.is_empty() { None } else { Some(normalized) }
}

fn push_bounded_character(output: &mut String, count: &mut usize, character: char, max_chars: usize) -> bool {
  if *count < max_chars {
    output.push(character);
    *count += 1;
    true
  } else {
    if max_chars > 0 {
      output.pop();
      output.push('\u{2026}');
    }
    false
  }
}

fn is_unsafe_session_character(character: char) -> bool {
  character.is_control()
    || matches!(
      character,
      '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
    )
}

fn consume_control_sequence(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
  for character in characters.by_ref() {
    if ('@'..='~').contains(&character) {
      break;
    }
  }
}

fn consume_operating_system_command(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
  while let Some(character) = characters.next() {
    if character == '\u{0007}' {
      break;
    }
    if character == '\u{001b}' && characters.next_if_eq(&'\\').is_some() {
      break;
    }
  }
}

fn event_page_bounds(
  total: usize,
  boundary: usize,
  direction: PageDirection,
  limit: usize,
) -> Result<(usize, usize), String> {
  if boundary > total {
    return Err("event cursor is outside the session".to_string());
  }
  Ok(match direction {
    PageDirection::Forward => (boundary, boundary.saturating_add(limit).min(total)),
    PageDirection::Backward => (boundary.saturating_sub(limit), boundary),
  })
}

/// Builds the pre-projection timeline. Tool operations are assembled before
/// trajectory folding so every later consumer sees one logical tool row and
/// stable child event keys.
fn base_timeline_entries(events: &[AgentEvent]) -> Vec<TimelineEntry> {
  let mut operations_by_timeline_source = HashMap::new();
  let mut hidden_tool_sources = HashSet::new();
  for operation in tokn_session_core::compaction_operations(events) {
    hidden_tool_sources.extend(operation.source_event_indices.into_iter().skip(1));
  }

  for operation in assemble_tool_operations(events) {
    let Some(timeline_source_event_index) = operation.timeline_source_event_index() else {
      continue;
    };
    let Some(&source_event_index) = operation.source_event_indices.first() else {
      continue;
    };
    // Keep the invocation key stable for selection and cached detail, while
    // placing a terminal historical operation where its result occurred.
    hidden_tool_sources.extend(
      operation
        .source_event_indices
        .iter()
        .copied()
        .filter(|index| *index != timeline_source_event_index),
    );
    operations_by_timeline_source.insert(timeline_source_event_index, (source_event_index, operation));
  }

  let mut entries = Vec::with_capacity(events.len().saturating_sub(hidden_tool_sources.len()));
  for source_event_index in 0..events.len() {
    if let Some((detail_source_event_index, operation)) = operations_by_timeline_source.remove(&source_event_index) {
      entries.push(TimelineEntry::ToolOperation {
        source_event_index: detail_source_event_index,
        operation,
      });
    } else if hidden_tool_sources.contains(&source_event_index) {
      continue;
    } else {
      // This should only be reachable for non-tool records. Retaining a
      // standalone tool record if an assembler invariant is violated is safer
      // than silently hiding provider data.
      entries.push(TimelineEntry::Event { source_event_index });
    }
  }
  entries
}

/// Folds each maximal visible work run into one synthetic high-level item.
///
/// User prompts and explicit final assistant replies remain in the outer
/// conversation. Assistant commentary and legacy/unspecified assistant
/// messages instead belong to the surrounding work trajectory so expanding a
/// turn reveals the ordinary assistant cards that led to its final reply. All
/// other message roles remain visible boundaries because their delivery
/// semantics are not reliable enough to fold safely. The grouping is
/// directional: bookkeeping written after a final reply is never made to look
/// like a second completed turn merely because it has a usage record.
fn timeline_entries(events: &[AgentEvent]) -> Vec<TimelineEntry> {
  let base_entries = base_timeline_entries(events);
  let mut entries = Vec::with_capacity(base_entries.len());
  let mut pending = Vec::new();
  // Codex writes a replaceable session token snapshot after a reply. Keep all
  // post-final records as ordinary rows until a new prompt or a non-final
  // assistant message proves that another turn has begun.
  let mut after_final_reply = false;

  for entry in base_entries {
    // Providers can write turn-start before the user prompt. Keep that marker
    // with the work below the prompt instead of making a separate empty turn.
    if matches!(&entry, TimelineEntry::Event { source_event_index } if matches!(&events[*source_event_index], AgentEvent::Message(m) if m.role == Role::User))
      && !pending.is_empty()
      && pending.iter().all(|e| trajectory_state::is_turn_start(e, events))
    {
      after_final_reply = false;
      entries.push(entry);
      continue;
    }
    if trajectory_state::is_turn_start(&entry, events) && !pending.is_empty() {
      flush_trajectory_candidate(&mut entries, &mut pending, events);
    }
    if is_trajectory_boundary(&entry, events) {
      flush_trajectory_candidate(&mut entries, &mut pending, events);
      update_after_final_reply(&entry, events, &mut after_final_reply);
      entries.push(entry);
    } else if after_final_reply && !starts_trajectory_after_final_reply(&entry, events) {
      // Do not hold bookkeeping in `pending`: if later activity starts a real
      // turn, these rows must remain chronologically outside that trajectory.
      entries.push(entry);
    } else {
      after_final_reply = false;
      pending.push(entry);
    }
  }
  flush_trajectory_candidate(&mut entries, &mut pending, events);
  entries
}

fn update_after_final_reply(entry: &TimelineEntry, events: &[AgentEvent], after_final_reply: &mut bool) {
  let TimelineEntry::Event { source_event_index } = entry else {
    return;
  };
  let Some(AgentEvent::Message(message)) = events.get(*source_event_index) else {
    return;
  };

  if message.role == Role::User {
    *after_final_reply = false;
  } else if is_final_assistant_message(message.role, message.delivery) {
    *after_final_reply = true;
  }
}

/// Returns whether a non-boundary row proves that the assistant has started a
/// new turn after a final reply. Late tool, reasoning, lifecycle, error, and
/// usage rows can be postamble from the completed turn, so only a non-final
/// assistant message reopens the pending trajectory. Once it does, ordinary
/// substantive-work rules apply to the remaining rows.
fn starts_trajectory_after_final_reply(entry: &TimelineEntry, events: &[AgentEvent]) -> bool {
  if trajectory_state::is_turn_start(entry, events) {
    return true;
  }
  match entry {
    TimelineEntry::Event { source_event_index } => matches!(
      events.get(*source_event_index),
      Some(AgentEvent::Message(message)) if is_non_final_assistant_message(message.role, message.delivery)
    ),
    TimelineEntry::ToolOperation { .. } | TimelineEntry::Trajectory { .. } => false,
  }
}

fn flush_trajectory_candidate(
  output: &mut Vec<TimelineEntry>,
  pending: &mut Vec<TimelineEntry>,
  events: &[AgentEvent],
) {
  if pending.is_empty() {
    return;
  }
  if !pending.iter().any(|entry| is_substantive_work_entry(entry, events)) {
    output.append(pending);
    return;
  }

  let Some(start_source_event_index) = pending.iter().filter_map(timeline_entry_start_source_event_index).min() else {
    // Every base timeline row should originate from at least one source
    // record. If that invariant is violated, retaining flat rows is safer
    // than inventing a synthetic key with no detail target.
    output.append(pending);
    return;
  };
  output.push(TimelineEntry::Trajectory {
    trajectory: Trajectory {
      start_source_event_index,
      entries: std::mem::take(pending),
    },
  });
}

fn is_trajectory_boundary(entry: &TimelineEntry, events: &[AgentEvent]) -> bool {
  let TimelineEntry::Event { source_event_index } = entry else {
    return false;
  };
  let Some(event) = events.get(*source_event_index) else {
    return true;
  };
  if event.is_hidden() {
    return true;
  }

  match event {
    AgentEvent::Message(message) => !is_non_final_assistant_message(message.role, message.delivery),
    AgentEvent::QuestionRequest(_) => true,
    AgentEvent::QuestionReply(_) => true,
    AgentEvent::SessionStarted(_) | AgentEvent::ProviderChanged(_) | AgentEvent::Compaction(_) => true,
    _ => false,
  }
}

fn is_substantive_work_entry(entry: &TimelineEntry, events: &[AgentEvent]) -> bool {
  match entry {
    TimelineEntry::ToolOperation { .. } => true,
    TimelineEntry::Event { source_event_index } => match events.get(*source_event_index) {
      // A non-final assistant message is itself part of the agent's work
      // trace. Treat it as substantive so standalone commentary/delta runs
      // are consistently represented by the parent `Worked for …` item.
      Some(AgentEvent::Message(message)) => is_non_final_assistant_message(message.role, message.delivery),
      Some(AgentEvent::Usage(usage)) => usage.kind != UsageKind::SessionSnapshot,
      Some(
        AgentEvent::Reasoning(_) | AgentEvent::AgentActivity(_) | AgentEvent::Lifecycle(_) | AgentEvent::Error(_),
      ) => true,
      _ => false,
    },
    TimelineEntry::Trajectory { .. } => false,
  }
}

fn is_non_final_assistant_message(role: Role, delivery: MessageDelivery) -> bool {
  role == Role::Assistant && delivery != MessageDelivery::Final
}

fn is_final_assistant_message(role: Role, delivery: MessageDelivery) -> bool {
  role == Role::Assistant && delivery == MessageDelivery::Final
}

fn timeline_entry_start_source_event_index(entry: &TimelineEntry) -> Option<usize> {
  match entry {
    TimelineEntry::Event { source_event_index } => Some(*source_event_index),
    TimelineEntry::ToolOperation { source_event_index, .. } => Some(*source_event_index),
    TimelineEntry::Trajectory { trajectory } => Some(trajectory.start_source_event_index),
  }
}

fn base_timeline_entry_for_source(events: &[AgentEvent], source_event_index: usize) -> Option<TimelineEntry> {
  base_timeline_entries(events).into_iter().find(|entry| match entry {
    TimelineEntry::Event {
      source_event_index: entry_index,
    } => *entry_index == source_event_index,
    TimelineEntry::ToolOperation {
      source_event_index: entry_index,
      operation,
    } => *entry_index == source_event_index || operation.source_event_indices.contains(&source_event_index),
    TimelineEntry::Trajectory { .. } => false,
  })
}

fn trajectory_for_start(events: &[AgentEvent], start_source_event_index: usize) -> Option<Trajectory> {
  timeline_entries(events).into_iter().find_map(|entry| match entry {
    TimelineEntry::Trajectory { trajectory } if trajectory.start_source_event_index == start_source_event_index => {
      Some(trajectory)
    }
    TimelineEntry::Event { .. } | TimelineEntry::ToolOperation { .. } | TimelineEntry::Trajectory { .. } => None,
  })
}

fn timeline_entry_has_targeted_agent_activity(entry: &TimelineEntry, events: &[AgentEvent]) -> bool {
  match entry {
    TimelineEntry::Event { source_event_index } => matches!(
      events.get(*source_event_index),
      Some(AgentEvent::AgentActivity(activity)) if present_string(activity.target_session_id.as_deref()).is_some()
        || activity.communication.is_some() && (present_string(activity.actor_session_id.as_deref()).is_some()
          || present_string(activity.actor_agent_path.as_deref()).is_some())
    ),
    TimelineEntry::ToolOperation { .. } => false,
    TimelineEntry::Trajectory { trajectory } => trajectory
      .entries
      .iter()
      .any(|entry| timeline_entry_has_targeted_agent_activity(entry, events)),
  }
}

fn timeline_entry_event_summary(
  entry: &TimelineEntry,
  events: &[AgentEvent],
  delegation_targets: &ActivityTargets,
  intermediate_usage: &HashSet<usize>,
) -> EventSummary {
  match entry {
    TimelineEntry::Event { source_event_index } => event_summary_with_delegation_targets(
      events,
      *source_event_index,
      &events[*source_event_index],
      delegation_targets,
      intermediate_usage,
    ),
    TimelineEntry::ToolOperation {
      source_event_index,
      operation,
    } => tool_operation_event_summary(*source_event_index, operation),
    TimelineEntry::Trajectory { trajectory } => trajectory_event_summary(trajectory, events),
  }
}

fn trajectory_event_summary(trajectory: &Trajectory, events: &[AgentEvent]) -> EventSummary {
  let card = trajectory_card_summary(trajectory, events);
  let provider = trajectory
    .entries
    .last()
    .map(|entry| timeline_entry_provider(entry, events))
    .unwrap_or(ViewerProvider::Codex);
  let summary = trajectory_summary(&card);

  EventSummary {
    child_keys: Some(
      trajectory
        .entries
        .iter()
        .map(|entry| {
          encode_event_key(timeline_entry_start_source_event_index(entry).expect("work entry has a source position"))
        })
        .collect(),
    ),
    slot_key: None,
    event_key: encode_trajectory_key(trajectory.start_source_event_index),
    event_type: "trajectory".to_string(),
    provider,
    timestamp: card.ended_at.clone(),
    delivery: None,
    phase: None,
    role: None,
    title: "Trajectory".to_string(),
    summary,
    summary_truncated: false,
    is_hidden: false,
    is_bookkeeping: false,
    is_error: (card.error_count > 0).then_some(true),
    tool: None,
    usage: None,
    reasoning: None,
    trajectory: Some(card),
    agent_activity: None,
    compaction: None,
  }
}

fn trajectory_summary(card: &TrajectoryCardSummary) -> String {
  let mut parts = vec![format!("{} events", card.event_count)];
  if card.tool_count > 0 {
    parts.push(format!("{} tools", card.tool_count));
  }
  if card.reasoning_count > 0 {
    parts.push(format!("{} reasoning", card.reasoning_count));
  }
  if card.agent_activity_count > 0 {
    parts.push(format!("{} agent activities", card.agent_activity_count));
  }
  if card.error_count > 0 {
    parts.push(format!("{} errors", card.error_count));
  }
  if card.unknown_count > 0 {
    parts.push(format!("{} unknown", card.unknown_count));
  }
  truncate(parts.join(", "), MAX_TECHNICAL_SUMMARY_CHARS)
}

fn timeline_entry_provider(entry: &TimelineEntry, events: &[AgentEvent]) -> ViewerProvider {
  match entry {
    TimelineEntry::Event { source_event_index } => events
      .get(*source_event_index)
      .map(provider_for_event)
      .unwrap_or(ViewerProvider::Codex),
    TimelineEntry::ToolOperation { operation, .. } => viewer_provider(operation.provider),
    TimelineEntry::Trajectory { trajectory } => trajectory
      .entries
      .last()
      .map(|entry| timeline_entry_provider(entry, events))
      .unwrap_or(ViewerProvider::Codex),
  }
}

fn trajectory_card_summary(trajectory: &Trajectory, events: &[AgentEvent]) -> TrajectoryCardSummary {
  let mut reasoning_count = 0;
  let mut tool_count = 0;
  let mut agent_activity_count = 0;
  let mut lifecycle_count = 0;
  let mut usage_count = 0;
  let mut error_count = 0;
  let mut unknown_count = 0;
  let mut first_timestamp = None;
  let mut last_timestamp = None;

  for entry in &trajectory.entries {
    match entry {
      TimelineEntry::ToolOperation { operation, .. } => {
        tool_count += 1;
        if operation.is_error == Some(true) || matches!(operation.status, ToolOperationStatus::Failed) {
          error_count += 1;
        }
      }
      TimelineEntry::Event { source_event_index } => {
        let Some(event) = events.get(*source_event_index) else {
          continue;
        };
        match event {
          AgentEvent::Reasoning(_) => reasoning_count += 1,
          AgentEvent::AgentActivity(_) => agent_activity_count += 1,
          AgentEvent::Lifecycle(_) => lifecycle_count += 1,
          AgentEvent::Usage(_) => usage_count += 1,
          AgentEvent::Error(_) => error_count += 1,
          AgentEvent::Unknown(_) => unknown_count += 1,
          AgentEvent::SessionStarted(_)
          | AgentEvent::ProviderChanged(_)
          | AgentEvent::SessionSettingsApplied(_)
          | AgentEvent::Message(_)
          | AgentEvent::QuestionRequest(_)
          | AgentEvent::QuestionReply(_)
          | AgentEvent::GoalUpdated(_)
          | AgentEvent::ToolCall(_)
          | AgentEvent::Metadata(_)
          | AgentEvent::Compaction(_) => {}
        }
        if error_for_event(event) == Some(true) && !matches!(event, AgentEvent::Error(_)) {
          error_count += 1;
        }
      }
      TimelineEntry::Trajectory { .. } => {}
    }

    if let Some((timestamp, timestamp_ms)) = timeline_entry_parseable_timestamp(entry, events) {
      if first_timestamp.is_none() {
        first_timestamp = Some((timestamp.clone(), timestamp_ms));
      }
      last_timestamp = Some((timestamp, timestamp_ms));
    }
  }

  let status = trajectory_state::status(trajectory, events);
  if status == "complete"
    && let Some(timestamp) = trajectory_state::completion_timestamp(trajectory, events)
    && let Some(time) = parse_updated_at_ms(Some(timestamp))
    && last_timestamp.as_ref().is_some_and(|(_, last)| time >= *last)
  {
    last_timestamp =
      normalize_one_line_text(timestamp, MAX_TRAJECTORY_TIMESTAMP_CHARS).map(|timestamp| (timestamp, time));
  }
  let started_at = first_timestamp.as_ref().map(|(timestamp, _)| timestamp.clone());
  let ended_at = last_timestamp.as_ref().map(|(timestamp, _)| timestamp.clone());
  let duration_ms = first_timestamp
    .zip(last_timestamp)
    .and_then(|((_, start), (_, end))| (end >= start).then_some((end - start).to_string()));

  TrajectoryCardSummary {
    status,
    event_count: trajectory.entries.len(),
    source_event_count: trajectory_source_event_indices(trajectory).len(),
    reasoning_count,
    tool_count,
    agent_activity_count,
    lifecycle_count,
    usage_count,
    error_count,
    unknown_count,
    started_at,
    ended_at,
    duration_ms,
  }
}

fn timeline_entry_parseable_timestamp(entry: &TimelineEntry, events: &[AgentEvent]) -> Option<(String, i64)> {
  let timestamp = match entry {
    TimelineEntry::Event { source_event_index } => events.get(*source_event_index).and_then(timestamp_for_event),
    TimelineEntry::ToolOperation { operation, .. } => {
      if operation.is_finished() {
        operation.updated_at.as_deref().or(operation.started_at.as_deref())
      } else {
        operation.started_at.as_deref().or(operation.updated_at.as_deref())
      }
    }
    TimelineEntry::Trajectory { .. } => None,
  }?;
  let timestamp_ms = parse_updated_at_ms(Some(timestamp))?;
  let timestamp = normalize_one_line_text(timestamp, MAX_TRAJECTORY_TIMESTAMP_CHARS)?;
  Some((timestamp, timestamp_ms))
}

fn trajectory_source_event_indices(trajectory: &Trajectory) -> Vec<usize> {
  let mut source_event_indices = trajectory
    .entries
    .iter()
    .flat_map(timeline_entry_source_event_indices)
    .collect::<Vec<_>>();
  source_event_indices.sort_unstable();
  source_event_indices.dedup();
  source_event_indices
}

fn timeline_entry_source_event_indices(entry: &TimelineEntry) -> Vec<usize> {
  match entry {
    TimelineEntry::Event { source_event_index } => vec![*source_event_index],
    TimelineEntry::ToolOperation { operation, .. } => operation.source_event_indices.clone(),
    TimelineEntry::Trajectory { trajectory } => trajectory_source_event_indices(trajectory),
  }
}

fn requested_trajectory_offset(
  cursor: Option<&str>,
  offset: Option<usize>,
  start_source_event_index: usize,
) -> Result<Option<usize>, String> {
  match (cursor, offset) {
    (Some(_), Some(_)) => Err("cursor and offset cannot be used together".to_string()),
    (Some(cursor), None) => {
      let (cursor_anchor, offset) = decode_trajectory_event_cursor(cursor)?;
      if cursor_anchor != start_source_event_index {
        return Err("trajectory cursor does not match the requested trajectory".to_string());
      }
      Ok(Some(offset))
    }
    (None, offset) => Ok(offset),
  }
}

#[cfg(test)]
fn event_summary(events: &[AgentEvent], index: usize, event: &AgentEvent) -> EventSummary {
  event_summary_with_delegation_targets(
    events,
    index,
    event,
    &ActivityTargets::default(),
    &usage_filter::intermediate_usage(events),
  )
}

fn event_summary_with_delegation_targets(
  events: &[AgentEvent],
  index: usize,
  event: &AgentEvent,
  delegation_targets: &ActivityTargets,
  intermediate_usage: &HashSet<usize>,
) -> EventSummary {
  let projected = if matches!(event, AgentEvent::Compaction(_)) {
    compaction::for_source(events, index).map(|operation| AgentEvent::Compaction(operation.event))
  } else {
    None
  };
  let event = projected.as_ref().unwrap_or(event);
  let hidden = event.is_hidden();
  let title = if hidden {
    "Hidden provider content".to_string()
  } else {
    truncate(event_title(event), 120)
  };
  let reasoning = (!hidden).then(|| reasoning_card_summary(event)).flatten();
  let summary = if hidden {
    render_event_summary(event)
  } else {
    match event {
      AgentEvent::Message(message) => message.text.clone(),
      AgentEvent::QuestionRequest(request) => request
        .questions
        .first()
        .map(|question| question.question.clone())
        .unwrap_or_default(),
      AgentEvent::QuestionReply(reply) => reply
        .replies
        .iter()
        .flat_map(|reply| &reply.answers)
        .next()
        .cloned()
        .unwrap_or_else(|| "No answers recorded".into()),
      AgentEvent::Reasoning(_) => reasoning
        .as_ref()
        .and_then(|card| {
          card
            .is_redacted
            .then_some("Reasoning redacted by provider".to_string())
            .or_else(|| card.preview.clone())
        })
        .unwrap_or_else(|| "Reasoning".to_string()),
      _ => render_event_summary(event),
    }
  };
  let summary_max_chars = if !hidden && matches!(event, AgentEvent::Message(_)) {
    MAX_MESSAGE_SUMMARY_CHARS
  } else {
    MAX_TECHNICAL_SUMMARY_CHARS
  };
  let (summary, summary_truncated) = truncate_with_flag(summary, summary_max_chars);
  let tool = (!hidden).then(|| tool_event(event).map(tool_card_summary)).flatten();
  let usage = (!hidden).then(|| usage_card_summary(event)).flatten();
  let agent_activity = (!hidden)
    .then(|| match event {
      AgentEvent::AgentActivity(activity) => Some(agent_activity_card_summary(activity, delegation_targets)),
      _ => None,
    })
    .flatten();
  EventSummary {
    child_keys: None,
    slot_key: None,
    event_key: encode_event_key(index),
    event_type: normalized_event_type(event).to_string(),
    provider: provider_for_event(event),
    timestamp: timestamp_for_event(event).map(str::to_string),
    delivery: match event {
      AgentEvent::Message(message) => serialized_label(message.delivery),
      _ => None,
    },
    phase: phase_for_event(event),
    role: role_for_event(event),
    title,
    summary,
    summary_truncated,
    is_hidden: hidden,
    is_bookkeeping: !hidden && (event_filter::is_bookkeeping(event) || intermediate_usage.contains(&index)),
    is_error: error_for_event(event),
    tool,
    usage,
    reasoning,
    trajectory: None,
    agent_activity,
    compaction: compaction::card(event),
  }
}

fn tool_operation_event_summary(source_event_index: usize, operation: &ToolOperation) -> EventSummary {
  let tool = tool_operation_card_summary(operation);
  let title = truncate(
    operation
      .tool_name
      .as_deref()
      .or(operation.provider_tool_name.as_deref())
      .unwrap_or("Tool operation")
      .to_string(),
    120,
  );
  let (summary, summary_truncated) =
    truncate_with_flag(tool_operation_summary(operation, &tool), MAX_TECHNICAL_SUMMARY_CHARS);
  EventSummary {
    child_keys: None,
    slot_key: None,
    event_key: encode_event_key(source_event_index),
    event_type: "tool_call".to_string(),
    provider: viewer_provider(operation.provider),
    timestamp: if operation.is_finished() {
      operation.updated_at.clone().or_else(|| operation.started_at.clone())
    } else {
      operation.started_at.clone().or_else(|| operation.updated_at.clone())
    },
    // A logical operation has an explicit derived status. Exposing the source
    // record phase here would recreate the old `finished` ambiguity.
    delivery: None,
    phase: None,
    role: None,
    title,
    summary,
    summary_truncated,
    is_hidden: false,
    is_bookkeeping: false,
    // Preserve an unspecified provider error state. A failed assembled
    // operation is the only case where we need to synthesize `true`.
    is_error: operation
      .is_error
      .or(matches!(operation.status, ToolOperationStatus::Failed).then_some(true)),
    tool: Some(tool),
    usage: None,
    reasoning: None,
    trajectory: None,
    agent_activity: None,
    compaction: None,
  }
}

fn tool_operation_summary(operation: &ToolOperation, card: &ToolCardSummary) -> String {
  match operation.summary.as_ref() {
    Some(ToolSummary::Shell { command, .. }) => command.clone().unwrap_or_else(|| "Shell operation".to_string()),
    Some(ToolSummary::Terminal {
      session_id,
      action,
      chars_len,
      wait_ms,
    }) => match action {
      Some(TerminalAction::Wait) => format!(
        "Wait for terminal {}{}",
        session_id.as_deref().unwrap_or("session"),
        wait_ms
          .map(|value| format!(" for up to {value} ms"))
          .unwrap_or_default(),
      ),
      Some(TerminalAction::Send) => format!(
        "Send {} characters to terminal {}",
        chars_len.unwrap_or(0),
        session_id.as_deref().unwrap_or("session"),
      ),
      None => "Terminal operation".to_string(),
    },
    Some(ToolSummary::CodeExecution { language }) => {
      format!("{} code execution", language.as_deref().unwrap_or("Unknown"))
    }
    Some(ToolSummary::FileRead { path }) => path.clone().unwrap_or_else(|| "Read file".to_string()),
    Some(ToolSummary::FileWrite { path, .. }) => path.clone().unwrap_or_else(|| "Write file".to_string()),
    Some(ToolSummary::FileEdit { path, .. }) => path.clone().unwrap_or_else(|| "Edit file".to_string()),
    Some(ToolSummary::Search { query }) => query.clone().unwrap_or_else(|| "Search".to_string()),
    Some(ToolSummary::Web { url }) => url.clone().unwrap_or_else(|| "Web request".to_string()),
    Some(ToolSummary::Task { title }) => title.clone().unwrap_or_else(|| "Task".to_string()),
    None => card.tool_name.clone().unwrap_or_else(|| "Tool operation".to_string()),
  }
}

fn usage_card_summary(event: &AgentEvent) -> Option<UsageCardSummary> {
  let AgentEvent::Usage(usage) = event else {
    return None;
  };

  Some(UsageCardSummary {
    kind: usage_kind_label(usage.kind).to_string(),
    input_tokens: usage.input_tokens.to_string(),
    output_tokens: usage.output_tokens.to_string(),
    total_tokens: usage.total_tokens.map(|value| value.to_string()),
    cache_read_tokens: usage.cache_read_tokens.map(|value| value.to_string()),
    cache_write_tokens: usage.cache_write_tokens.map(|value| value.to_string()),
    reasoning_tokens: usage.reasoning_tokens.map(|value| value.to_string()),
    turn_id: present_string(usage.turn_id.as_deref()).map(str::to_string),
    step_id: present_string(usage.step_id.as_deref()).map(str::to_string),
  })
}

fn usage_kind_label(kind: UsageKind) -> &'static str {
  match kind {
    UsageKind::ModelCall => "model_call",
    UsageKind::OperationTotal => "operation_total",
    UsageKind::SessionSnapshot => "session_snapshot",
  }
}

fn reasoning_card_summary(event: &AgentEvent) -> Option<ReasoningCardSummary> {
  let AgentEvent::Reasoning(reasoning) = event else {
    return None;
  };

  let summary_preview = reasoning
    .summary
    .as_deref()
    .and_then(|value| normalize_one_line_text(value, MAX_REASONING_CARD_PREVIEW_CHARS));
  let text_preview = reasoning
    .text
    .as_deref()
    .and_then(|value| normalize_one_line_text(value, MAX_REASONING_CARD_PREVIEW_CHARS));
  let is_redacted = reasoning.redacted == Some(true);
  let has_summary = reasoning.summary.is_some();
  let has_text = reasoning.text.is_some();
  let preview = (!is_redacted).then(|| summary_preview.or(text_preview)).flatten();

  Some(ReasoningCardSummary {
    preview,
    has_summary,
    has_text,
    has_encrypted_content: reasoning.encrypted_content.is_some(),
    is_redacted,
  })
}

fn tool_card_summary(event: &ToolCallEvent) -> ToolCardSummary {
  let mut card = ToolCardSummary {
    kind: tool_kind_label(event.tool_kind).to_string(),
    tool_name: present_string(event.tool_name.as_deref()).map(str::to_string),
    tool_call_id: present_string(event.tool_call_id.as_deref()).map(str::to_string),
    status: tool_record_status_label(event).to_string(),
    provider_tool_name: present_string(event.effective_provider_tool_name()).map(str::to_string),
    language: None,
    command: None,
    cwd: None,
    terminal_session_id: None,
    terminal_action: None,
    chars_len: None,
    wait_ms: None,
    path: None,
    query: None,
    url: None,
    task_title: None,
    exit_code: None,
    bytes: None,
    added: None,
    removed: None,
  };

  match event.summary.as_ref() {
    Some(ToolSummary::CodeExecution { language }) => card.language.clone_from(language),
    Some(ToolSummary::Shell {
      command,
      cwd,
      exit_code,
    }) => {
      card.command.clone_from(command);
      card.cwd.clone_from(cwd);
      card.exit_code = *exit_code;
    }
    Some(ToolSummary::Terminal {
      session_id,
      action,
      chars_len,
      wait_ms,
    }) => {
      card.terminal_session_id.clone_from(session_id);
      card.terminal_action = action.map(terminal_action_label).map(str::to_string);
      card.chars_len = *chars_len;
      card.wait_ms = *wait_ms;
    }
    Some(ToolSummary::FileRead { path }) => card.path.clone_from(path),
    Some(ToolSummary::FileWrite { path, bytes }) => {
      card.path.clone_from(path);
      card.bytes = *bytes;
    }
    Some(ToolSummary::FileEdit { path, added, removed }) => {
      card.path.clone_from(path);
      card.added = *added;
      card.removed = *removed;
    }
    Some(ToolSummary::Search { query }) => card.query.clone_from(query),
    Some(ToolSummary::Web { url }) => card.url.clone_from(url),
    Some(ToolSummary::Task { title }) => card.task_title.clone_from(title),
    None => {}
  }
  card
}

fn tool_operation_card_summary(operation: &ToolOperation) -> ToolCardSummary {
  let mut card = ToolCardSummary {
    kind: tool_kind_label(operation.tool_kind).to_string(),
    tool_name: present_string(operation.tool_name.as_deref()).map(str::to_string),
    tool_call_id: present_string(operation.tool_call_id.as_deref()).map(str::to_string),
    status: tool_operation_status_label(operation.status).to_string(),
    provider_tool_name: present_string(operation.provider_tool_name.as_deref()).map(str::to_string),
    language: None,
    command: None,
    cwd: None,
    terminal_session_id: None,
    terminal_action: None,
    chars_len: None,
    wait_ms: None,
    path: None,
    query: None,
    url: None,
    task_title: None,
    exit_code: None,
    bytes: None,
    added: None,
    removed: None,
  };

  match operation.summary.as_ref() {
    Some(ToolSummary::CodeExecution { language }) => card.language.clone_from(language),
    Some(ToolSummary::Shell {
      command,
      cwd,
      exit_code,
    }) => {
      card.command.clone_from(command);
      card.cwd.clone_from(cwd);
      card.exit_code = *exit_code;
    }
    Some(ToolSummary::Terminal {
      session_id,
      action,
      chars_len,
      wait_ms,
    }) => {
      card.terminal_session_id.clone_from(session_id);
      card.terminal_action = action.map(terminal_action_label).map(str::to_string);
      card.chars_len = *chars_len;
      card.wait_ms = *wait_ms;
    }
    Some(ToolSummary::FileRead { path }) => card.path.clone_from(path),
    Some(ToolSummary::FileWrite { path, bytes }) => {
      card.path.clone_from(path);
      card.bytes = *bytes;
    }
    Some(ToolSummary::FileEdit { path, added, removed }) => {
      card.path.clone_from(path);
      card.added = *added;
      card.removed = *removed;
    }
    Some(ToolSummary::Search { query }) => card.query.clone_from(query),
    Some(ToolSummary::Web { url }) => card.url.clone_from(url),
    Some(ToolSummary::Task { title }) => card.task_title.clone_from(title),
    None => {}
  }
  bound_tool_card_summary(card)
}

fn terminal_action_label(action: TerminalAction) -> &'static str {
  match action {
    TerminalAction::Send => "send",
    TerminalAction::Wait => "wait",
  }
}

fn tool_operation_status_label(status: ToolOperationStatus) -> &'static str {
  match status {
    ToolOperationStatus::Pending => "pending",
    ToolOperationStatus::Running => "running",
    ToolOperationStatus::Completed => "completed",
    ToolOperationStatus::Failed => "failed",
  }
}

fn tool_record_status_label(event: &ToolCallEvent) -> &'static str {
  if event.is_error == Some(true) {
    return "failed";
  }
  match event.phase {
    Phase::Started => "pending",
    Phase::Delta | Phase::Updated => "running",
    Phase::Finished => "completed",
  }
}

fn bound_tool_card_summary(mut card: ToolCardSummary) -> ToolCardSummary {
  card.kind = truncate(card.kind, MAX_TOOL_NAME_CHARS);
  card.tool_name = truncate_option(card.tool_name, MAX_TOOL_NAME_CHARS);
  card.tool_call_id = truncate_option(card.tool_call_id, MAX_TOOL_CARD_STRING_CHARS);
  card.status = truncate(card.status, MAX_TOOL_NAME_CHARS);
  card.provider_tool_name = truncate_option(card.provider_tool_name, MAX_TOOL_NAME_CHARS);
  card.language = truncate_option(card.language, MAX_TOOL_NAME_CHARS);
  card.command = truncate_option(card.command, MAX_TOOL_CARD_STRING_CHARS);
  card.cwd = truncate_option(card.cwd, MAX_TOOL_CARD_STRING_CHARS);
  card.terminal_session_id = truncate_option(card.terminal_session_id, MAX_TOOL_CARD_STRING_CHARS);
  card.terminal_action = truncate_option(card.terminal_action, MAX_TOOL_NAME_CHARS);
  card.path = truncate_option(card.path, MAX_TOOL_CARD_STRING_CHARS);
  card.query = truncate_option(card.query, MAX_TOOL_CARD_STRING_CHARS);
  card.url = truncate_option(card.url, MAX_TOOL_CARD_STRING_CHARS);
  card.task_title = truncate_option(card.task_title, MAX_TOOL_CARD_STRING_CHARS);
  card
}

fn truncate_option(value: Option<String>, max_chars: usize) -> Option<String> {
  value.map(|value| truncate(value, max_chars))
}

fn tool_kind_label(kind: ToolKind) -> &'static str {
  match kind {
    ToolKind::CodeExecution => "code_execution",
    ToolKind::Shell => "shell",
    ToolKind::Terminal => "terminal",
    ToolKind::FileRead => "file_read",
    ToolKind::FileWrite => "file_write",
    ToolKind::FileEdit => "file_edit",
    ToolKind::Search => "search",
    ToolKind::Web => "web",
    ToolKind::Task => "task",
    ToolKind::Unknown => "unknown",
  }
}

/// Matches the serialized `AgentEvent` discriminants consumed by the viewer.
/// Keep this exhaustive so adding an IR variant cannot silently fall back to a
/// render-layer display label.
fn normalized_event_type(event: &AgentEvent) -> &'static str {
  match event {
    AgentEvent::SessionStarted(_) => "session_started",
    AgentEvent::ProviderChanged(_) => "provider_changed",
    AgentEvent::SessionSettingsApplied(_) => "session_settings_applied",
    AgentEvent::Message(_) => "message",
    AgentEvent::QuestionRequest(_) => "question_request",
    AgentEvent::QuestionReply(_) => "question_reply",
    AgentEvent::Reasoning(_) => "reasoning",
    AgentEvent::GoalUpdated(_) => "goal_updated",
    AgentEvent::AgentActivity(_) => "agent_activity",
    AgentEvent::ToolCall(_) => "tool_call",
    AgentEvent::Lifecycle(_) => "lifecycle",
    AgentEvent::Usage(_) => "usage",
    AgentEvent::Metadata(_) => "metadata",
    AgentEvent::Compaction(_) => "compaction",
    AgentEvent::Error(_) => "error",
    AgentEvent::Unknown(_) => "unknown",
  }
}

fn event_title(event: &AgentEvent) -> String {
  match event {
    AgentEvent::SessionStarted(_) => "Session started".to_string(),
    AgentEvent::ProviderChanged(_) => "Provider changed".to_string(),
    AgentEvent::SessionSettingsApplied(_) => "Session settings".to_string(),
    AgentEvent::Message(event) => match event.role {
      Role::User => "User".to_string(),
      Role::Assistant => "Assistant".to_string(),
      Role::System => "System".to_string(),
      Role::Tool => "Tool message".to_string(),
      Role::Unknown => "Message".to_string(),
    },
    AgentEvent::Reasoning(_) => "Reasoning".to_string(),
    AgentEvent::QuestionRequest(_) => "Questions".to_string(),
    AgentEvent::QuestionReply(_) => "Question answers".to_string(),
    AgentEvent::GoalUpdated(_) => "Goal updated".to_string(),
    AgentEvent::AgentActivity(_) => "Agent activity".to_string(),
    AgentEvent::ToolCall(event) => event.tool_name.clone().unwrap_or_else(|| "Tool call".to_string()),
    AgentEvent::Lifecycle(_) => "Lifecycle".to_string(),
    AgentEvent::Usage(_) => "Usage".to_string(),
    AgentEvent::Metadata(event) => event.native_type.clone(),
    AgentEvent::Compaction(event) => event.state.label().into(),
    AgentEvent::Error(_) => "Error".to_string(),
    AgentEvent::Unknown(event) => event.native_type.clone().unwrap_or_else(|| "Unknown event".to_string()),
  }
}

fn provider_for_event(event: &AgentEvent) -> ViewerProvider {
  let provider = match event {
    AgentEvent::SessionStarted(event) => event.provider,
    AgentEvent::ProviderChanged(event) => event.provider,
    AgentEvent::SessionSettingsApplied(event) => event.provider,
    AgentEvent::Message(event) => event.provider,
    AgentEvent::QuestionRequest(event) => event.provider,
    AgentEvent::QuestionReply(event) => event.provider,
    AgentEvent::Reasoning(event) => event.provider,
    AgentEvent::GoalUpdated(event) => event.provider,
    AgentEvent::AgentActivity(event) => event.provider,
    AgentEvent::ToolCall(event) => event.provider,
    AgentEvent::Lifecycle(event) => event.provider,
    AgentEvent::Usage(event) => event.provider,
    AgentEvent::Metadata(event) => event.provider,
    AgentEvent::Compaction(event) => event.provider,
    AgentEvent::Error(event) => event.provider,
    AgentEvent::Unknown(event) => event.provider,
  };
  viewer_provider(provider)
}

fn viewer_provider(provider: Provider) -> ViewerProvider {
  match provider {
    Provider::Codex => ViewerProvider::Codex,
    Provider::Pi => ViewerProvider::Pi,
    Provider::OpenCode => ViewerProvider::OpenCode,
    Provider::ZCode => ViewerProvider::ZCode,
    Provider::WorkBuddy => ViewerProvider::WorkBuddy,
    Provider::Dsh => ViewerProvider::Dsh,
  }
}

fn timestamp_for_event(event: &AgentEvent) -> Option<&str> {
  match event {
    AgentEvent::SessionStarted(event) => event.timestamp.as_deref(),
    AgentEvent::ProviderChanged(event) => event.timestamp.as_deref(),
    AgentEvent::SessionSettingsApplied(event) => event.timestamp.as_deref(),
    AgentEvent::Message(event) => event.timestamp.as_deref(),
    AgentEvent::QuestionRequest(event) => event.timestamp.as_deref(),
    AgentEvent::QuestionReply(event) => event.timestamp.as_deref(),
    AgentEvent::Reasoning(event) => event.timestamp.as_deref(),
    AgentEvent::GoalUpdated(event) => event.timestamp.as_deref(),
    AgentEvent::AgentActivity(event) => event.timestamp.as_deref(),
    AgentEvent::ToolCall(event) => event.timestamp.as_deref(),
    AgentEvent::Lifecycle(event) => event.timestamp.as_deref(),
    AgentEvent::Usage(event) => event.timestamp.as_deref(),
    AgentEvent::Metadata(event) => event.timestamp.as_deref(),
    AgentEvent::Compaction(event) => event.timestamp.as_deref(),
    AgentEvent::Error(event) => event.timestamp.as_deref(),
    AgentEvent::Unknown(event) => event.timestamp.as_deref(),
  }
}

fn phase_for_event(event: &AgentEvent) -> Option<String> {
  match event {
    AgentEvent::Message(event) => serialized_label(event.phase),
    AgentEvent::QuestionRequest(event) => serialized_label(event.phase),
    AgentEvent::Reasoning(event) => serialized_label(event.phase),
    AgentEvent::ToolCall(event) => serialized_label(event.phase),
    AgentEvent::Lifecycle(event) => serialized_label(event.phase),
    AgentEvent::Compaction(event) => serialized_label(event.state),
    _ => None,
  }
}

fn role_for_event(event: &AgentEvent) -> Option<String> {
  match event {
    AgentEvent::QuestionReply(_) => serialized_label(Role::User),
    AgentEvent::Message(event) => serialized_label(event.role),
    _ => None,
  }
}

fn serialized_label(value: impl Serialize) -> Option<String> {
  serde_json::to_value(value).ok()?.as_str().map(str::to_string)
}

fn error_for_event(event: &AgentEvent) -> Option<bool> {
  match event {
    AgentEvent::Error(_) => Some(true),
    AgentEvent::ToolCall(event) => event.is_error,
    AgentEvent::Lifecycle(event) => Some(matches!(event.outcome, Some(LifecycleOutcome::Failed))),
    AgentEvent::Compaction(event) => Some(event.state == tokn_session_core::CompactionState::Failed),
    _ => None,
  }
}

fn tool_event(event: &AgentEvent) -> Option<&ToolCallEvent> {
  match event {
    AgentEvent::ToolCall(event) => Some(event),
    _ => None,
  }
}

fn present_string(value: Option<&str>) -> Option<&str> {
  value.map(str::trim).filter(|value| !value.is_empty())
}

fn tool_output_preview(events: &[AgentEvent], index: usize) -> Option<ToolOutputPreview> {
  let origin = events.get(index).and_then(tool_event)?;
  projected_event_output(origin).map(|sections| bound_tool_output(sections, index))
}

fn projected_event_output(event: &ToolCallEvent) -> Option<Vec<ToolOutputSection>> {
  let output = event.output.as_ref().filter(|value| !value.is_null())?;
  let sections = project_output(output, 0);
  (!sections.is_empty()).then_some(sections)
}

fn project_output(value: &Value, depth: usize) -> Vec<ToolOutputSection> {
  const MAX_OUTPUT_PROJECTION_DEPTH: usize = 16;
  if depth >= MAX_OUTPUT_PROJECTION_DEPTH {
    return json_section(None, value).into_iter().collect();
  }

  match value {
    Value::Null => Vec::new(),
    Value::String(text) if text.is_empty() => Vec::new(),
    Value::String(text) => vec![text_section(None, text.clone())],
    Value::Array(values) if values.is_empty() => Vec::new(),
    Value::Array(_) => content_text(value)
      .map(|text| vec![text_section(None, text)])
      .unwrap_or_else(|| json_section(None, value).into_iter().collect()),
    Value::Object(object) if object.is_empty() => Vec::new(),
    Value::Object(object) => {
      if let Some(sections) = shell_output_sections(object) {
        return sections;
      }

      // OpenCode wraps provider output in an `output` object. Recurse through
      // that wrapper before considering metadata or the raw fallback.
      if object.contains_key("output") {
        let mut sections = object
          .get("output")
          .map(|output| project_output(output, depth + 1))
          .unwrap_or_default();
        if sections.is_empty()
          && let Some(raw) = object.get("raw")
        {
          sections = project_labeled_value("Raw", raw, depth + 1);
        }
        if sections.is_empty()
          && let Some(error) = object.get("error")
        {
          sections = project_labeled_value("Error", error, depth + 1);
        }
        if sections.is_empty()
          && let Some(metadata) = object.get("metadata").filter(|value| !is_effectively_empty(value))
        {
          sections = project_labeled_value("Metadata", metadata, depth + 1);
        }
        return sections;
      }

      // Semantic tool adapters, including Codex Code Mode, commonly retain
      // response metadata next to one readable `text` field. Show that
      // payload directly instead of turning the entire result into JSON.
      if let Some(text) = object
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
      {
        let mut sections = vec![text_section(None, text.to_string())];
        append_distinct_error(&mut sections, object.get("error"), depth + 1);
        return sections;
      }

      if let Some(content) = object.get("content") {
        let mut sections = content_text(content)
          .map(|text| vec![text_section(None, text)])
          .unwrap_or_else(|| project_output(content, depth + 1));
        append_distinct_error(&mut sections, object.get("error"), depth + 1);
        if !sections.is_empty() {
          return sections;
        }
      }

      if let Some(content_items) = object.get("content_items") {
        let mut sections = content_text(content_items)
          .map(|text| vec![text_section(None, text)])
          .unwrap_or_else(|| project_labeled_value("Content", content_items, depth + 1));
        append_distinct_error(&mut sections, object.get("error"), depth + 1);
        if !sections.is_empty() {
          return sections;
        }
      }

      if let Some(result) = object.get("result") {
        let sections = project_labeled_value("Result", result, depth + 1);
        if !sections.is_empty() {
          return sections;
        }
      }

      if let Some(results) = object.get("results") {
        let sections = project_labeled_value("Results", results, depth + 1);
        if !sections.is_empty() {
          return sections;
        }
      }

      if let Some(error) = object.get("error") {
        let sections = project_labeled_value("Error", error, depth + 1);
        if !sections.is_empty() {
          return sections;
        }
      }

      if is_effectively_empty(value) {
        Vec::new()
      } else {
        json_section(None, value).into_iter().collect()
      }
    }
    Value::Bool(_) | Value::Number(_) => json_section(None, value).into_iter().collect(),
  }
}

fn shell_output_sections(object: &serde_json::Map<String, Value>) -> Option<Vec<ToolOutputSection>> {
  const SHELL_FIELDS: [&str; 4] = ["formatted_output", "aggregated_output", "stdout", "stderr"];
  if !SHELL_FIELDS.iter().any(|field| object.contains_key(*field)) {
    return None;
  }

  let main = ["formatted_output", "aggregated_output", "stdout"]
    .into_iter()
    .find_map(|field| nonempty_string_field(object, field).map(|text| (field, text)));
  let stderr = nonempty_string_field(object, "stderr");
  let mut sections = Vec::new();
  if let Some((field, text)) = main {
    let label = if field == "stdout" { "Stdout" } else { "Output" };
    sections.push(text_section(Some(label), text.to_string()));
  }
  if let Some(stderr) = stderr {
    let is_distinct = sections
      .iter()
      .all(|section| section.text != stderr && !section.text.contains(stderr));
    if is_distinct {
      sections.push(text_section(Some("Stderr"), stderr.to_string()));
    }
  }

  if sections.is_empty()
    && SHELL_FIELDS
      .iter()
      .filter_map(|field| object.get(*field))
      .any(|value| !is_effectively_empty(value))
  {
    return json_section(None, &Value::Object(object.clone()))
      .map(|section| vec![section])
      .or_else(|| Some(Vec::new()));
  }
  Some(sections)
}

fn nonempty_string_field<'a>(object: &'a serde_json::Map<String, Value>, field: &str) -> Option<&'a str> {
  object
    .get(field)
    .and_then(Value::as_str)
    .filter(|text| !text.is_empty())
}

fn project_labeled_value(label: &str, value: &Value, depth: usize) -> Vec<ToolOutputSection> {
  let mut sections = project_output(value, depth);
  if sections.len() == 1 && sections[0].label.is_none() {
    sections[0].label = Some(label.to_string());
  }
  sections
}

fn append_distinct_error(sections: &mut Vec<ToolOutputSection>, error: Option<&Value>, depth: usize) {
  let Some(error) = error.filter(|value| !is_effectively_empty(value)) else {
    return;
  };
  for section in project_labeled_value("Error", error, depth) {
    let is_distinct = sections
      .iter()
      .all(|existing| existing.text != section.text && !existing.text.contains(&section.text));
    if is_distinct {
      sections.push(section);
    }
  }
}

fn content_text(value: &Value) -> Option<String> {
  let mut parts = Vec::new();
  collect_content_text(value, 0, &mut parts);
  (!parts.is_empty()).then(|| parts.join("\n"))
}

fn collect_content_text(value: &Value, depth: usize, parts: &mut Vec<String>) {
  const MAX_CONTENT_DEPTH: usize = 16;
  if depth >= MAX_CONTENT_DEPTH {
    return;
  }
  match value {
    Value::String(text) if !text.is_empty() => parts.push(text.clone()),
    Value::Array(values) => {
      for value in values {
        collect_content_text(value, depth + 1, parts);
      }
    }
    Value::Object(object) => {
      if let Some(text) = object
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
      {
        parts.push(text.to_string());
      } else if let Some(content) = object.get("content") {
        collect_content_text(content, depth + 1, parts);
      }
    }
    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
  }
}

fn is_effectively_empty(value: &Value) -> bool {
  match value {
    Value::Null => true,
    Value::String(text) => text.is_empty(),
    Value::Array(values) => values.iter().all(is_effectively_empty),
    Value::Object(object) => object.values().all(is_effectively_empty),
    Value::Bool(_) | Value::Number(_) => false,
  }
}

fn text_section(label: Option<&str>, text: String) -> ToolOutputSection {
  ToolOutputSection {
    label: label.map(str::to_string),
    text,
    format: "text".to_string(),
  }
}

fn json_section(label: Option<&str>, value: &Value) -> Option<ToolOutputSection> {
  if is_effectively_empty(value) {
    return None;
  }
  serde_json::to_string_pretty(value).ok().map(|text| ToolOutputSection {
    label: label.map(str::to_string),
    text,
    format: "json".to_string(),
  })
}

fn bound_tool_output(mut sections: Vec<ToolOutputSection>, source_index: usize) -> ToolOutputPreview {
  let original_size_bytes = sections
    .iter()
    .fold(0usize, |total, section| total.saturating_add(section.text.len()));
  let truncated = original_size_bytes > MAX_TOOL_OUTPUT_BYTES;
  if truncated {
    let budgets = section_budgets(&sections, MAX_TOOL_OUTPUT_BYTES);
    for (section, budget) in sections.iter_mut().zip(budgets) {
      section.text = truncate_utf8_head_tail(&section.text, budget);
    }
    sections.retain(|section| !section.text.is_empty());
  }
  ToolOutputPreview {
    sections,
    truncated,
    original_size_bytes,
    source_event_key: encode_event_key(source_index),
  }
}

fn section_budgets(sections: &[ToolOutputSection], limit: usize) -> Vec<usize> {
  let mut budgets = vec![0; sections.len()];
  let mut pending: Vec<usize> = (0..sections.len()).collect();
  let mut remaining = limit;

  while !pending.is_empty() {
    let fair_share = remaining / pending.len();
    let small: Vec<usize> = pending
      .iter()
      .copied()
      .filter(|index| sections[*index].text.len() <= fair_share)
      .collect();
    if small.is_empty() {
      for (position, index) in pending.iter().copied().enumerate() {
        let budget = fair_share + usize::from(position < remaining % pending.len());
        budgets[index] = budget;
      }
      break;
    }
    for index in &small {
      let size = sections[*index].text.len();
      budgets[*index] = size;
      remaining = remaining.saturating_sub(size);
    }
    pending.retain(|index| !small.contains(index));
  }
  budgets
}

fn truncate_utf8_head_tail(text: &str, limit: usize) -> String {
  if text.len() <= limit {
    return text.to_string();
  }
  if limit <= TOOL_OUTPUT_TRUNCATION_MARKER.len() {
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
      end -= 1;
    }
    return text[..end].to_string();
  }

  let retained = limit - TOOL_OUTPUT_TRUNCATION_MARKER.len();
  let requested_head = retained.div_ceil(2);
  let requested_tail = retained / 2;
  let mut head_end = requested_head;
  while head_end > 0 && !text.is_char_boundary(head_end) {
    head_end -= 1;
  }
  let mut tail_start = text.len().saturating_sub(requested_tail);
  while tail_start < text.len() && !text.is_char_boundary(tail_start) {
    tail_start += 1;
  }
  format!(
    "{}{}{}",
    &text[..head_end],
    TOOL_OUTPUT_TRUNCATION_MARKER,
    &text[tail_start..]
  )
}

fn native_detail(event: &AgentEvent) -> Option<Value> {
  match event {
    AgentEvent::SessionSettingsApplied(event) => event.native.clone(),
    AgentEvent::QuestionRequest(event) => Some(event.native.clone()),
    AgentEvent::QuestionReply(event) => Some(event.native.clone()),
    AgentEvent::Message(event) => event.provenance.as_ref().and_then(|value| value.native.clone()),
    AgentEvent::Reasoning(event) => event.provenance.as_ref().and_then(|value| value.native.clone()),
    AgentEvent::AgentActivity(event) => event.native.clone(),
    AgentEvent::Lifecycle(event) => Some(event.native.clone()),
    AgentEvent::Usage(event) => Some(event.native.clone()),
    AgentEvent::Metadata(event) => Some(event.native.clone()),
    AgentEvent::Unknown(event) => event.native.clone(),
    AgentEvent::SessionStarted(_)
    | AgentEvent::ProviderChanged(_)
    | AgentEvent::GoalUpdated(_)
    | AgentEvent::ToolCall(_)
    | AgentEvent::Compaction(_)
    | AgentEvent::Error(_) => None,
  }
}

fn remove_embedded_native(event: &mut Value) {
  let Some(event) = event.as_object_mut() else {
    return;
  };
  event.remove("native");
  if let Some(provenance) = event.get_mut("provenance").and_then(Value::as_object_mut) {
    provenance.remove("native");
  }
}

fn bounded_detail_value(value: Value, representation: &'static str) -> Result<Value, String> {
  bounded_detail_value_with_limit(value, representation, MAX_DETAIL_VALUE_BYTES)
}

fn bounded_detail_value_with_limit(
  value: Value,
  representation: &'static str,
  limit_bytes: usize,
) -> Result<Value, String> {
  let original_size_bytes = serde_json::to_vec(&value)
    .map_err(|error| format!("failed to size {representation} detail: {error}"))?
    .len();
  if original_size_bytes <= limit_bytes {
    return Ok(value);
  }
  Ok(json!({
    "truncated": true,
    "original_size_bytes": original_size_bytes,
    "limit_bytes": limit_bytes,
    "representation": representation,
  }))
}

fn truncate(value: String, max_chars: usize) -> String {
  truncate_with_flag(value, max_chars).0
}

fn truncate_with_flag(value: String, max_chars: usize) -> (String, bool) {
  if value.chars().count() <= max_chars {
    return (value, false);
  }
  let mut truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
  truncated.push('…');
  (truncated, true)
}

#[cfg(test)]
mod tests {
  mod communications;
  mod event_filter;
  mod history_cache;
  mod questions;
  mod targeted_index;
  mod usage_filter;
  use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
  use std::path::PathBuf;
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicUsize, Ordering};

  use serde_json::json;
  use tokn_session_core::{
    AgentActivity, ErrorEvent, LifecycleEvent, LifecycleScope, MessageDelivery, MessageEvent, MessageProvenance,
    MetadataEvent, MetadataKind, Phase, ProviderChanged, ReasoningEvent, SessionHistoryStatus, SessionRef,
    SessionSettingsApplied, SessionStarted, ToolCallEvent, ToolKind, ToolRecordKind, ToolTransport, UnknownEvent,
    UsageEvent, UsageKind,
  };

  use super::*;
  use crate::model::{SessionQuery, decode_event_cursor, decode_session_key};

  struct FakeRepository {
    listings: HashMap<ViewerProvider, Result<Vec<SessionHeader>, String>>,
    loaded: Mutex<Option<LoadedSession>>,
  }

  impl ViewerRepository for FakeRepository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      self.listings.get(&provider).cloned().unwrap_or_else(|| Ok(Vec::new()))
    }

    fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
      self
        .loaded
        .lock()
        .expect("fixture lock should not be poisoned")
        .take()
        .ok_or_else(|| "fixture session already loaded".to_string())
    }
  }

  struct SequenceRepository {
    loaded: Mutex<VecDeque<LoadedSession>>,
  }

  impl ViewerRepository for SequenceRepository {
    fn list_session_headers(&self, _provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      Ok(Vec::new())
    }

    fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
      self
        .loaded
        .lock()
        .expect("fixture lock should not be poisoned")
        .pop_front()
        .ok_or_else(|| "fixture session sequence is exhausted".to_string())
    }
  }

  #[derive(Clone)]
  struct IndexedMessageSpec {
    role: Role,
    delivery: MessageDelivery,
    hidden: bool,
  }

  #[derive(Clone)]
  struct IndexedLoadSpec {
    header: SessionHeader,
    messages: Vec<IndexedMessageSpec>,
  }

  impl IndexedLoadSpec {
    fn load(&self) -> LoadedSession {
      LoadedSession {
        reference: SessionRef {
          id: self.header.id.clone(),
          parent_session_id: self.header.parent_session_id.clone(),
          agent_path: self.header.agent_path.clone(),
          agent_nickname: self.header.agent_nickname.clone(),
          agent_role: self.header.agent_role.clone(),
          title: self.header.title.clone(),
          preview: self.header.preview.clone(),
          path: self.header.path.clone(),
          cwd: self.header.cwd.clone(),
          timestamp: self.header.timestamp.clone(),
          message_count: self.messages.len(),
        },
        events: self
          .messages
          .iter()
          .enumerate()
          .map(|(index, message)| {
            AgentEvent::Message(MessageEvent {
              provenance: message.hidden.then(|| MessageProvenance {
                source: json!({"fixture": true}),
                display: Some(false),
                native: None,
                surface_op: None,
                source_event_seqs: None,
              }),
              provider: Provider::Codex,
              session_id: Some(self.header.id.clone()),
              message_id: Some(format!("{}-{index}", self.header.id)),
              parent_id: None,
              role: message.role,
              delivery: message.delivery,
              phase: Phase::Finished,
              text: "fixture message".to_string(),
              timestamp: self.header.timestamp.clone(),
            })
          })
          .collect(),
        history_status: SessionHistoryStatus::Complete,
      }
    }
  }

  struct IndexingRepository {
    listings: Mutex<HashMap<ViewerProvider, Vec<SessionHeader>>>,
    targeted_headers: Mutex<HashMap<(ViewerProvider, PathBuf), Result<SessionHeader, String>>>,
    loads: Mutex<HashMap<SessionLocator, Result<IndexedLoadSpec, String>>>,
    mutate_source_on_next_load: Mutex<Option<PathBuf>>,
    mutate_source_on_next_targeted_header: Mutex<Option<PathBuf>>,
    header_calls: AtomicUsize,
    header_calls_by_provider: Mutex<HashMap<ViewerProvider, usize>>,
    targeted_header_calls: AtomicUsize,
    load_calls: AtomicUsize,
    load_order: Mutex<Vec<SessionLocator>>,
  }

  impl ViewerRepository for IndexingRepository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      self.header_calls.fetch_add(1, Ordering::SeqCst);
      *self
        .header_calls_by_provider
        .lock()
        .expect("fixture header-call lock should not be poisoned")
        .entry(provider)
        .or_default() += 1;
      Ok(
        self
          .listings
          .lock()
          .expect("fixture listings lock should not be poisoned")
          .get(&provider)
          .cloned()
          .unwrap_or_default(),
      )
    }

    fn session_header_at_path(&self, provider: ViewerProvider, path: &Path) -> Result<SessionHeader, String> {
      self.targeted_header_calls.fetch_add(1, Ordering::SeqCst);
      if let Some(path) = self
        .mutate_source_on_next_targeted_header
        .lock()
        .expect("fixture targeted-header mutation lock should not be poisoned")
        .take()
      {
        std::fs::write(path, "fixture source changed during targeted header read")
          .expect("fixture source mutation should succeed");
      }
      self
        .targeted_headers
        .lock()
        .expect("fixture targeted headers lock should not be poisoned")
        .get(&(provider, path.to_path_buf()))
        .cloned()
        .ok_or_else(|| "fixture targeted header is not configured".to_string())?
    }

    fn load_session(&self, locator: &SessionLocator) -> Result<LoadedSession, String> {
      self.load_calls.fetch_add(1, Ordering::SeqCst);
      self
        .load_order
        .lock()
        .expect("fixture load order lock should not be poisoned")
        .push(locator.clone());
      if let Some(path) = self
        .mutate_source_on_next_load
        .lock()
        .expect("fixture source mutation lock should not be poisoned")
        .take()
      {
        std::fs::write(path, "fixture source changed during body load")
          .expect("fixture source mutation should succeed");
      }
      let spec = self
        .loads
        .lock()
        .expect("fixture loads lock should not be poisoned")
        .get(locator)
        .cloned()
        .ok_or_else(|| "fixture session is not configured".to_string())??;
      Ok(spec.load())
    }
  }

  struct WarmFileRepository {
    header: SessionHeader,
    header_calls: AtomicUsize,
    targeted_header_calls: AtomicUsize,
  }

  impl ViewerRepository for WarmFileRepository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      self.header_calls.fetch_add(1, Ordering::SeqCst);
      Ok(
        (provider == ViewerProvider::Codex)
          .then(|| self.header.clone())
          .into_iter()
          .collect(),
      )
    }

    fn file_session_paths(&self, provider: ViewerProvider) -> Result<Vec<PathBuf>, String> {
      Ok(
        (provider == ViewerProvider::Codex)
          .then(|| self.header.path.clone())
          .into_iter()
          .collect(),
      )
    }

    fn session_header_at_path(&self, _provider: ViewerProvider, _path: &Path) -> Result<SessionHeader, String> {
      self.targeted_header_calls.fetch_add(1, Ordering::SeqCst);
      Ok(self.header.clone())
    }

    fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
      Err("warm catalog fixture does not load bodies".to_string())
    }
  }

  struct CatalogSequenceRepository {
    codex_catalogs: Vec<Vec<SessionHeader>>,
    codex_header_calls: AtomicUsize,
    mutate_path_after_first_catalog: Option<PathBuf>,
    remove_path_after_first_catalog: Option<PathBuf>,
  }

  impl ViewerRepository for CatalogSequenceRepository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      if provider != ViewerProvider::Codex {
        return Ok(Vec::new());
      }
      let call = self.codex_header_calls.fetch_add(1, Ordering::SeqCst);
      if call == 1
        && let Some(path) = &self.mutate_path_after_first_catalog
      {
        std::fs::write(path, "fixture source changed during catalog confirmation")
          .expect("fixture source mutation should succeed");
      }
      if call == 1
        && let Some(path) = &self.remove_path_after_first_catalog
      {
        std::fs::remove_file(path).expect("fixture source removal should succeed");
      }
      self
        .codex_catalogs
        .get(call.min(self.codex_catalogs.len().saturating_sub(1)))
        .cloned()
        .ok_or_else(|| "catalog sequence requires at least one Codex listing".to_string())
    }

    fn load_session(&self, locator: &SessionLocator) -> Result<LoadedSession, String> {
      let header = self
        .codex_catalogs
        .iter()
        .flatten()
        .find(|header| header.id == locator.session_id && header.path == locator.source_path)
        .cloned()
        .ok_or_else(|| "fixture session is not configured".to_string())?;
      Ok(
        IndexedLoadSpec {
          header,
          messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
        }
        .load(),
      )
    }
  }

  struct BlockingCatalogRepository {
    header: SessionHeader,
    codex_header_calls: AtomicUsize,
    confirmation_reached: std::sync::mpsc::Sender<()>,
    resume_confirmation: Mutex<std::sync::mpsc::Receiver<()>>,
  }

  impl ViewerRepository for BlockingCatalogRepository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      if provider != ViewerProvider::Codex {
        return Ok(Vec::new());
      }
      let call = self.codex_header_calls.fetch_add(1, Ordering::SeqCst);
      if call == 1 {
        self
          .confirmation_reached
          .send(())
          .map_err(|_| "catalog test no longer has a waiting coordinator".to_owned())?;
        self
          .resume_confirmation
          .lock()
          .expect("catalog test resume lock should not be poisoned")
          .recv()
          .map_err(|_| "catalog test coordinator stopped before confirmation".to_owned())?;
      }
      Ok(vec![self.header.clone()])
    }

    fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
      Err("catalog-only test must not load a session body".to_owned())
    }
  }

  struct BlockingBodyRepository {
    header: SessionHeader,
    loaded: IndexedLoadSpec,
    load_started: std::sync::mpsc::Sender<()>,
    resume_load: Mutex<std::sync::mpsc::Receiver<()>>,
  }

  impl ViewerRepository for BlockingBodyRepository {
    fn list_session_headers(&self, provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      if provider == ViewerProvider::Codex {
        Ok(vec![self.header.clone()])
      } else {
        Ok(Vec::new())
      }
    }

    fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
      self
        .load_started
        .send(())
        .map_err(|_| "body test no longer has a waiting coordinator".to_owned())?;
      self
        .resume_load
        .lock()
        .expect("body test resume lock should not be poisoned")
        .recv()
        .map_err(|_| "body test coordinator stopped before resuming the load".to_owned())?;
      Ok(self.loaded.load())
    }
  }

  fn indexing_repository(specs: Vec<IndexedLoadSpec>) -> Arc<IndexingRepository> {
    indexing_repository_for(ViewerProvider::Codex, specs)
  }

  fn indexing_repository_for(provider: ViewerProvider, specs: Vec<IndexedLoadSpec>) -> Arc<IndexingRepository> {
    let headers = specs.iter().map(|spec| spec.header.clone()).collect::<Vec<_>>();
    let targeted_headers = specs
      .iter()
      .map(|spec| ((provider, spec.header.path.clone()), Ok(spec.header.clone())))
      .collect();
    let loads = specs
      .into_iter()
      .map(|spec| (locator_for_header(provider, &spec.header), Ok(spec)))
      .collect();
    Arc::new(IndexingRepository {
      listings: Mutex::new(HashMap::from([(provider, headers)])),
      targeted_headers: Mutex::new(targeted_headers),
      loads: Mutex::new(loads),
      mutate_source_on_next_load: Mutex::new(None),
      mutate_source_on_next_targeted_header: Mutex::new(None),
      header_calls: AtomicUsize::new(0),
      header_calls_by_provider: Mutex::new(HashMap::new()),
      targeted_header_calls: AtomicUsize::new(0),
      load_calls: AtomicUsize::new(0),
      load_order: Mutex::new(Vec::new()),
    })
  }

  fn indexed_header(path: PathBuf, id: &str, parent: Option<&str>) -> SessionHeader {
    let mut header = session_header(id, parent, "/projects/indexed", "2026-09-01T00:00:00Z");
    header.path = path;
    header.title = Some(format!("Indexed {id}"));
    header
  }

  fn indexed_message(role: Role, delivery: MessageDelivery) -> IndexedMessageSpec {
    IndexedMessageSpec {
      role,
      delivery,
      hidden: false,
    }
  }

  fn indexed_attention_session(marker: Option<&str>, present: bool) -> IndexedSession {
    IndexedSession {
      key: IndexedSessionKey::new("codex", "path.v1.fixture", "fixture"),
      source_path: "/tmp/fixture.jsonl".to_string(),
      title: None,
      preview: None,
      catalog_title: None,
      catalog_preview: None,
      body_title: None,
      body_preview: None,
      cwd: None,
      timestamp: None,
      updated_at: None,
      updated_at_ms: None,
      parent_session_id: None,
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      attention_marker: marker.map(str::to_owned),
      attention_baselined: true,
      notify_on_baseline: false,
      attention_revision: 0,
      seen_attention_revision: 0,
      seen_at_ms: None,
      present,
    }
  }

  #[test]
  fn attention_requires_new_visible_messages_even_after_a_source_reappears() {
    let tombstoned = indexed_attention_session(Some("final-replies.v2.2.0"), false);

    assert!(!has_new_visible_attention(
      Some(&tombstoned),
      Some("final-replies.v2.2.0")
    ));
    assert!(has_new_visible_attention(
      Some(&tombstoned),
      Some("final-replies.v2.3.0")
    ));
    assert!(!has_new_visible_attention(Some(&tombstoned), None));
  }

  #[test]
  fn final_reply_accounting_ignores_state_changes_and_quietly_upgrades_old_markers() {
    let indexed = indexed_attention_session(Some("final-replies.v2.2.0"), true);
    assert_eq!(
      body_new_attention_count(&indexed, None, Some("final-replies.v2.5.0")),
      3
    );
    assert_eq!(
      body_new_attention_count(&indexed, None, Some("final-replies.v2.2.1")),
      0
    );
    assert_eq!(
      body_new_attention_count(&indexed, None, Some("final-replies.v2.1.0")),
      0
    );
    let legacy = indexed_attention_session(Some("visible-message-count.v1.50"), true);
    assert!(needs_activity_upgrade(&legacy));
    assert_eq!(
      body_new_attention_count(&legacy, None, Some("final-replies.v2.25.0")),
      0
    );
    let mut newly_discovered = indexed_attention_session(None, true);
    newly_discovered.attention_baselined = false;
    newly_discovered.notify_on_baseline = true;
    assert_eq!(
      body_new_attention_count(&newly_discovered, None, Some("final-replies.v2.3.0")),
      3
    );
  }

  #[test]
  fn project_order_precedes_pagination_and_survives_search() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("new");
    let checkout = directory.path().join("worktree");
    let registration = root.join(".git/worktrees/task");
    std::fs::create_dir_all(&registration).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(checkout.join(".git"), format!("gitdir: {}", registration.display())).unwrap();
    std::fs::write(registration.join("commondir"), "../..").unwrap();
    let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
    let checkout = checkout.to_string_lossy().into_owned();
    let mut specs = Vec::new();
    for (id, cwd, activity) in [
      ("a", "/old", 300),
      ("b", root.as_str(), 100),
      ("c", checkout.as_str(), 200),
    ] {
      let path = directory.path().join(format!("{id}.jsonl"));
      std::fs::write(&path, id).unwrap();
      let mut header = indexed_header(path, id, None);
      header.cwd = Some(cwd.into());
      header.updated_at_ms = Some(activity);
      specs.push(IndexedLoadSpec {
        header,
        messages: vec![],
      });
    }
    let index = Arc::new(SessionIndex::open_in_memory().unwrap());
    index.project_order(&["/old".into()], 10).unwrap();
    index.project_order(&[root.clone()], 20).unwrap();
    let service = ViewerService::new_with_index(indexing_repository(specs), index);
    service.refresh_session_index().unwrap();
    let request = |cursor, search| ListSessionsRequest {
      query: SessionQuery {
        order: SessionOrder::Project,
        providers: vec![ViewerProvider::Codex],
        search,
      },
      cursor,
      offset: None,
      limit: Some(1),
    };
    let first = service.list_sessions(request(None, None)).unwrap();
    assert_eq!(first.sessions[0].session_id, "c");
    assert_eq!(first.sessions[0].project_order_ms, Some(20));
    let second = service.list_sessions(request(first.next_cursor, None)).unwrap();
    assert_eq!(second.sessions[0].session_id, "b");
    assert_eq!(first.sessions[0].project_key.as_deref(), Some(root.as_str()));
    assert_eq!(first.sessions[0].project_key, second.sessions[0].project_key);
    assert_eq!(first.sessions[0].cwd.as_deref(), Some(checkout.as_str()));
    let third = service.list_sessions(request(second.next_cursor, None)).unwrap();
    assert_eq!(third.sessions[0].session_id, "a");
    let filtered = service.list_sessions(request(None, Some("Indexed a".into()))).unwrap();
    assert_eq!(filtered.sessions[0].project_order_ms, Some(10));
  }

  #[test]
  fn session_index_catalogs_every_header_then_backfills_newest_bodies_in_batches() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let mut specs = Vec::new();
    for index in 0..=INDEX_BODY_SCAN_BATCH_SIZE {
      let id = format!("session-{index}");
      let path = directory.path().join(format!("{id}.jsonl"));
      std::fs::write(&path, format!("fixture {index}")).expect("fixture source should be written");
      let mut header = indexed_header(path, &id, None);
      header.updated_at = Some(format!("2026-09-01T00:00:{index:02}Z"));
      header.updated_at_ms = Some(i64::try_from(index).expect("small fixture index should fit"));
      specs.push(IndexedLoadSpec {
        header,
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      });
    }
    let repository = indexing_repository(specs);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));

    let first_refresh = service
      .refresh_session_index()
      .expect("catalog and first batch should refresh");
    assert!(first_refresh.changed);
    assert!(first_refresh.has_pending_body_jobs);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), INDEX_BODY_SCAN_BATCH_SIZE);
    assert_eq!(
      repository.header_calls.load(Ordering::SeqCst),
      ViewerProvider::ALL.len() * 2,
      "each body job should reuse the committed catalog row instead of rediscovering every provider header"
    );
    let first_loads = repository
      .load_order
      .lock()
      .expect("fixture load order lock should not be poisoned")
      .iter()
      .map(|locator| locator.session_id.clone())
      .collect::<Vec<_>>();
    assert_eq!(
      first_loads,
      (1..=INDEX_BODY_SCAN_BATCH_SIZE)
        .rev()
        .map(|index| format!("session-{index}"))
        .collect::<Vec<_>>(),
      "newer provider update timestamps should receive body inspection first"
    );

    let cataloged = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: Some(100),
      })
      .expect("cataloged listing should work");
    assert_eq!(cataloged.sessions.len(), INDEX_BODY_SCAN_BATCH_SIZE + 1);
    assert!(cataloged.sessions.iter().all(|session| !session.has_unread));
    let oldest_locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "session-0".to_string(),
      source_path: directory.path().join("session-0.jsonl"),
    };
    assert!(
      !index
        .session(&index_session_key(&oldest_locator).expect("index key should encode"))
        .expect("index query should work")
        .expect("oldest catalog row should exist")
        .attention_baselined,
      "the sidebar should expose the old row before its body is parsed"
    );

    let second_refresh = service
      .refresh_pending_session_index()
      .expect("second body-only batch should refresh");
    assert!(!second_refresh.has_pending_body_jobs);
    assert_eq!(
      repository.load_calls.load(Ordering::SeqCst),
      INDEX_BODY_SCAN_BATCH_SIZE + 1
    );
    assert_eq!(
      repository.header_calls.load(Ordering::SeqCst),
      ViewerProvider::ALL.len() * 2,
      "a one-second body-only pass must not rediscover the catalog"
    );
    assert!(
      index
        .session(&index_session_key(&oldest_locator).expect("index key should encode"))
        .expect("index query should work")
        .expect("oldest catalog row should exist")
        .attention_baselined
    );
  }

  #[test]
  fn provider_local_catalog_skips_unrelated_provider_discovery() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("opencode.db");
    std::fs::write(&path, "fixture database").expect("fixture source should be written");
    let repository = indexing_repository_for(
      ViewerProvider::OpenCode,
      vec![IndexedLoadSpec {
        header: indexed_header(path, "opencode-active", None),
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      }],
    );
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), index);

    let refresh = service
      .refresh_session_catalog_providers(&[ViewerProvider::OpenCode])
      .expect("provider-local catalog should refresh");

    assert!(refresh.changed);
    assert_eq!(
      *repository
        .header_calls_by_provider
        .lock()
        .expect("fixture header-call lock should not be poisoned"),
      HashMap::from([(ViewerProvider::OpenCode, 2)]),
      "the selected provider needs one inventory and one stable confirmation only"
    );
    let progress = service.session_index_progress();
    assert_eq!(progress.catalog.scope, CatalogRefreshScope::Full);
    assert_eq!(progress.catalog.total_providers, 1);
    assert_eq!(progress.catalog.processed_providers, 1);
  }

  #[test]
  fn warm_file_catalog_reuses_unchanged_indexed_headers() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("stable.jsonl");
    std::fs::write(&path, "stable fixture source").expect("fixture source should be written");
    let repository = Arc::new(WarmFileRepository {
      header: indexed_header(path, "stable", None),
      header_calls: AtomicUsize::new(0),
      targeted_header_calls: AtomicUsize::new(0),
    });
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), index);

    service
      .refresh_session_catalog_providers(&[ViewerProvider::Codex])
      .expect("cold catalog should establish the provider baseline");
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), 2);

    service
      .refresh_session_catalog_providers(&[ViewerProvider::Codex])
      .expect("warm catalog should reconcile from file revisions");

    assert_eq!(
      repository.header_calls.load(Ordering::SeqCst),
      2,
      "an unchanged warm catalog should not enumerate and parse every provider header again"
    );
    assert_eq!(repository.targeted_header_calls.load(Ordering::SeqCst), 0);
  }

  #[test]
  fn targeted_file_catalog_updates_one_known_source_without_a_provider_rescan() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("active.jsonl");
    std::fs::write(&path, "initial fixture source").expect("fixture source should be written");
    let initial_header = indexed_header(path.clone(), "active", None);
    let initial_spec = IndexedLoadSpec {
      header: initial_header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    };
    let repository = indexing_repository(vec![initial_spec]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));

    service
      .refresh_session_catalog()
      .expect("initial catalog should establish the provider baseline");
    service
      .refresh_pending_session_index()
      .expect("initial body should establish a quiet attention baseline");
    let header_calls_before = repository.header_calls.load(Ordering::SeqCst);

    std::fs::write(&path, "initial fixture source with a newly appended turn").expect("fixture source should change");
    let mut updated_header = initial_header.clone();
    updated_header.title = Some("Updated active session".to_string());
    repository
      .targeted_headers
      .lock()
      .expect("fixture targeted headers lock should not be poisoned")
      .insert((ViewerProvider::Codex, path.clone()), Ok(updated_header.clone()));
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        locator_for_header(ViewerProvider::Codex, &updated_header),
        Ok(IndexedLoadSpec {
          header: updated_header.clone(),
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );

    let refresh = service
      .refresh_changed_file_catalogs(BTreeMap::from([(
        ViewerProvider::Codex,
        BTreeSet::from([path.clone()]),
      )]))
      .expect("known source should use the targeted catalog path");

    assert!(refresh.changed);
    assert!(!refresh.retry_catalog_soon);
    assert!(refresh.retry_changed_file_paths.is_empty());
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), header_calls_before);
    assert_eq!(repository.targeted_header_calls.load(Ordering::SeqCst), 1);
    let progress = service.session_index_progress();
    assert_eq!(progress.catalog.scope, CatalogRefreshScope::Targeted);
    assert_eq!(progress.catalog.total_providers, 1);
    assert_eq!(progress.catalog.processed_providers, 1);

    let source_key = index_source_key_for_path(ViewerProvider::Codex, &path).expect("fixture path should index");
    let session_key = IndexedSessionKey::new("codex", source_key.source_key.clone(), "active");
    let staged = index
      .session(&session_key)
      .expect("staged session should be readable")
      .expect("active session should remain present");
    assert!(!staged.attention_baselined);
    assert_eq!(staged.catalog_title.as_deref(), Some("Updated active session"));

    let body_refresh = service
      .refresh_pending_session_index()
      .expect("targeted source should enter the existing bounded body queue");
    assert_eq!(body_refresh.attention_session_keys.len(), 1);
    assert!(
      !body_refresh.catalog_refresh_required,
      "attention-only completion is covered by notifications"
    );
    let completed = index
      .session(&session_key)
      .expect("completed session should be readable")
      .expect("active session should remain present");
    assert!(completed.attention_baselined);
    assert!(completed.has_unread());
  }

  #[test]
  fn targeted_file_catalog_retains_catalog_presentation_missing_from_a_direct_header() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("active.jsonl");
    std::fs::write(&path, "initial fixture source").expect("fixture source should be written");
    let mut initial_header = indexed_header(path.clone(), "active", None);
    initial_header.title = Some("Desktop catalog title".to_string());
    initial_header.preview = Some("Desktop catalog preview".to_string());
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: initial_header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));

    service
      .refresh_session_catalog()
      .expect("initial catalog should establish the provider baseline");
    let header_calls_before = repository.header_calls.load(Ordering::SeqCst);

    std::fs::write(&path, "initial fixture source with an appended turn").expect("fixture source should change");
    let mut raw_direct_header = initial_header.clone();
    raw_direct_header.title = None;
    raw_direct_header.preview = None;
    repository
      .targeted_headers
      .lock()
      .expect("fixture targeted headers lock should not be poisoned")
      .insert((ViewerProvider::Codex, path.clone()), Ok(raw_direct_header));

    let refresh = service
      .refresh_changed_file_catalogs(BTreeMap::from([(
        ViewerProvider::Codex,
        BTreeSet::from([path.clone()]),
      )]))
      .expect("known source should use the targeted catalog path");

    assert!(refresh.changed);
    assert!(!refresh.retry_catalog_soon);
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), header_calls_before);
    let source_key = index_source_key_for_path(ViewerProvider::Codex, &path).expect("fixture path should index");
    let session = index
      .session(&IndexedSessionKey::new("codex", source_key.source_key, "active"))
      .expect("staged session should be readable")
      .expect("active session should remain present");
    assert_eq!(session.catalog_title.as_deref(), Some("Desktop catalog title"));
    assert_eq!(session.catalog_preview.as_deref(), Some("Desktop catalog preview"));
    assert_eq!(session.title.as_deref(), Some("Desktop catalog title"));
    assert_eq!(session.preview.as_deref(), Some("Desktop catalog preview"));
  }

  #[test]
  fn targeted_file_catalog_escalates_unknown_paths_without_replacing_known_rows() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let known_path = directory.path().join("known.jsonl");
    let unknown_path = directory.path().join("new.jsonl");
    std::fs::write(&known_path, "known fixture source").expect("known fixture should be written");
    std::fs::write(&unknown_path, "unknown fixture source").expect("unknown fixture should be written");
    let header = indexed_header(known_path.clone(), "known", None);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    service
      .refresh_session_catalog()
      .expect("initial catalog should establish the provider baseline");
    let header_calls_before = repository.header_calls.load(Ordering::SeqCst);
    let source_key = index_source_key_for_path(ViewerProvider::Codex, &known_path).expect("known path should index");
    let before = index
      .source_state(&source_key)
      .expect("known source should be readable")
      .expect("known source should be indexed");

    let refresh = service
      .refresh_changed_file_catalogs(BTreeMap::from([(
        ViewerProvider::Codex,
        BTreeSet::from([unknown_path]),
      )]))
      .expect("unknown path should request a safe full catalog rather than fail the worker");

    assert!(!refresh.changed);
    assert!(refresh.retry_catalog_soon);
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), header_calls_before);
    assert_eq!(repository.targeted_header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
      index
        .source_state(&source_key)
        .expect("known source should remain readable"),
      Some(before)
    );
  }

  #[test]
  fn targeted_file_catalog_retries_a_cursor_race_without_a_full_catalog() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("racing.jsonl");
    std::fs::write(&path, "initial fixture source").expect("fixture source should be written");
    let header = indexed_header(path.clone(), "racing", None);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    service
      .refresh_session_catalog()
      .expect("initial catalog should establish the provider baseline");
    let source_key = index_source_key_for_path(ViewerProvider::Codex, &path).expect("fixture path should index");
    let before = index
      .source_state(&source_key)
      .expect("source should be readable")
      .expect("source should be indexed");
    let header_calls_before = repository.header_calls.load(Ordering::SeqCst);
    *repository
      .mutate_source_on_next_targeted_header
      .lock()
      .expect("fixture targeted-header mutation lock should not be poisoned") = Some(path.clone());
    std::fs::write(&path, "fixture append before the racing header read").unwrap();

    let refresh = service
      .refresh_changed_file_catalogs(BTreeMap::from([(
        ViewerProvider::Codex,
        BTreeSet::from([path.clone()]),
      )]))
      .expect("cursor race should leave the known source for a bounded retry");

    assert!(!refresh.changed);
    assert!(!refresh.retry_catalog_soon);
    assert_eq!(
      refresh.retry_changed_file_paths,
      BTreeMap::from([(ViewerProvider::Codex, BTreeSet::from([path]))])
    );
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), header_calls_before);
    assert_eq!(repository.targeted_header_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
      index.source_state(&source_key).expect("source should remain readable"),
      Some(before)
    );
  }

  #[test]
  fn session_index_ignores_mutable_header_changes_between_catalog_passes() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("active.jsonl");
    std::fs::write(&path, "fixture source").expect("fixture source should be written");
    let before = indexed_header(path.clone(), "active", None);
    let mut after = before.clone();
    after.title = Some("Title assigned while indexing".to_string());
    after.preview = Some("Preview assigned while indexing".to_string());
    after.updated_at = Some("2026-09-01T00:00:01Z".to_string());
    after.updated_at_ms = Some(1);
    let repository = Arc::new(CatalogSequenceRepository {
      codex_catalogs: vec![vec![before.clone()], vec![after]],
      codex_header_calls: AtomicUsize::new(0),
      mutate_path_after_first_catalog: None,
      remove_path_after_first_catalog: None,
    });
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository, Arc::clone(&index));

    let refresh = service
      .refresh_session_index()
      .expect("catalog should tolerate metadata churn");
    assert!(!refresh.retry_catalog_soon);
    assert!(service.index_error_for(ViewerProvider::Codex).is_none());
    let locator = locator_for_header(ViewerProvider::Codex, &before);
    let stored = index
      .session(&index_session_key(&locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("stable session should be cataloged");
    assert_eq!(stored.title.as_deref(), before.title.as_deref());
    assert!(stored.attention_baselined);
  }

  #[test]
  fn concurrent_same_cursor_catalog_update_retries_quietly_without_overwriting_the_winner() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("active.jsonl");
    std::fs::write(&path, "unchanged source bytes").expect("fixture source should be written");
    let initial_header = indexed_header(path.clone(), "concurrent-catalog", None);
    let mut stale_header = initial_header.clone();
    stale_header.title = Some("stale catalog title".to_owned());
    stale_header.parent_session_id = Some("stale-parent".to_owned());
    let mut winning_header = initial_header.clone();
    winning_header.title = Some("winning catalog title".to_owned());
    winning_header.parent_session_id = Some("winning-parent".to_owned());

    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let initial_service = ViewerService::new_with_index(
      indexing_repository(vec![IndexedLoadSpec {
        header: initial_header.clone(),
        messages: Vec::new(),
      }]),
      Arc::clone(&index),
    );
    initial_service
      .refresh_provider_catalog(ViewerProvider::Codex)
      .expect("initial catalog should commit");

    let (confirmation_reached, confirmation_waiter) = std::sync::mpsc::channel();
    let (resume_sender, resume_confirmation) = std::sync::mpsc::channel();
    let stale_service = ViewerService::new_with_index(
      Arc::new(BlockingCatalogRepository {
        header: stale_header,
        codex_header_calls: AtomicUsize::new(0),
        confirmation_reached,
        resume_confirmation: Mutex::new(resume_confirmation),
      }),
      Arc::clone(&index),
    );
    let stale_refresh_service = stale_service.clone();
    let stale_refresh = std::thread::spawn(move || stale_refresh_service.refresh_session_catalog());
    confirmation_waiter
      .recv_timeout(std::time::Duration::from_secs(1))
      .expect("stale catalog should pause before its confirmation read");

    let winning_service = ViewerService::new_with_index(
      indexing_repository(vec![IndexedLoadSpec {
        header: winning_header,
        messages: Vec::new(),
      }]),
      Arc::clone(&index),
    );
    assert!(
      winning_service
        .refresh_session_catalog()
        .expect("winning catalog should refresh")
        .changed
    );
    resume_sender
      .send(())
      .expect("stale catalog confirmation should resume");
    let stale_refresh = stale_refresh
      .join()
      .expect("stale catalog thread should not panic")
      .expect("same-cursor conflict should be a quiet retry");
    assert!(
      stale_refresh.changed,
      "the loser must ask its sidebar to reread the winner's shared index snapshot"
    );
    assert!(stale_refresh.retry_catalog_soon);
    assert!(
      stale_service.index_error_for(ViewerProvider::Codex).is_none(),
      "an optimistic collision is not a provider read error"
    );

    let locator = locator_for_header(ViewerProvider::Codex, &initial_header);
    let stored = index
      .session(&index_session_key(&locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("winning row should remain indexed");
    assert_eq!(stored.catalog_title.as_deref(), Some("winning catalog title"));
    assert_eq!(stored.parent_session_id.as_deref(), Some("winning-parent"));
  }

  #[test]
  fn concurrent_body_completion_requests_a_sidebar_reread_for_the_other_process() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("concurrent-body.jsonl");
    std::fs::write(&path, "stable source bytes").expect("fixture source should be written");
    let mut header = indexed_header(path, "concurrent-body", None);
    header.title = None;
    let locator = locator_for_header(ViewerProvider::Codex, &header);
    let mut loaded_header = header.clone();
    loaded_header.title = Some("winner body title".to_owned());
    let body_spec = IndexedLoadSpec {
      header: loaded_header,
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    };
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let catalog_service = ViewerService::new_with_index(
      indexing_repository(vec![IndexedLoadSpec {
        header: header.clone(),
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      }]),
      Arc::clone(&index),
    );
    catalog_service
      .refresh_provider_catalog(ViewerProvider::Codex)
      .expect("catalog should stage the body job");

    let (load_started, load_waiter) = std::sync::mpsc::channel();
    let (resume_sender, resume_load) = std::sync::mpsc::channel();
    let stale_service = ViewerService::new_with_index(
      Arc::new(BlockingBodyRepository {
        header: header.clone(),
        loaded: body_spec.clone(),
        load_started,
        resume_load: Mutex::new(resume_load),
      }),
      Arc::clone(&index),
    );
    let stale_body_service = stale_service.clone();
    let stale_body_refresh = std::thread::spawn(move || stale_body_service.refresh_pending_session_index());
    load_waiter
      .recv_timeout(std::time::Duration::from_secs(1))
      .expect("first process should pause during its body load");

    let winner_service = ViewerService::new_with_index(indexing_repository(vec![body_spec]), Arc::clone(&index));
    assert!(
      winner_service
        .refresh_pending_session_index()
        .expect("winning body completion should commit")
        .changed
    );
    resume_sender.send(()).expect("stale body load should resume");
    let stale_refresh = stale_body_refresh
      .join()
      .expect("stale body thread should not panic")
      .expect("stale body completion should stay quiet");
    assert!(
      stale_refresh.changed,
      "the stale process must refresh its sidebar from the winner's shared index commit"
    );

    let stored = index
      .session(&index_session_key(&locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("winner session should remain indexed");
    assert_eq!(stored.title.as_deref(), Some("winner body title"));
  }

  #[test]
  fn external_commit_after_the_initial_sidebar_read_requests_a_reread_on_the_first_catalog_pass() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let source_path = directory.path().join("external-commit.jsonl");
    std::fs::write(&source_path, "stable source bytes").expect("fixture source should be written");
    let index_path = directory.path().join("session-index.sqlite");
    let observing_index = Arc::new(SessionIndex::open(&index_path).expect("observing index should open"));
    let writing_index = Arc::new(SessionIndex::open(&index_path).expect("writing index should open"));
    let initial_header = indexed_header(source_path.clone(), "external-commit", None);
    let mut winning_header = initial_header.clone();
    winning_header.title = Some("written by the other viewer".to_owned());

    let initial_writer = ViewerService::new_with_index(
      indexing_repository(vec![IndexedLoadSpec {
        header: initial_header.clone(),
        messages: Vec::new(),
      }]),
      Arc::clone(&writing_index),
    );
    assert!(
      initial_writer
        .refresh_session_catalog()
        .expect("initial catalog should commit before the observing viewer opens")
        .changed
    );

    let observing_repository = indexing_repository(vec![IndexedLoadSpec {
      header: initial_header.clone(),
      messages: Vec::new(),
    }]);
    let observing_service = ViewerService::new_with_index(observing_repository.clone(), observing_index);
    let initial_sidebar = observing_service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .expect("first sidebar request should read the durable initial catalog");
    assert_eq!(initial_sidebar.sessions.len(), 1);
    assert_eq!(
      initial_sidebar.sessions[0].title.as_deref(),
      Some("Indexed external-commit")
    );

    let writing_service = ViewerService::new_with_index(
      indexing_repository(vec![IndexedLoadSpec {
        header: winning_header.clone(),
        messages: Vec::new(),
      }]),
      writing_index,
    );
    assert!(
      writing_service
        .refresh_session_catalog()
        .expect("other viewer should commit its catalog")
        .changed
    );
    observing_repository
      .listings
      .lock()
      .expect("fixture listings lock should not be poisoned")
      .insert(ViewerProvider::Codex, vec![winning_header]);

    let observed = observing_service
      .refresh_session_catalog()
      .expect("matching local catalog should refresh");
    assert!(
      observed.changed,
      "SQLite data_version must wake this process even when its first local scan writes nothing"
    );
  }

  #[test]
  fn unchanged_shared_index_follow_keeps_the_recovered_progress_snapshot() {
    let service = ViewerService::new(indexing_repository(Vec::new()));
    service.index_progress.update(|progress| {
      let codex = provider_body_mut(&mut progress.body.providers, ViewerProvider::Codex);
      codex.total_jobs = 42;
      codex.completed_jobs = 42;
    });

    assert!(!service.follow_shared_index().expect("unchanged index should query"));
    let progress = service.session_index_progress();
    let codex = progress
      .body
      .providers
      .iter()
      .find(|provider| provider.provider == ViewerProvider::Codex)
      .expect("Codex progress should exist");
    assert_eq!(codex.total_jobs, 42);
    assert_eq!(codex.completed_jobs, 42);
  }

  #[test]
  fn session_index_quietly_retries_when_catalog_membership_changes() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let first_path = directory.path().join("first.jsonl");
    let second_path = directory.path().join("second.jsonl");
    std::fs::write(&first_path, "first source").expect("first fixture source should be written");
    std::fs::write(&second_path, "second source").expect("second fixture source should be written");
    let first = indexed_header(first_path, "first", None);
    let second = indexed_header(second_path, "second", None);
    let repository = Arc::new(CatalogSequenceRepository {
      codex_catalogs: vec![vec![first.clone()], vec![second.clone()]],
      codex_header_calls: AtomicUsize::new(0),
      mutate_path_after_first_catalog: None,
      remove_path_after_first_catalog: None,
    });
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository, Arc::clone(&index));

    let unstable = service.refresh_session_index().expect("catalog race should stay quiet");
    assert!(unstable.retry_catalog_soon);
    assert!(service.index_error_for(ViewerProvider::Codex).is_none());
    assert!(
      index
        .source_state(&index_catalog_source_key(ViewerProvider::Codex))
        .expect("catalog state should query")
        .is_none(),
      "an unstable first catalog must not publish its readiness sentinel"
    );
    assert!(
      index
        .session(
          &index_session_key(&locator_for_header(ViewerProvider::Codex, &first)).expect("first key should encode")
        )
        .expect("index query should work")
        .is_none(),
      "the disappearing source should wait for a stable catalog"
    );

    let stable = service
      .refresh_session_index()
      .expect("stable retry should catalog the replacement");
    assert!(!stable.retry_catalog_soon);
    assert!(service.index_error_for(ViewerProvider::Codex).is_none());
    assert!(
      index
        .session(
          &index_session_key(&locator_for_header(ViewerProvider::Codex, &second)).expect("second key should encode")
        )
        .expect("index query should work")
        .is_some()
    );
  }

  #[test]
  fn session_index_defers_only_a_source_that_changes_during_cataloging() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("active.jsonl");
    std::fs::write(&path, "before catalog confirmation").expect("fixture source should be written");
    let header = indexed_header(path.clone(), "active", None);
    let repository = Arc::new(CatalogSequenceRepository {
      codex_catalogs: vec![vec![header.clone()]],
      codex_header_calls: AtomicUsize::new(0),
      mutate_path_after_first_catalog: Some(path),
      remove_path_after_first_catalog: None,
    });
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository, Arc::clone(&index));
    let locator = locator_for_header(ViewerProvider::Codex, &header);

    let unstable = service
      .refresh_session_index()
      .expect("active source should defer without an error");
    assert!(unstable.retry_catalog_soon);
    assert!(service.index_error_for(ViewerProvider::Codex).is_none());
    assert!(
      index
        .session(&index_session_key(&locator).expect("index key should encode"))
        .expect("index query should work")
        .is_none(),
      "only the source with a changed cursor should be deferred"
    );

    let stable = service
      .refresh_session_index()
      .expect("next stable catalog should recover");
    assert!(!stable.retry_catalog_soon);
    assert!(
      index
        .session(&index_session_key(&locator).expect("index key should encode"))
        .expect("index query should work")
        .is_some()
    );
  }

  #[test]
  fn session_index_reports_a_source_that_becomes_unreadable_during_cataloging() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("removed.jsonl");
    std::fs::write(&path, "fixture source").expect("fixture source should be written");
    let header = indexed_header(path.clone(), "removed", None);
    let repository = Arc::new(CatalogSequenceRepository {
      codex_catalogs: vec![vec![header]],
      codex_header_calls: AtomicUsize::new(0),
      mutate_path_after_first_catalog: None,
      remove_path_after_first_catalog: Some(path),
    });
    let service = ViewerService::new_with_index(
      repository,
      Arc::new(SessionIndex::open_in_memory().expect("test index should open")),
    );

    service
      .refresh_session_index()
      .expect("provider failures should stay isolated from the refresh loop");
    assert_eq!(
      service.index_error_for(ViewerProvider::Codex).as_deref(),
      Some("session source is unavailable while indexing")
    );
  }

  #[test]
  fn session_index_backfills_a_shared_source_in_one_newest_first_batch() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let database_path = directory.path().join("opencode.db");
    std::fs::write(&database_path, "fixture database").expect("fixture database should be written");
    let mut specs = Vec::new();
    for index in 0..=INDEX_BODY_SCAN_BATCH_SIZE {
      let id = format!("shared-{index}");
      let mut header = indexed_header(database_path.clone(), &id, None);
      header.updated_at = Some(format!("2026-09-01T00:01:{index:02}Z"));
      header.updated_at_ms = Some(i64::try_from(index).expect("small fixture index should fit"));
      specs.push(IndexedLoadSpec {
        header,
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      });
    }
    let repository = indexing_repository_for(ViewerProvider::OpenCode, specs);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));

    let first_refresh = service
      .refresh_session_index()
      .expect("catalog and first shared-source batch should refresh");
    assert!(first_refresh.has_pending_body_jobs);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), INDEX_BODY_SCAN_BATCH_SIZE);
    let first_loads = repository
      .load_order
      .lock()
      .expect("fixture load order lock should not be poisoned")
      .iter()
      .map(|locator| locator.session_id.clone())
      .collect::<Vec<_>>();
    assert_eq!(
      first_loads,
      (1..=INDEX_BODY_SCAN_BATCH_SIZE)
        .rev()
        .map(|index| format!("shared-{index}"))
        .collect::<Vec<_>>(),
      "the newest sessions in a shared source should use the entire bounded batch"
    );

    let source_key =
      index_source_key_for_path(ViewerProvider::OpenCode, &database_path).expect("shared source key should encode");
    let source = index
      .source_state(&source_key)
      .expect("source query should work")
      .expect("shared source should be indexed");
    assert!(
      source.cursor.starts_with(&format!(
        "{INDEX_PENDING_BODY_CURSOR_PREFIX}{INDEX_BODY_SCAN_BATCH_SIZE}."
      )),
      "each completed shared-source session should advance the staged generation"
    );
    let indexed = index
      .list_present_sessions()
      .expect("present sessions should list")
      .into_iter()
      .filter(|session| session.key.provider == ViewerProvider::OpenCode.as_str())
      .collect::<Vec<_>>();
    assert_eq!(indexed.len(), INDEX_BODY_SCAN_BATCH_SIZE + 1);
    assert_eq!(
      indexed.iter().filter(|session| session.attention_baselined).count(),
      INDEX_BODY_SCAN_BATCH_SIZE,
      "completing one shared-source session must not tombstone its siblings"
    );

    let second_refresh = service
      .refresh_session_index()
      .expect("remaining shared-source body should refresh");
    assert!(!second_refresh.has_pending_body_jobs);
    assert_eq!(
      repository.load_calls.load(Ordering::SeqCst),
      INDEX_BODY_SCAN_BATCH_SIZE + 1
    );
    assert!(
      index
        .list_present_sessions()
        .expect("present sessions should list")
        .into_iter()
        .filter(|session| session.key.provider == ViewerProvider::OpenCode.as_str())
        .all(|session| session.attention_baselined),
      "the final sibling should complete without replacing the shared source inventory"
    );
  }

  #[test]
  fn session_index_defers_new_session_attention_until_its_body_completes() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let original_path = directory.path().join("original.jsonl");
    std::fs::write(&original_path, "original session").expect("fixture source should be written");
    let original = indexed_header(original_path, "original", None);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: original,
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    service
      .refresh_session_index()
      .expect("initial quiet baseline should refresh");

    let new_path = directory.path().join("new.jsonl");
    std::fs::write(&new_path, "new session").expect("fixture source should be written");
    let mut new_header = indexed_header(new_path, "new", None);
    new_header.updated_at_ms = Some(99);
    let new_locator = locator_for_header(ViewerProvider::Codex, &new_header);
    repository
      .listings
      .lock()
      .expect("fixture listings lock should not be poisoned")
      .insert(ViewerProvider::Codex, vec![new_header.clone()]);
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        new_locator.clone(),
        Ok(IndexedLoadSpec {
          header: new_header,
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );

    service
      .refresh_provider_catalog(ViewerProvider::Codex)
      .expect("new header catalog should refresh");
    let pending = index
      .session(&index_session_key(&new_locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("new catalog row should exist");
    assert!(!pending.attention_baselined);
    assert!(pending.notify_on_baseline);
    assert!(!pending.has_unread());

    let completed = service.refresh_session_index().expect("new body should complete");
    assert_eq!(
      completed.attention_session_keys,
      vec![encode_session_key(&new_locator).expect("session key should encode")]
    );
    assert!(
      index
        .session(&index_session_key(&new_locator).expect("index key should encode"))
        .expect("index query should work")
        .expect("new session should remain indexed")
        .has_unread()
    );
  }

  #[test]
  fn session_index_does_not_dot_a_session_file_moved_to_a_new_path() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let active_path = directory.path().join("active.jsonl");
    std::fs::write(&active_path, "active session").expect("fixture source should be written");
    let active_header = indexed_header(active_path, "moved", None);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: active_header.clone(),
      messages: vec![
        indexed_message(Role::User, MessageDelivery::Unspecified),
        indexed_message(Role::Assistant, MessageDelivery::Final),
      ],
    }]);
    let service = ViewerService::new_with_index(
      repository.clone(),
      Arc::new(SessionIndex::open_in_memory().expect("test index should open")),
    );
    service.refresh_session_index().expect("baseline should refresh");

    // Codex can relocate a completed rollout from its active sessions folder
    // to archived_sessions without changing the thread ID or conversation.
    let archived_path = directory.path().join("archived.jsonl");
    std::fs::write(&archived_path, "archived session").expect("fixture source should be written");
    let mut archived_header = active_header;
    archived_header.path = archived_path;
    let archived_locator = locator_for_header(ViewerProvider::Codex, &archived_header);
    repository
      .listings
      .lock()
      .expect("fixture listings lock should not be poisoned")
      .insert(ViewerProvider::Codex, vec![archived_header.clone()]);
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        archived_locator.clone(),
        Ok(IndexedLoadSpec {
          header: archived_header,
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );

    let refresh = service.refresh_session_index().expect("moved source should refresh");
    assert!(refresh.changed);
    assert!(refresh.attention_session_keys.is_empty());
    let sessions = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .expect("indexed listing should work");
    assert_eq!(sessions.sessions.len(), 1);
    assert_eq!(
      sessions.sessions[0].session_key,
      encode_session_key(&archived_locator).expect("session key should encode")
    );
    assert!(!sessions.sessions[0].has_unread);
  }

  #[test]
  fn session_index_keeps_an_initial_quiet_baseline_when_its_file_moves_before_body_backfill() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let active_path = directory.path().join("active.jsonl");
    std::fs::write(&active_path, "active session").expect("fixture source should be written");
    let active_header = indexed_header(active_path.clone(), "moved-before-body", None);
    let active_locator = locator_for_header(ViewerProvider::Codex, &active_header);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: active_header.clone(),
      messages: vec![
        indexed_message(Role::User, MessageDelivery::Unspecified),
        indexed_message(Role::Assistant, MessageDelivery::Final),
      ],
    }]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));

    service
      .refresh_provider_catalog(ViewerProvider::Codex)
      .expect("initial header catalog should refresh");
    let initial = index
      .session(&index_session_key(&active_locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("initial catalog row should exist");
    assert!(!initial.attention_baselined);
    assert!(!initial.notify_on_baseline);

    let archived_path = directory.path().join("archived.jsonl");
    std::fs::rename(&active_path, &archived_path).expect("fixture source should move");
    let mut archived_header = active_header;
    archived_header.path = archived_path;
    let archived_locator = locator_for_header(ViewerProvider::Codex, &archived_header);
    repository
      .listings
      .lock()
      .expect("fixture listings lock should not be poisoned")
      .insert(ViewerProvider::Codex, vec![archived_header.clone()]);
    let mut loads = repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned");
    loads.remove(&active_locator);
    loads.insert(
      archived_locator.clone(),
      Ok(IndexedLoadSpec {
        header: archived_header,
        messages: vec![
          indexed_message(Role::User, MessageDelivery::Unspecified),
          indexed_message(Role::Assistant, MessageDelivery::Final),
        ],
      }),
    );
    drop(loads);

    let refresh = service.refresh_session_index().expect("moved source should refresh");
    assert!(refresh.attention_session_keys.is_empty());
    let archived = index
      .session(&index_session_key(&archived_locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("archived row should exist");
    assert!(archived.attention_baselined);
    assert!(!archived.has_unread());
  }

  #[test]
  fn session_index_preserves_unread_attention_when_a_moved_body_temporarily_fails() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let active_path = directory.path().join("active.jsonl");
    std::fs::write(&active_path, "active session").expect("fixture source should be written");
    let active_header = indexed_header(active_path.clone(), "moved-unread", None);
    let active_locator = locator_for_header(ViewerProvider::Codex, &active_header);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: active_header.clone(),
      messages: vec![
        indexed_message(Role::User, MessageDelivery::Unspecified),
        indexed_message(Role::Assistant, MessageDelivery::Final),
      ],
    }]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    service
      .refresh_session_index()
      .expect("initial baseline should refresh");

    std::fs::write(&active_path, "active session with a new final reply").expect("fixture source should change");
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        active_locator.clone(),
        Ok(IndexedLoadSpec {
          header: active_header.clone(),
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Final),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );
    service.refresh_session_index().expect("new attention should refresh");
    assert!(
      index
        .session(&index_session_key(&active_locator).expect("index key should encode"))
        .expect("index query should work")
        .expect("active row should remain indexed")
        .has_unread()
    );

    let archived_path = directory.path().join("archived.jsonl");
    std::fs::rename(&active_path, &archived_path).expect("fixture source should move");
    let mut archived_header = active_header;
    archived_header.path = archived_path;
    let archived_locator = locator_for_header(ViewerProvider::Codex, &archived_header);
    repository
      .listings
      .lock()
      .expect("fixture listings lock should not be poisoned")
      .insert(ViewerProvider::Codex, vec![archived_header.clone()]);
    let mut loads = repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned");
    loads.remove(&active_locator);
    loads.insert(
      archived_locator.clone(),
      Err("fixture archive body is temporarily unavailable".to_string()),
    );
    drop(loads);

    service
      .refresh_session_index()
      .expect("catalog should preserve the moved unread state despite the body failure");
    let staged_archive = index
      .session(&index_session_key(&archived_locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("archived row should remain indexed");
    assert!(!staged_archive.attention_baselined);
    assert!(staged_archive.has_unread());
    assert_eq!(staged_archive.attention_revision, 1);

    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        archived_locator.clone(),
        Ok(IndexedLoadSpec {
          header: archived_header,
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Final),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );
    let retry = service
      .refresh_pending_session_index()
      .expect("archive body retry should refresh");
    assert!(retry.attention_session_keys.is_empty());
    let completed_archive = index
      .session(&index_session_key(&archived_locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("archived row should remain indexed");
    assert!(completed_archive.attention_baselined);
    assert!(completed_archive.has_unread());
    assert_eq!(completed_archive.attention_revision, 1);
  }

  #[test]
  fn session_index_preserves_body_presentation_when_a_session_moves_and_archive_body_fails() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let active_path = directory.path().join("active.jsonl");
    std::fs::write(&active_path, "active session").expect("fixture source should be written");
    let mut active_header = indexed_header(active_path.clone(), "moved-presentation", None);
    active_header.title = None;
    active_header.preview = None;
    let active_locator = locator_for_header(ViewerProvider::Codex, &active_header);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: active_header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let mut loaded_active_header = active_header.clone();
    loaded_active_header.title = Some("Body-derived title".to_owned());
    loaded_active_header.preview = Some("Body-derived preview".to_owned());
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        active_locator.clone(),
        Ok(IndexedLoadSpec {
          header: loaded_active_header,
          messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
        }),
      );
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    service
      .refresh_session_index()
      .expect("active body should establish its presentation fallback");
    let active = index
      .session(&index_session_key(&active_locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("active row should exist");
    assert_eq!(active.title.as_deref(), Some("Body-derived title"));
    assert_eq!(active.catalog_title, None);
    assert_eq!(active.body_title.as_deref(), Some("Body-derived title"));

    let archived_path = directory.path().join("archived.jsonl");
    std::fs::rename(&active_path, &archived_path).expect("fixture source should move");
    let mut archived_header = active_header;
    archived_header.path = archived_path;
    let archived_locator = locator_for_header(ViewerProvider::Codex, &archived_header);
    repository
      .listings
      .lock()
      .expect("fixture listings lock should not be poisoned")
      .insert(ViewerProvider::Codex, vec![archived_header]);
    let mut loads = repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned");
    loads.remove(&active_locator);
    loads.insert(
      archived_locator.clone(),
      Err("fixture archive body is temporarily unavailable".to_owned()),
    );
    drop(loads);

    service
      .refresh_session_index()
      .expect("catalog should retain moved presentation despite an archive body failure");
    let archived = index
      .session(&index_session_key(&archived_locator).expect("index key should encode"))
      .expect("index query should work")
      .expect("archived row should remain indexed");
    assert!(!archived.attention_baselined);
    assert_eq!(archived.catalog_title, None);
    assert_eq!(archived.catalog_preview, None);
    assert_eq!(archived.body_title.as_deref(), Some("Body-derived title"));
    assert_eq!(archived.body_preview.as_deref(), Some("Body-derived preview"));
    assert_eq!(archived.title.as_deref(), Some("Body-derived title"));
    assert_eq!(archived.preview.as_deref(), Some("Body-derived preview"));
  }

  #[test]
  fn session_index_updates_header_metadata_without_replaying_an_unchanged_body() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("renamed.jsonl");
    std::fs::write(&path, "unchanged body").expect("fixture source should be written");
    let header = indexed_header(path, "renamed", None);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let service = ViewerService::new_with_index(
      repository.clone(),
      Arc::new(SessionIndex::open_in_memory().expect("test index should open")),
    );
    service.refresh_session_index().expect("baseline should refresh");
    let load_calls_after_baseline = repository.load_calls.load(Ordering::SeqCst);

    // Codex title metadata can change in state_5.sqlite while the rollout
    // JSONL has not changed. The compact index must reflect it without a
    // needless body replay or a new-message dot.
    let mut renamed_header = header;
    renamed_header.title = Some("Renamed in state metadata".to_string());
    renamed_header.preview = Some("Updated compact preview".to_string());
    repository
      .listings
      .lock()
      .expect("fixture listings lock should not be poisoned")
      .insert(ViewerProvider::Codex, vec![renamed_header]);
    let refresh = service.refresh_session_index().expect("metadata refresh should work");
    assert!(refresh.changed);
    assert!(refresh.attention_session_keys.is_empty());
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), load_calls_after_baseline);

    let sessions = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .expect("indexed listing should work");
    assert_eq!(sessions.sessions[0].title.as_deref(), Some("Renamed in state metadata"));
    assert_eq!(sessions.sessions[0].preview.as_deref(), Some("Updated compact preview"));
    assert!(!sessions.sessions[0].has_unread);
  }

  #[test]
  fn session_index_baselines_then_tracks_and_acknowledges_visible_messages() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("root.jsonl");
    std::fs::write(&path, "baseline").expect("fixture source should be written");
    let header = indexed_header(path.clone(), "root", None);
    let locator = locator_for_header(ViewerProvider::Codex, &header);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![
        indexed_message(Role::User, MessageDelivery::Unspecified),
        indexed_message(Role::Assistant, MessageDelivery::Final),
      ],
    }]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    let request = ListSessionsRequest {
      query: SessionQuery {
        order: SessionOrder::Time,
        providers: vec![ViewerProvider::Codex],
        search: None,
      },
      cursor: None,
      offset: None,
      limit: None,
    };

    // Before the first catalog commits, list IPC reports a cold index instead
    // of reading provider headers or bodies on the UI request path.
    let initial = service
      .list_sessions(request.clone())
      .expect("cold index listing should work");
    assert!(initial.sessions.is_empty());
    assert_eq!(initial.pending_providers, vec![ViewerProvider::Codex]);
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 0);

    let baseline_refresh = service.refresh_session_index().expect("baseline should refresh");
    assert!(baseline_refresh.changed);
    assert!(baseline_refresh.attention_session_keys.is_empty());
    assert_eq!(
      baseline_refresh.updated_session_keys,
      vec![encode_session_key(&locator).unwrap()]
    );
    let calls_after_baseline = repository.header_calls.load(Ordering::SeqCst);
    let baseline = service
      .list_sessions(request.clone())
      .expect("indexed listing should work");
    assert!(!baseline.sessions[0].has_unread);
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), calls_after_baseline);

    // Appending a final reply changes both the file checkpoint and the compact
    // eligible-message count. The SQLite row gets attention, not the body.
    std::fs::write(&path, "baseline plus a later reply").expect("fixture source should change");
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        locator.clone(),
        Ok(IndexedLoadSpec {
          header,
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Final),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );
    let changed_refresh = service.refresh_session_index().expect("changed source should refresh");
    assert!(changed_refresh.changed);
    assert_eq!(
      changed_refresh.attention_session_keys,
      vec![encode_session_key(&locator).expect("session key should encode")]
    );
    let unread = service
      .list_sessions(request.clone())
      .expect("indexed listing should work");
    assert!(unread.sessions[0].has_unread);

    let page = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: unread.sessions[0].session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Backward,
        limit: None,
      })
      .expect("event page should load");
    let attention_revision = page.attention_revision.expect("unread page should capture attention");
    let acknowledgement = service
      .acknowledge_session_attention(AcknowledgeSessionAttentionRequest {
        session_key: unread.sessions[0].session_key.clone(),
        attention_revision,
      })
      .expect("acknowledgement should succeed");
    assert!(acknowledgement.changed);
    assert!(
      !service
        .list_sessions(request)
        .expect("indexed listing should work")
        .sessions[0]
        .has_unread
    );

    let indexed = index
      .session(&index_session_key(&locator).expect("index key should encode"))
      .expect("indexed session should query")
      .expect("indexed session should exist");
    assert_eq!(indexed.attention_marker.as_deref(), Some("session-activity.v3.2.0.0.0"));
  }

  #[test]
  fn catalog_refresh_publishes_index_rows_before_body_backfill() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("catalog-first.jsonl");
    std::fs::write(&path, "fixture body").expect("fixture source should be written");
    let header = indexed_header(path, "catalog-first", None);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let service = ViewerService::new_with_index(
      repository.clone(),
      Arc::new(SessionIndex::open_in_memory().expect("test index should open")),
    );

    let catalog = service
      .refresh_session_catalog()
      .expect("catalog-only refresh should succeed");
    assert!(catalog.changed);
    assert!(catalog.has_pending_body_jobs);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 0);

    let listed = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .expect("cataloged rows should be listable before body work");
    assert!(listed.pending_providers.is_empty());
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].session_id, header.id);

    service
      .refresh_pending_session_index()
      .expect("body backfill should succeed");
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn index_progress_is_index_only_and_tracks_a_bounded_body_backfill() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let mut specs = Vec::new();
    for index in 0..=INDEX_BODY_SCAN_BATCH_SIZE {
      let id = format!("progress-{index}");
      let path = directory.path().join(format!("{id}.jsonl"));
      std::fs::write(&path, format!("fixture {index}")).expect("fixture source should be written");
      let mut header = indexed_header(path, &id, None);
      header.updated_at_ms = Some(i64::try_from(index).expect("fixture index should fit"));
      specs.push(IndexedLoadSpec {
        header,
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      });
    }
    let repository = indexing_repository(specs);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));

    let cold = service.session_index_progress();
    assert_eq!(cold.activity, IndexActivity::Idle);
    assert_eq!(cold.catalog.pending_providers, ViewerProvider::ALL.to_vec());
    assert_eq!(cold.body.pending_jobs, 0);
    for _ in 0..3 {
      let _ = service.session_index_progress();
    }
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 0);

    service.refresh_session_catalog().expect("catalog should refresh");
    let cataloged = service.session_index_progress();
    assert_ne!(cataloged.revision, cold.revision);
    assert_eq!(cataloged.activity, IndexActivity::WaitingToRetry);
    assert!(!cataloged.is_refreshing);
    assert!(cataloged.catalog.pending_providers.is_empty());
    assert_eq!(cataloged.catalog.processed_providers, ViewerProvider::ALL.len());
    assert_eq!(cataloged.body.pending_jobs, INDEX_BODY_SCAN_BATCH_SIZE + 1);
    assert_eq!(cataloged.body.failed_jobs, 0);
    assert_eq!(cataloged.body.batch_size, INDEX_BODY_SCAN_BATCH_SIZE);
    assert_eq!(cataloged.body.providers[0].provider, ViewerProvider::Codex);
    assert_eq!(cataloged.body.providers[0].total_jobs, INDEX_BODY_SCAN_BATCH_SIZE + 1);
    assert_eq!(cataloged.body.providers[0].completed_jobs, 0);
    assert_eq!(cataloged.body.providers[0].pending_jobs, INDEX_BODY_SCAN_BATCH_SIZE + 1);

    service
      .refresh_pending_session_index()
      .expect("first body batch should refresh");
    let first_body_batch = service.session_index_progress();
    assert_eq!(first_body_batch.activity, IndexActivity::WaitingToRetry);
    assert_eq!(first_body_batch.body.pending_jobs, 1);
    assert_eq!(first_body_batch.body.completed_in_run, INDEX_BODY_SCAN_BATCH_SIZE);
    assert_eq!(first_body_batch.body.stale_in_run, 0);
    assert_eq!(
      first_body_batch.body.providers[0].total_jobs,
      INDEX_BODY_SCAN_BATCH_SIZE + 1
    );
    assert_eq!(
      first_body_batch.body.providers[0].completed_jobs,
      INDEX_BODY_SCAN_BATCH_SIZE
    );
    assert_eq!(first_body_batch.body.providers[0].pending_jobs, 1);

    // The staged v3 cursor stores the completed generation. A new viewer
    // process must therefore retain this exact bounded-batch fraction without
    // rereading provider headers or bodies.
    let header_calls_before_reopen = repository.header_calls.load(Ordering::SeqCst);
    let load_calls_before_reopen = repository.load_calls.load(Ordering::SeqCst);
    let reopened = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    let reopened_progress = reopened.session_index_progress();
    assert_eq!(
      reopened_progress.body.providers[0].total_jobs,
      INDEX_BODY_SCAN_BATCH_SIZE + 1
    );
    assert_eq!(
      reopened_progress.body.providers[0].completed_jobs,
      INDEX_BODY_SCAN_BATCH_SIZE
    );
    assert_eq!(reopened_progress.body.providers[0].pending_jobs, 1);
    assert_eq!(
      repository.header_calls.load(Ordering::SeqCst),
      header_calls_before_reopen
    );
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), load_calls_before_reopen);

    reopened
      .refresh_pending_session_index()
      .expect("last body job should refresh");
    let complete = reopened.session_index_progress();
    assert_eq!(complete.body.pending_jobs, 0);
    assert_eq!(complete.body.completed_in_run, 1);
    assert_eq!(complete.body.failed_jobs, 0);
    assert_eq!(complete.body.providers[0].total_jobs, INDEX_BODY_SCAN_BATCH_SIZE + 1);
    assert_eq!(
      complete.body.providers[0].completed_jobs,
      INDEX_BODY_SCAN_BATCH_SIZE + 1
    );

    let completed_reopen = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    let completed_progress = completed_reopen.session_index_progress();
    assert_eq!(
      completed_progress.body.providers[0].total_jobs,
      INDEX_BODY_SCAN_BATCH_SIZE + 1
    );
    assert_eq!(
      completed_progress.body.providers[0].completed_jobs,
      INDEX_BODY_SCAN_BATCH_SIZE + 1
    );
    assert_eq!(completed_progress.body.providers[0].pending_jobs, 0);
  }

  #[test]
  fn a_changed_catalog_source_starts_a_fresh_provider_body_baseline() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("progress-reset.jsonl");
    std::fs::write(&path, "initial body").expect("fixture source should be written");
    let header = indexed_header(path.clone(), "progress-reset", None);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header,
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let service = ViewerService::new_with_index(
      repository,
      Arc::new(SessionIndex::open_in_memory().expect("test index should open")),
    );

    service.refresh_session_index().expect("initial body should complete");
    let completed = service.session_index_progress();
    assert_eq!(completed.body.providers[0].total_jobs, 1);
    assert_eq!(completed.body.providers[0].completed_jobs, 1);
    assert_eq!(completed.body.providers[0].pending_jobs, 0);

    std::fs::write(&path, "changed body with a distinct source revision").expect("fixture source should change");
    service
      .refresh_session_catalog()
      .expect("changed source should establish a new catalog baseline");
    let staged = service.session_index_progress();
    assert_eq!(staged.body.providers[0].total_jobs, 1);
    assert_eq!(staged.body.providers[0].completed_jobs, 0);
    assert_eq!(staged.body.providers[0].pending_jobs, 1);
  }

  #[test]
  fn reopened_index_progress_matches_staged_body_jobs_without_provider_reads() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let codex_path = directory.path().join("codex.jsonl");
    let pi_path = directory.path().join("pi.jsonl");
    let completed_path = directory.path().join("completed.jsonl");
    let retired_path = directory.path().join("retired.jsonl");
    for path in [&codex_path, &pi_path, &completed_path, &retired_path] {
      std::fs::write(path, "fixture body").expect("fixture source should be written");
    }

    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let codex_source = index_source_key_for_path(ViewerProvider::Codex, &codex_path).expect("Codex key should encode");
    let pi_source = index_source_key_for_path(ViewerProvider::Pi, &pi_path).expect("Pi key should encode");
    let completed_source =
      index_source_key_for_path(ViewerProvider::ZCode, &completed_path).expect("completed key should encode");
    let retired_source = SourceKey::new("retired", "path.v1.fixture");

    let mut codex_session = session_metadata_from_header(
      &codex_source,
      indexed_header(codex_path.clone(), "codex-pending", None),
      None,
      false,
    )
    .expect("Codex session should be indexable");
    codex_session.attention_baselined = false;
    let mut pi_session = session_metadata_from_header(
      &pi_source,
      indexed_header(pi_path.clone(), "pi-pending", None),
      None,
      false,
    )
    .expect("Pi session should be indexable");
    pi_session.attention_baselined = false;
    let mut completed_session = session_metadata_from_header(
      &completed_source,
      indexed_header(completed_path, "completed-pending", None),
      None,
      false,
    )
    .expect("completed session should be indexable");
    completed_session.attention_baselined = false;
    let mut retired_session = session_metadata_from_header(
      &retired_source,
      indexed_header(retired_path, "retired-pending", None),
      None,
      false,
    )
    .expect("retired session should be indexable");
    retired_session.attention_baselined = false;

    index
      .replace_sources(&[
        SourceReplacement::new(
          SourceState::new(codex_source, pending_body_cursor("codex"), 0),
          vec![codex_session],
        ),
        SourceReplacement::new(
          SourceState::new(pi_source, format!("{LEGACY_PENDING_BODY_CURSOR_PREFIX}pi"), 0),
          vec![pi_session],
        ),
        SourceReplacement::new(
          SourceState::new(completed_source, completed_body_cursor("zcode"), 0),
          vec![completed_session],
        ),
        SourceReplacement::new(
          SourceState::new(retired_source, pending_body_cursor("retired"), 0),
          vec![retired_session],
        ),
      ])
      .expect("fixture sources should be indexed");

    let repository = indexing_repository(Vec::new());
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    let startup = service.session_index_progress();
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 0);
    assert_eq!(startup.body.pending_jobs, 2);
    assert_eq!(startup.body.failed_jobs, 0);
    assert_eq!(
      startup.body.providers,
      vec![
        ProviderBody {
          provider: ViewerProvider::Codex,
          total_jobs: 1,
          completed_jobs: 0,
          pending_jobs: 1,
          failed_jobs: 0,
        },
        ProviderBody {
          provider: ViewerProvider::Pi,
          total_jobs: 1,
          completed_jobs: 0,
          pending_jobs: 1,
          failed_jobs: 0,
        },
        ProviderBody {
          provider: ViewerProvider::OpenCode,
          total_jobs: 0,
          completed_jobs: 0,
          pending_jobs: 0,
          failed_jobs: 0,
        },
        ProviderBody {
          provider: ViewerProvider::ZCode,
          total_jobs: 0,
          completed_jobs: 0,
          pending_jobs: 0,
          failed_jobs: 0,
        },
        ProviderBody {
          provider: ViewerProvider::WorkBuddy,
          total_jobs: 0,
          completed_jobs: 0,
          pending_jobs: 0,
          failed_jobs: 0,
        },
        ProviderBody {
          provider: ViewerProvider::Dsh,
          total_jobs: 0,
          completed_jobs: 0,
          pending_jobs: 0,
          failed_jobs: 0,
        },
      ]
    );

    let all_jobs = service
      .pending_body_jobs(&HashSet::new())
      .expect("staged body jobs should query");
    let (pending_jobs, failed_jobs, providers) = body_queue_progress(&all_jobs);
    assert_eq!(pending_jobs, startup.body.pending_jobs);
    assert_eq!(failed_jobs, startup.body.failed_jobs);
    assert_eq!(
      providers
        .iter()
        .map(|provider| (provider.provider, provider.pending_jobs, provider.failed_jobs))
        .collect::<Vec<_>>(),
      startup
        .body
        .providers
        .iter()
        .map(|provider| (provider.provider, provider.pending_jobs, provider.failed_jobs))
        .collect::<Vec<_>>()
    );

    let unavailable_jobs = service
      .pending_body_jobs(&HashSet::from([ViewerProvider::Pi]))
      .expect("unavailable provider should be skippable");
    assert_eq!(unavailable_jobs.len(), 1);
    service.record_failed_body_job(&all_jobs[0], "fixture body failure".to_owned());
    let failed_jobs = service
      .pending_body_jobs(&HashSet::new())
      .expect("failed job should remain queued");
    assert_eq!(body_queue_progress(&failed_jobs).1, 1);
    // Failed and unavailable state is process-local. The fresh startup
    // snapshot therefore reports durable work only; later scheduler passes
    // replace it with their live failed/unavailable view.
    assert_eq!(startup.body.failed_jobs, 0);
  }

  #[test]
  fn index_progress_exposes_catalog_failures_without_provider_error_text() {
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::from([(ViewerProvider::Codex, Err("fixture catalog failure".to_string()))]),
      loaded: Mutex::new(None),
    }));

    let refresh = service
      .refresh_session_catalog()
      .expect("one provider failure should not abort other catalogs");
    assert!(refresh.has_catalog_errors);

    let progress = service.session_index_progress();
    assert_eq!(progress.activity, IndexActivity::WaitingToRetry);
    assert_eq!(progress.catalog.error_providers, vec![ViewerProvider::Codex]);
    assert_eq!(progress.catalog.pending_providers, vec![ViewerProvider::Codex]);
    let serialized = serde_json::to_string(&progress).expect("progress should serialize");
    assert!(!serialized.contains("fixture catalog failure"));

    let body_only = service
      .refresh_pending_session_index()
      .expect("body-only refresh should retain the catalog retry hint");
    assert!(body_only.has_catalog_errors);
  }

  #[test]
  fn index_progress_retry_publishes_waiting_state_and_wakes_the_scheduler() {
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::new(),
      loaded: Mutex::new(None),
    }));
    let mut progress_updates = service.subscribe_session_index_progress();
    let (retry_sender, mut retry_receiver) = tokio::sync::mpsc::unbounded_channel();
    service.set_session_index_retry_sender(retry_sender);

    service.failed_body_jobs.lock().unwrap().insert(
      (SourceKey::new("codex", "failed"), "failed".into()),
      FailedBodyJob {
        raw_cursor: "cursor".into(),
        source_generation: 1,
        message: "failed".into(),
      },
    );
    let before = service.session_index_progress();
    let queued = service
      .request_session_index_retry()
      .expect("configured scheduler should accept a retry wake");

    assert_eq!(queued.activity, IndexActivity::WaitingToRetry);
    assert!(!queued.is_refreshing);
    assert_eq!(queued.retry_at_ms, None);
    assert_ne!(queued.revision, before.revision);
    assert!(progress_updates.has_changed().expect("watch sender should remain open"));
    assert_eq!(progress_updates.borrow_and_update().revision, queued.revision);
    assert!(retry_receiver.try_recv().is_ok());
    assert!(service.failed_body_jobs.lock().unwrap().is_empty());
  }

  #[test]
  fn index_progress_retry_does_not_hide_an_active_refresh() {
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::new(),
      loaded: Mutex::new(None),
    }));
    let (retry_sender, mut retry_receiver) = tokio::sync::mpsc::unbounded_channel();
    service.set_session_index_retry_sender(retry_sender);
    service.begin_session_index_catalog_refresh(CatalogRefreshScope::Full, ViewerProvider::ALL.len());

    let queued = service
      .request_session_index_retry()
      .expect("configured scheduler should accept an active retry wake");

    assert_eq!(queued.activity, IndexActivity::Catalog);
    assert!(queued.is_refreshing);
    assert_eq!(queued.retry_at_ms, None);
    assert!(retry_receiver.try_recv().is_ok());
  }

  #[test]
  fn manual_retry_between_worker_finish_and_scheduler_settlement_stays_visible() {
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::new(),
      loaded: Mutex::new(None),
    }));
    let (retry_sender, mut retry_receiver) = tokio::sync::mpsc::unbounded_channel();
    service.set_session_index_retry_sender(retry_sender);

    service.begin_session_index_catalog_refresh(CatalogRefreshScope::Full, ViewerProvider::ALL.len());
    let result = Ok(IndexRefresh::default());
    service.finish_session_index_refresh(&result);
    assert_eq!(service.session_index_progress().activity, IndexActivity::Idle);

    let queued = service
      .request_session_index_retry()
      .expect("configured scheduler should accept a retry wake after the worker finishes");
    assert_eq!(queued.activity, IndexActivity::WaitingToRetry);
    assert_eq!(queued.retry_at_ms, None);

    // This mirrors the async scheduler applying its exact normal-poll state
    // after `spawn_blocking` returns. It must not replace the newer manual
    // wake with a stale idle snapshot.
    let settled = service.settle_session_index_idle_after_refresh();
    assert_eq!(settled.activity, IndexActivity::WaitingToRetry);
    assert_eq!(settled.retry_at_ms, None);
    assert!(retry_receiver.try_recv().is_ok());
  }

  #[test]
  fn manual_retry_during_a_worker_pass_suppresses_its_stale_error() {
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::new(),
      loaded: Mutex::new(None),
    }));
    let (retry_sender, mut retry_receiver) = tokio::sync::mpsc::unbounded_channel();
    service.set_session_index_retry_sender(retry_sender);

    service.begin_session_index_catalog_refresh(CatalogRefreshScope::Full, ViewerProvider::ALL.len());
    let queued = service
      .request_session_index_retry()
      .expect("configured scheduler should accept an active retry wake");
    assert_eq!(queued.activity, IndexActivity::Catalog);
    service.continue_session_index_body_refresh();
    assert_eq!(service.session_index_progress().activity, IndexActivity::Body);

    let result = Err("fixture worker failure".to_string());
    service.finish_session_index_refresh(&result);
    let settled = service.settle_session_index_worker_error_after_refresh(IndexWorkerError::TaskFailed, Some(42));
    assert_eq!(settled.activity, IndexActivity::WaitingToRetry);
    assert_eq!(settled.worker_error, None);
    assert_eq!(settled.retry_at_ms, None);
    assert!(retry_receiver.try_recv().is_ok());
  }

  #[test]
  fn index_progress_exposes_only_a_sanitized_worker_failure_category() {
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::new(),
      loaded: Mutex::new(None),
    }));

    let failed = service.settle_session_index_worker_error_after_refresh(IndexWorkerError::TaskFailed, Some(42));
    assert_eq!(failed.activity, IndexActivity::WaitingToRetry);
    assert_eq!(failed.worker_error, Some(IndexWorkerError::TaskFailed));
    assert_eq!(failed.retry_at_ms, Some(42));
    let serialized = serde_json::to_string(&failed).expect("progress should serialize");
    assert!(serialized.contains("task_failed"));

    let recovered = service.settle_session_index_waiting_to_retry_after_refresh(Some(43));
    assert_eq!(recovered.worker_error, None);
    assert_eq!(recovered.retry_at_ms, Some(43));
  }

  #[test]
  fn session_index_keeps_the_last_snapshot_when_a_source_changes_during_body_load() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("racing.jsonl");
    std::fs::write(&path, "baseline").expect("fixture source should be written");
    let header = indexed_header(path.clone(), "racing", None);
    let locator = locator_for_header(ViewerProvider::Codex, &header);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));
    service.refresh_session_index().expect("baseline should refresh");

    std::fs::write(&path, "changed before scan").expect("fixture source should change");
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        locator.clone(),
        Ok(IndexedLoadSpec {
          header,
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );
    *repository
      .mutate_source_on_next_load
      .lock()
      .expect("fixture source mutation lock should not be poisoned") = Some(path.clone());
    assert!(
      service
        .refresh_session_index()
        .expect("refresh loop should isolate provider failure")
        .changed,
      "the catalog update must remain visible while its body job is retried"
    );

    let stale = index
      .session(&index_session_key(&locator).expect("index key should encode"))
      .expect("indexed session should query")
      .expect("indexed session should remain present");
    assert_eq!(stale.attention_marker.as_deref(), Some("session-activity.v3.0.0.0.0"));
    assert!(!stale.attention_baselined);
    assert!(!stale.has_unread());
    assert!(service.index_error_for(ViewerProvider::Codex).is_none());

    // The next stable pass can safely observe the final reply and mark it new.
    service.refresh_session_index().expect("stable retry should refresh");
    let recovered = index
      .session(&index_session_key(&locator).expect("index key should encode"))
      .expect("indexed session should query")
      .expect("indexed session should remain present");
    assert_eq!(
      recovered.attention_marker.as_deref(),
      Some("session-activity.v3.1.0.0.0")
    );
    assert!(recovered.has_unread());
  }

  #[test]
  fn session_index_clears_a_recovered_body_warning_without_waiting_for_the_next_catalog() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("retry.jsonl");
    std::fs::write(&path, "baseline").expect("fixture source should be written");
    let header = indexed_header(path.clone(), "retry", None);
    let locator = locator_for_header(ViewerProvider::Codex, &header);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let service = ViewerService::new_with_index(
      repository.clone(),
      Arc::new(SessionIndex::open_in_memory().expect("test index should open")),
    );
    service.refresh_session_index().expect("baseline should refresh");

    std::fs::write(&path, "body retry is required").expect("fixture source should change");
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(locator.clone(), Err("fixture body read failed".to_string()));
    service
      .refresh_session_index()
      .expect("catalog should remain usable when a body fails");
    assert_eq!(
      service.index_error_for(ViewerProvider::Codex).as_deref(),
      Some("fixture body read failed")
    );

    let calls_after_failure = repository.load_calls.load(Ordering::SeqCst);
    let automatic = service
      .refresh_pending_session_index_automated()
      .expect("automatic pass should retain the failed job without retrying it");
    assert!(!automatic.has_pending_body_jobs);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), calls_after_failure);

    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        locator,
        Ok(IndexedLoadSpec {
          header,
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );
    service
      .refresh_pending_session_index()
      .expect("body-only retry should refresh");
    assert!(service.index_error_for(ViewerProvider::Codex).is_none());
  }

  #[test]
  fn same_cursor_catalog_metadata_change_reenables_a_failed_body_job() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let path = directory.path().join("same-cursor-retry.jsonl");
    std::fs::write(&path, "unchanged source bytes").expect("fixture source should be written");
    let header = indexed_header(path.clone(), "same-cursor-retry", None);
    let locator = locator_for_header(ViewerProvider::Codex, &header);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(locator.clone(), Err("fixture body read failed".to_owned()));
    let index = Arc::new(SessionIndex::open_in_memory().expect("test index should open"));
    let service = ViewerService::new_with_index(repository.clone(), Arc::clone(&index));

    service
      .refresh_provider_catalog(ViewerProvider::Codex)
      .expect("initial catalog should commit");
    service
      .refresh_pending_session_index()
      .expect("failed body should remain retryable");
    let source_key = index_source_key_for_path(ViewerProvider::Codex, &path).expect("source key should encode");
    let failed_generation = index
      .source_state(&source_key)
      .expect("source query should work")
      .expect("pending source should exist")
      .generation;

    let mut renamed_header = header;
    renamed_header.title = Some("new lightweight title".to_owned());
    repository
      .listings
      .lock()
      .expect("fixture listings lock should not be poisoned")
      .insert(ViewerProvider::Codex, vec![renamed_header]);
    service
      .refresh_provider_catalog(ViewerProvider::Codex)
      .expect("same-cursor catalog metadata update should commit");

    let jobs = service
      .pending_body_jobs(&HashSet::new())
      .expect("pending jobs should query");
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].source.generation > failed_generation);
    assert!(
      !jobs[0].deprioritized,
      "a failure from an earlier same-cursor generation must not delay the refreshed body job"
    );
  }

  #[test]
  fn session_index_ignores_commentary_and_keeps_child_unread_separate() {
    let directory = tempfile::tempdir().expect("temporary directory should exist");
    let parent_path = directory.path().join("parent.jsonl");
    let child_path = directory.path().join("child.jsonl");
    std::fs::write(&parent_path, "parent baseline").expect("parent fixture source should be written");
    std::fs::write(&child_path, "child baseline").expect("child fixture source should be written");
    let parent = indexed_header(parent_path.clone(), "parent", None);
    let child = indexed_header(child_path.clone(), "child", Some("parent"));
    let child_locator = locator_for_header(ViewerProvider::Codex, &child);
    let repository = indexing_repository(vec![
      IndexedLoadSpec {
        header: parent.clone(),
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      },
      IndexedLoadSpec {
        header: child.clone(),
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      },
    ]);
    let service = ViewerService::new_with_index(
      repository.clone(),
      Arc::new(SessionIndex::open_in_memory().expect("test index should open")),
    );
    service.refresh_session_index().expect("baseline should refresh");

    // A non-final assistant update is visible in the timeline but intentionally
    // does not count as attention. The direct child and collapsed parent stay
    // clear after its source checkpoint changes.
    std::fs::write(&child_path, "child commentary only").expect("child fixture source should change");
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        child_locator.clone(),
        Ok(IndexedLoadSpec {
          header: child.clone(),
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Commentary),
          ],
        }),
      );
    service
      .refresh_session_index()
      .expect("commentary source should refresh");
    let no_attention = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .expect("indexed listing should work");
    assert!(!no_attention.sessions[0].has_unread);
    assert!(!no_attention.sessions[0].has_unread_descendant);
    assert!(no_attention.sessions[0].has_running_descendant);
    assert!(!no_attention.sessions[0].is_running);

    std::fs::write(&child_path, "child commentary and final reply").expect("child fixture source should change");
    repository
      .loads
      .lock()
      .expect("fixture loads lock should not be poisoned")
      .insert(
        child_locator.clone(),
        Ok(IndexedLoadSpec {
          header: child.clone(),
          messages: vec![
            indexed_message(Role::User, MessageDelivery::Unspecified),
            indexed_message(Role::Assistant, MessageDelivery::Commentary),
            indexed_message(Role::Assistant, MessageDelivery::Final),
            indexed_message(Role::Assistant, MessageDelivery::Final),
            indexed_message(Role::Assistant, MessageDelivery::Final),
          ],
        }),
      );
    service
      .refresh_session_index()
      .expect("final reply source should refresh");
    let roots = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .expect("indexed listing should work");
    assert_eq!(roots.sessions.len(), 1);
    assert!(!roots.sessions[0].has_unread);
    assert!(!roots.sessions[0].has_unread_descendant);
    assert_eq!(roots.sessions[0].unread_descendant_count, 0);
    assert_eq!(roots.sessions[0].unread_final_count, 0);
    assert!(!roots.sessions[0].has_running_descendant);

    let children = service
      .list_session_children(ListSessionChildrenRequest {
        parent_session_key: roots.sessions[0].session_key.clone(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .expect("indexed child listing should work");
    assert_eq!(children.sessions.len(), 1);
    assert!(children.sessions[0].has_unread);
    assert_eq!(children.sessions[0].unread_final_count, 3);
    assert!(!children.sessions[0].is_running);
    let revision = service
      .attention_revision_for_locator(&child_locator)
      .expect("child attention should be indexed");
    service
      .acknowledge_session_attention(AcknowledgeSessionAttentionRequest {
        session_key: children.sessions[0].session_key.clone(),
        attention_revision: revision,
      })
      .expect("child acknowledgement should succeed");
    let acknowledged = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .expect("indexed listing should work");
    assert!(!acknowledged.sessions[0].has_unread_descendant);
  }

  struct CountingRepository {
    loads: Arc<AtomicUsize>,
  }

  impl ViewerRepository for CountingRepository {
    fn list_session_headers(&self, _provider: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
      Ok(Vec::new())
    }

    fn load_session(&self, _locator: &SessionLocator) -> Result<LoadedSession, String> {
      self.loads.fetch_add(1, Ordering::SeqCst);
      Ok(visible_session(1))
    }
  }

  /// Seeds only the durable metadata surface for tests that exercise sidebar
  /// behavior. It deliberately does not call the repository: that mirrors a
  /// fully indexed app startup and makes unexpected provider reads observable.
  fn service_with_indexed_headers(
    repository: Arc<dyn ViewerRepository>,
    provider_headers: Vec<(ViewerProvider, Vec<SessionHeader>)>,
  ) -> ViewerService {
    let session_index = Arc::new(SessionIndex::open_in_memory().expect("test session index should open"));
    for (provider, headers) in provider_headers {
      let mut sessions_by_source = HashMap::<SourceKey, Vec<SessionMetadata>>::new();
      for header in headers {
        let source_key =
          index_source_key_for_path(provider, &header.path).expect("fixture header path should be valid UTF-8");
        let metadata =
          session_metadata_from_header(&source_key, header, None, false).expect("fixture header should be indexable");
        sessions_by_source.entry(source_key).or_default().push(metadata);
      }
      let mut replacements = sessions_by_source
        .into_iter()
        .map(|(source_key, sessions)| {
          SourceReplacement::new(SourceState::new(source_key, "fixture-source", 0), sessions)
        })
        .collect::<Vec<_>>();
      replacements.push(SourceReplacement::new(
        SourceState::new(index_catalog_source_key(provider), "fixture-catalog", 0),
        Vec::new(),
      ));
      session_index
        .replace_sources(&replacements)
        .expect("fixture catalog should seed");
    }
    ViewerService::new_with_index(repository, session_index)
  }

  #[test]
  fn listing_filters_roots_searches_paginates_and_isolates_provider_errors() {
    let codex_headers = vec![
      session_header("root-new", None, "/projects/Alpha", "2026-06-05T00:00:00Z"),
      session_header(
        "child-hidden",
        Some("root-new"),
        "/projects/Alpha",
        "2026-06-06T00:00:00Z",
      ),
      session_header("root-old", None, "/projects/Beta", "2026-06-01T00:00:00Z"),
    ];
    let pi_headers = vec![session_header("pi-root", None, "/projects/Alpha", "1000")];
    let service = service_with_indexed_headers(
      Arc::new(FakeRepository {
        listings: HashMap::new(),
        loaded: Mutex::new(None),
      }),
      vec![(ViewerProvider::Codex, codex_headers), (ViewerProvider::Pi, pi_headers)],
    );
    service
      .index_errors
      .lock()
      .expect("fixture index errors lock should not be poisoned")
      .insert(ViewerProvider::Dsh, "fixture provider unavailable".to_string());

    let first = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: Vec::new(),
          search: Some("alpha".to_string()),
        },
        cursor: None,
        offset: None,
        limit: Some(1),
      })
      .unwrap();

    assert_eq!(first.sessions.len(), 1);
    assert_eq!(first.sessions[0].session_id, "root-new");
    assert_eq!(first.sessions[0].message_count, None);
    assert!(serde_json::to_value(&first.sessions[0]).unwrap()["message_count"].is_null());
    assert!(first.sessions.iter().all(|session| session.parent_session_id.is_none()));
    assert_eq!(first.source_errors.len(), 1);
    assert_eq!(first.source_errors[0].provider, ViewerProvider::Dsh);
    let next_cursor = first.next_cursor.expect("a second root matches");

    let second = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: Vec::new(),
          search: Some("alpha".to_string()),
        },
        cursor: Some(next_cursor),
        offset: None,
        limit: Some(1),
      })
      .unwrap();
    assert_eq!(second.sessions[0].session_id, "pi-root");
    assert!(second.next_cursor.is_none());
  }

  #[test]
  fn child_listing_is_paged_uses_provider_timestamp_and_keeps_agent_identity_safe() {
    let mut newest_child = session_header("child", Some("root"), "/projects/Alpha", "3000");
    newest_child.path = PathBuf::from("/fixtures/child-new.jsonl");
    // Filesystem times can be rewritten by a sync or restore. The source's
    // provider timestamp, not mtime, decides which duplicate owns this ID.
    newest_child.updated_at = Some("1".to_string());
    newest_child.updated_at_ms = Some(1);
    newest_child.agent_path = Some(" /root/\u{001b}[31mresearcher\u{202e} ".to_string());
    newest_child.agent_nickname = Some(" Hubble\n\t".to_string());
    newest_child.agent_role = Some(" explorer ".to_string());

    let mut older_duplicate = session_header("child", Some("root"), "/projects/Alpha", "2000");
    older_duplicate.path = PathBuf::from("/fixtures/child-old.jsonl");
    older_duplicate.updated_at = Some("9999".to_string());
    older_duplicate.updated_at_ms = Some(9_999);
    older_duplicate.agent_nickname = Some("older duplicate".to_string());

    let mut pi_same_id = session_header("child", Some("root"), "/projects/Pi", "4000");
    pi_same_id.path = PathBuf::from("/fixtures/pi-child.jsonl");
    let codex_headers = vec![
      session_header("root", None, "/projects/Alpha", "1000"),
      older_duplicate,
      newest_child,
      session_header("grandchild", Some("child"), "/projects/Alpha", "2500"),
      session_header("sibling", Some("root"), "/projects/Alpha", "1500"),
    ];
    let service = service_with_indexed_headers(
      Arc::new(FakeRepository {
        listings: HashMap::new(),
        loaded: Mutex::new(None),
      }),
      vec![
        (ViewerProvider::Codex, codex_headers),
        (ViewerProvider::Pi, vec![pi_same_id]),
      ],
    );

    let roots = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    assert_eq!(roots.sessions.len(), 1);
    assert_eq!(roots.sessions[0].session_id, "root");
    assert!(!roots.sessions[0].is_subagent);
    assert_eq!(roots.sessions[0].child_count, 2);

    let children = service
      .list_session_children(ListSessionChildrenRequest {
        parent_session_key: roots.sessions[0].session_key.clone(),
        cursor: None,
        offset: None,
        limit: Some(1),
      })
      .unwrap();
    assert_eq!(children.sessions.len(), 1);
    let child_cursor = children.next_cursor.clone().expect("a second direct child is paged");
    // Last-update time controls the visible order, independently from
    // provider-time canonicalization of duplicate identities.
    assert_eq!(children.sessions[0].session_id, "sibling");

    let second_child_page = service
      .list_session_children(ListSessionChildrenRequest {
        parent_session_key: roots.sessions[0].session_key.clone(),
        cursor: Some(child_cursor),
        offset: None,
        limit: Some(1),
      })
      .unwrap();
    assert_eq!(second_child_page.sessions.len(), 1);
    let child = &second_child_page.sessions[0];
    assert_eq!(child.session_id, "child");
    assert!(child.is_subagent);
    assert_eq!(child.child_count, 1);
    assert_eq!(child.agent_path.as_deref(), Some("/root/researcher"));
    assert_eq!(child.agent_nickname.as_deref(), Some("Hubble"));
    assert_eq!(child.agent_role.as_deref(), Some("explorer"));
    assert!(second_child_page.next_cursor.is_none());

    let grandchildren = service
      .list_session_children(ListSessionChildrenRequest {
        parent_session_key: child.session_key.clone(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    assert_eq!(grandchildren.sessions.len(), 1);
    assert_eq!(grandchildren.sessions[0].session_id, "grandchild");
    assert_eq!(grandchildren.sessions[0].child_count, 0);
  }

  #[test]
  fn session_shares_filter_before_relations_search_and_pagination() {
    let headers = vec![
      session_header("hidden-root", None, "/hidden", "9000"),
      session_header("shared-child", Some("hidden-root"), "/shared", "8000"),
      session_header("hidden-child", Some("shared-child"), "/hidden", "7000"),
      session_header("shared-root", None, "/shared", "6000"),
    ];
    let key = |header: &SessionHeader| encode_session_key(&locator_for_header(ViewerProvider::Codex, header)).unwrap();
    let child_key = key(&headers[1]);
    let root_key = key(&headers[3]);
    let hidden_key = key(&headers[0]);
    let service = service_with_indexed_headers(
      Arc::new(FakeRepository {
        listings: HashMap::new(),
        loaded: Mutex::new(None),
      }),
      vec![(ViewerProvider::Codex, headers)],
    );
    service
      .index_errors
      .lock()
      .unwrap()
      .insert(ViewerProvider::Pi, "secret source path".into());
    let shared = service.scoped_to_sessions(&[child_key.clone(), root_key]).unwrap();
    let first = shared
      .list_sessions(ListSessionsRequest {
        limit: Some(1),
        ..Default::default()
      })
      .unwrap();
    assert_eq!(first.sessions.len(), 1);
    assert_eq!(first.sessions[0].session_id, "shared-child");
    assert_eq!(first.sessions[0].parent_session_id, None);
    assert!(!first.sessions[0].is_subagent);
    assert_eq!(first.sessions[0].child_count, 0);
    assert!(!first.sessions[0].has_unread_descendant);
    assert!(first.pending_providers.is_empty());
    assert!(first.source_errors.is_empty());
    let next = shared
      .list_sessions(ListSessionsRequest {
        cursor: first.next_cursor,
        limit: Some(1),
        ..Default::default()
      })
      .unwrap();
    assert_eq!(next.sessions[0].session_id, "shared-root");
    assert!(next.next_cursor.is_none());
    let search = shared
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          search: Some("hidden".into()),
          ..Default::default()
        },
        ..Default::default()
      })
      .unwrap();
    assert!(search.sessions.is_empty());
    assert!(search.next_cursor.is_none());
    let children = shared
      .list_session_children(ListSessionChildrenRequest {
        parent_session_key: child_key.clone(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    assert!(children.sessions.is_empty());
    assert!(shared.validate_session_key(&hidden_key).is_err());
    assert!(shared.scoped_to_sessions(&[hidden_key.clone()]).is_err());
    assert!(shared.load_verified(&decode_session_key(&hidden_key).unwrap()).is_err());
    assert!(
      shared
        .delegation_targets_for_parent(&decode_session_key(&child_key).unwrap())
        .direct_children
        .is_empty()
    );
    assert_eq!(
      service.list_sessions(ListSessionsRequest::default()).unwrap().sessions[0].session_id,
      "hidden-root"
    );
  }

  #[test]
  fn relation_index_promotes_orphans_and_breaks_cycles_without_losing_sessions() {
    let service = service_with_indexed_headers(
      Arc::new(FakeRepository {
        listings: HashMap::new(),
        loaded: Mutex::new(None),
      }),
      vec![(
        ViewerProvider::Codex,
        vec![
          session_header("orphan", Some("missing"), "/projects/Alpha", "4000"),
          session_header("cycle-a", Some("cycle-b"), "/projects/Alpha", "3000"),
          session_header("cycle-b", Some("cycle-a"), "/projects/Alpha", "2000"),
        ],
      )],
    );

    let roots = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery::default(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    let root_ids = roots
      .sessions
      .iter()
      .map(|session| session.session_id.as_str())
      .collect::<Vec<_>>();
    assert!(root_ids.contains(&"orphan"));
    assert!(root_ids.contains(&"cycle-b"));
    assert!(roots.sessions.iter().all(|session| !session.is_subagent));

    let cycle_root = roots
      .sessions
      .iter()
      .find(|session| session.session_id == "cycle-b")
      .expect("a cycle node becomes a root");
    let children = service
      .list_session_children(ListSessionChildrenRequest {
        parent_session_key: cycle_root.session_key.clone(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    assert_eq!(children.sessions.len(), 1);
    assert_eq!(children.sessions[0].session_id, "cycle-a");

    let descendant_children = service
      .list_session_children(ListSessionChildrenRequest {
        parent_session_key: children.sessions[0].session_key.clone(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    assert!(descendant_children.sessions.is_empty());
  }

  #[test]
  fn listing_isolates_a_malformed_index_row_to_its_provider_warning() {
    let session_index = Arc::new(SessionIndex::open_in_memory().unwrap());
    let source_key = SourceKey::new(ViewerProvider::Codex.as_str(), "fixture-source");
    let invalid_key = IndexedSessionKey::new(ViewerProvider::Codex.as_str(), source_key.source_key.clone(), "invalid");
    let valid_key = IndexedSessionKey::new(ViewerProvider::Codex.as_str(), source_key.source_key.clone(), "valid");
    let invalid = SessionMetadata::new(invalid_key, "");
    let valid = SessionMetadata::new(valid_key, "/fixtures/valid.jsonl");
    session_index
      .replace_sources(&[
        SourceReplacement::new(SourceState::new(source_key, "fixture-source", 0), vec![invalid, valid]),
        SourceReplacement::new(
          SourceState::new(index_catalog_source_key(ViewerProvider::Codex), "fixture-catalog", 0),
          Vec::new(),
        ),
      ])
      .unwrap();
    let service = ViewerService::new_with_index(
      Arc::new(FakeRepository {
        listings: HashMap::from([(
          ViewerProvider::Codex,
          Err("sidebar must not read this fixture".to_string()),
        )]),
        loaded: Mutex::new(None),
      }),
      session_index,
    );

    let response = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery::default(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();

    assert_eq!(response.sessions.len(), 1);
    assert_eq!(response.sessions[0].session_id, "valid");
    assert_eq!(response.source_errors.len(), 1);
    assert_eq!(response.source_errors[0].provider, ViewerProvider::Codex);
  }

  #[test]
  fn session_metadata_and_agent_identity_are_sanitized_and_bounded() {
    let mut header = session_header("session-id", None, "/projects/Alpha", "1000");
    header.agent_nickname = Some("worker\u{202e}-name".to_string());
    header.agent_role = Some(format!("role {}", "x".repeat(MAX_AGENT_IDENTITY_CHARS + 20)));
    header.title = Some(" \u{001b}[31mBuild\u{202e}\n\tthe viewer\u{001b}[0m ".to_string());
    header.preview = Some(format!("Prompt {}", "x".repeat(MAX_SESSION_PREVIEW_CHARS + 20)));

    let summary = session_summary(ViewerProvider::Pi, header).unwrap();

    assert_eq!(summary.title.as_deref(), Some("Build the viewer"));
    assert_eq!(
      summary.preview.as_ref().unwrap().chars().count(),
      MAX_SESSION_PREVIEW_CHARS
    );
    assert!(summary.preview.as_ref().unwrap().ends_with('\u{2026}'));
    assert_eq!(summary.agent_nickname.as_deref(), Some("worker-name"));
    assert_eq!(
      summary.agent_role.as_ref().unwrap().chars().count(),
      MAX_AGENT_IDENTITY_CHARS
    );
    assert!(summary.agent_role.as_ref().unwrap().ends_with('\u{2026}'));
  }

  #[test]
  fn search_matches_native_titles_and_first_message_previews() {
    let mut title_header = session_header("title", None, "/projects/Alpha", "2000");
    title_header.title = Some("Release checklist".to_string());
    let mut preview_header = session_header("preview", None, "/projects/Alpha", "1000");
    preview_header.preview = Some("Investigate socket ownership".to_string());
    let service = service_with_indexed_headers(
      Arc::new(FakeRepository {
        listings: HashMap::new(),
        loaded: Mutex::new(None),
      }),
      vec![(ViewerProvider::Codex, vec![title_header, preview_header])],
    );

    let response = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: Some("socket ownership".to_string()),
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();

    assert_eq!(response.sessions.len(), 1);
    assert_eq!(response.sessions[0].session_id, "preview");
  }

  #[test]
  fn cold_sidebar_listing_reports_pending_catalogs_without_provider_reads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cold.jsonl");
    std::fs::write(&path, "fixture\n").unwrap();
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: indexed_header(path, "cold", None),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let viewer_repository: Arc<dyn ViewerRepository> = repository.clone();
    let service = ViewerService::new(viewer_repository);

    let response = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();

    assert!(response.sessions.is_empty());
    assert_eq!(response.pending_providers, vec![ViewerProvider::Codex]);
    assert!(response.source_errors.is_empty());
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 0);
  }

  #[test]
  fn indexed_sidebar_queries_use_no_provider_reads_until_explicit_timeline_load() {
    let mut parent = session_header("parent", None, "/projects/Alpha", "2000");
    parent.title = Some("Indexed parent".to_string());
    let mut child = session_header("child", Some("parent"), "/projects/Alpha", "1000");
    child.preview = Some("Indexed child preview".to_string());
    let repository = indexing_repository(vec![
      IndexedLoadSpec {
        header: parent.clone(),
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      },
      IndexedLoadSpec {
        header: child.clone(),
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      },
    ]);
    let viewer_repository: Arc<dyn ViewerRepository> = repository.clone();
    let service = service_with_indexed_headers(viewer_repository, vec![(ViewerProvider::Codex, vec![parent, child])]);

    let roots = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: None,
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    assert_eq!(roots.sessions.len(), 1);
    let parent_session_key = roots.sessions[0].session_key.clone();

    let search = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery {
          order: SessionOrder::Time,
          providers: vec![ViewerProvider::Codex],
          search: Some("indexed child preview".to_string()),
        },
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    assert!(
      search.sessions.is_empty(),
      "root search intentionally excludes child rows"
    );

    let children = service
      .list_session_children(ListSessionChildrenRequest {
        parent_session_key: parent_session_key.clone(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();
    assert_eq!(children.sessions.len(), 1);
    let parent_locator = decode_session_key(&parent_session_key).unwrap();
    assert!(
      service
        .delegation_targets_for_parent(&parent_locator)
        .direct_children
        .contains_key("child")
    );
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 0);

    let page = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: parent_session_key,
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    assert_eq!(page.total_events, 1);
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn body_backfill_persists_loaded_presentation_and_catalog_keeps_it_while_pending() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("body-presentation.jsonl");
    std::fs::write(&path, "first body\n").unwrap();
    let mut header = indexed_header(path.clone(), "body-presentation", None);
    header.title = None;
    header.preview = None;
    let locator = locator_for_header(ViewerProvider::Codex, &header);
    let repository = indexing_repository(vec![IndexedLoadSpec {
      header: header.clone(),
      messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
    }]);
    let mut first_loaded_header = header.clone();
    first_loaded_header.title = Some("Derived first title".to_string());
    first_loaded_header.preview = Some("Derived first preview".to_string());
    repository.loads.lock().unwrap().insert(
      locator.clone(),
      Ok(IndexedLoadSpec {
        header: first_loaded_header,
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      }),
    );
    let viewer_repository: Arc<dyn ViewerRepository> = repository.clone();
    let service = ViewerService::new(viewer_repository);
    service.refresh_session_index().unwrap();

    let request = ListSessionsRequest {
      query: SessionQuery {
        order: SessionOrder::Time,
        providers: vec![ViewerProvider::Codex],
        search: None,
      },
      cursor: None,
      offset: None,
      limit: None,
    };
    let first = service.list_sessions(request.clone()).unwrap();
    assert_eq!(first.sessions[0].title.as_deref(), Some("Derived first title"));
    assert_eq!(first.sessions[0].preview.as_deref(), Some("Derived first preview"));

    // The first catalog after the final body completion upgrades the staged
    // source cursor from pending to completed. Once that one-time transition
    // settles, a blank header still matches its raw catalog values; the
    // effective fallback must not force a replacement every ten seconds.
    assert!(service.refresh_provider_catalog(ViewerProvider::Codex).unwrap().changed);
    let no_op_catalog = service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    assert!(!no_op_catalog.changed);

    std::fs::write(&path, "second body with a changed revision\n").unwrap();
    let mut refreshed_loaded_header = header.clone();
    refreshed_loaded_header.title = Some("Derived refreshed title".to_string());
    refreshed_loaded_header.preview = Some("Derived refreshed preview".to_string());
    repository.loads.lock().unwrap().insert(
      locator.clone(),
      Ok(IndexedLoadSpec {
        header: refreshed_loaded_header,
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      }),
    );
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();

    let pending = service.list_sessions(request.clone()).unwrap();
    assert_eq!(pending.sessions[0].title.as_deref(), Some("Derived first title"));
    assert_eq!(pending.sessions[0].preview.as_deref(), Some("Derived first preview"));

    service.refresh_pending_session_index().unwrap();
    let refreshed = service.list_sessions(request.clone()).unwrap();
    assert_eq!(refreshed.sessions[0].title.as_deref(), Some("Derived refreshed title"));
    assert_eq!(
      refreshed.sessions[0].preview.as_deref(),
      Some("Derived refreshed preview")
    );

    // A successful body load with no presentation fields is authoritative for
    // the current source revision: it deliberately clears the prior backfill.
    std::fs::write(&path, "third body with cleared presentation\n").unwrap();
    repository.loads.lock().unwrap().insert(
      locator,
      Ok(IndexedLoadSpec {
        header,
        messages: vec![indexed_message(Role::User, MessageDelivery::Unspecified)],
      }),
    );
    service.refresh_provider_catalog(ViewerProvider::Codex).unwrap();
    let still_pending = service.list_sessions(request.clone()).unwrap();
    assert_eq!(
      still_pending.sessions[0].title.as_deref(),
      Some("Derived refreshed title")
    );
    assert_eq!(
      still_pending.sessions[0].preview.as_deref(),
      Some("Derived refreshed preview")
    );

    service.refresh_pending_session_index().unwrap();
    let cleared = service.list_sessions(request).unwrap();
    assert_eq!(cleared.sessions[0].title, None);
    assert_eq!(cleared.sessions[0].preview, None);
  }

  #[test]
  fn event_pages_are_bounded_and_keep_absolute_event_keys() {
    let service = service_with_session(visible_session(5));
    let session_key = key_for("fixture");

    let page = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key,
        cursor: None,
        offset: Some(1),
        direction: PageDirection::Forward,
        limit: Some(2),
      })
      .unwrap();

    assert_eq!(page.events.len(), 2);
    assert_eq!(decode_event_key(&page.events[0].event_key).unwrap(), 1);
    assert_eq!(decode_event_key(&page.events[1].event_key).unwrap(), 2);
    assert_eq!(page.events[0].role.as_deref(), Some("assistant"));
    assert_eq!(page.events[0].summary, "message 1");
    assert!(!page.events[0].summary_truncated);
    assert_eq!(decode_event_cursor(page.next_cursor.as_deref().unwrap()).unwrap(), 3);
    assert_eq!(
      decode_event_cursor(page.previous_cursor.as_deref().unwrap()).unwrap(),
      1
    );
    assert_eq!(page.total_events, 5);
  }

  #[test]
  fn trajectory_cards_collapse_visible_work_runs_and_keep_boundaries_flat() {
    let events = vec![
      AgentEvent::SessionStarted(SessionStarted {
        provider: Provider::Codex,
        session_id: "fixture".to_string(),
        cwd: None,
        timestamp: Some("2026-09-01T00:00:00Z".to_string()),
      }),
      metadata_event("turn context"),
      with_timestamp(
        reasoning_event(Some("consider options"), None, None, None, None),
        "2026-09-01T00:01:00Z",
      ),
      with_timestamp(
        usage_event(UsageKind::ModelCall, Provider::Codex),
        "2026-09-01T00:02:00Z",
      ),
      with_timestamp(
        AgentEvent::Unknown(UnknownEvent {
          provider: Provider::Codex,
          session_id: Some("fixture".to_string()),
          native_type: Some("future_event".to_string()),
          native: None,
          timestamp: None,
        }),
        "2026-09-01T00:03:00Z",
      ),
      with_timestamp(
        AgentEvent::Error(ErrorEvent {
          provider: Provider::Codex,
          session_id: Some("fixture".to_string()),
          message: "command failed".to_string(),
          timestamp: None,
        }),
        "2026-09-01T00:04:00Z",
      ),
      message_event("A visible assistant message"),
      metadata_event("turn metadata without work"),
      AgentEvent::ProviderChanged(ProviderChanged {
        provider: Provider::Codex,
        session_id: Some("fixture".to_string()),
        native_id: None,
        native_parent_id: None,
        model_provider: None,
        model_id: None,
        thinking_level: None,
        timestamp: None,
      }),
      message_event_with_role("follow-up progress", Role::Assistant, MessageDelivery::Commentary),
      agent_activity("child", Some("/root/reviewer")),
    ];

    let page = service_with_session(loaded_session(events))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(
      page
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      [
        "session_started",
        "trajectory",
        "message",
        "metadata",
        "provider_changed",
        "trajectory"
      ]
    );
    let trajectory = page.events[1].trajectory.as_ref().expect("work run is collapsed");
    assert_eq!(page.events[1].event_key, encode_trajectory_key(1));
    assert!(decode_event_key(&page.events[1].event_key).is_err());
    assert_eq!(trajectory.event_count, 5);
    assert_eq!(trajectory.source_event_count, 5);
    assert_eq!(trajectory.reasoning_count, 1);
    assert_eq!(trajectory.usage_count, 1);
    assert_eq!(trajectory.error_count, 1);
    assert_eq!(trajectory.unknown_count, 1);
    assert_eq!(trajectory.started_at.as_deref(), Some("2026-09-01T00:01:00Z"));
    assert_eq!(trajectory.ended_at.as_deref(), Some("2026-09-01T00:04:00Z"));
    assert_eq!(trajectory.duration_ms.as_deref(), Some("180000"));
    assert_eq!(page.events[5].event_key, encode_trajectory_key(9));
  }

  #[test]
  fn trajectory_folds_non_final_assistant_messages_but_keeps_conversation_boundaries_visible() {
    let events = vec![
      message_event_with_role("My request", Role::User, MessageDelivery::Unspecified),
      with_timestamp(
        message_event_with_role("I will inspect this", Role::Assistant, MessageDelivery::Commentary),
        "2026-09-01T00:00:00Z",
      ),
      metadata_event("turn context"),
      with_timestamp(
        message_event_with_role("legacy progress", Role::Assistant, MessageDelivery::Unspecified),
        "2026-09-01T00:00:30Z",
      ),
      message_event("Final answer"),
      message_event_with_role("System note", Role::System, MessageDelivery::Unspecified),
      message_event_with_role("Tool output", Role::Tool, MessageDelivery::Unspecified),
      message_event_with_role("Unknown message", Role::Unknown, MessageDelivery::Unspecified),
      with_timestamp(
        message_event_with_role("standalone progress", Role::Assistant, MessageDelivery::Commentary),
        "2026-09-01T00:01:00Z",
      ),
      message_event("Second final answer"),
    ];
    let directory = tempfile::tempdir().unwrap();
    let session_key = key_for_cached_source(&directory, "fixture");
    let service = service_with_session(loaded_session(events));

    let first = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(2),
      })
      .unwrap();

    assert_eq!(first.total_events, 8);
    assert_eq!(
      first
        .events
        .iter()
        .map(|event| (event.event_type.as_str(), event.summary.as_str()))
        .collect::<Vec<_>>(),
      [("message", "My request"), ("trajectory", "3 events")]
    );
    assert_eq!(first.events[0].role.as_deref(), Some("user"));
    let first_trajectory = first.events[1].trajectory.as_ref().unwrap();
    assert_eq!(first_trajectory.event_count, 3);
    assert_eq!(first_trajectory.started_at.as_deref(), Some("2026-09-01T00:00:00Z"));
    assert_eq!(first_trajectory.ended_at.as_deref(), Some("2026-09-01T00:00:30Z"));
    assert_eq!(first_trajectory.duration_ms.as_deref(), Some("30000"));

    let trajectory_key = first.events[1].event_key.clone();
    let trajectory_first_page = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: session_key.clone(),
        trajectory_key: trajectory_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(2),
      })
      .unwrap();
    assert_eq!(trajectory_first_page.total_events, 3);
    assert_eq!(
      trajectory_first_page
        .events
        .iter()
        .map(|event| (event.event_type.as_str(), event.summary.as_str()))
        .collect::<Vec<_>>(),
      [
        ("message", "I will inspect this"),
        ("metadata", "[fixture_metadata] turn context"),
      ]
    );
    let trajectory_second_page = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: session_key.clone(),
        trajectory_key,
        cursor: trajectory_first_page.next_cursor,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(2),
      })
      .unwrap();
    assert_eq!(trajectory_second_page.events.len(), 1);
    assert_eq!(trajectory_second_page.events[0].summary, "legacy progress");
    assert_eq!(trajectory_second_page.events[0].role.as_deref(), Some("assistant"));

    let second = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key,
        cursor: first.next_cursor,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(6),
      })
      .unwrap();
    assert_eq!(
      second
        .events
        .iter()
        .map(|event| (event.event_type.as_str(), event.summary.as_str()))
        .collect::<Vec<_>>(),
      [
        ("message", "Final answer"),
        ("message", "System note"),
        ("message", "Tool output"),
        ("message", "Unknown message"),
        ("trajectory", "1 events"),
        ("message", "Second final answer"),
      ]
    );
    assert_eq!(second.events[0].role.as_deref(), Some("assistant"));
    assert_eq!(second.events[1].role.as_deref(), Some("system"));
    assert_eq!(second.events[2].role.as_deref(), Some("tool"));
    assert_eq!(second.events[3].role.as_deref(), Some("unknown"));
    assert_eq!(second.events[4].trajectory.as_ref().unwrap().event_count, 1);
  }

  #[test]
  fn post_final_session_bookkeeping_stays_flat_until_the_next_user_prompt() {
    let events = vec![
      message_event_with_role("First request", Role::User, MessageDelivery::Unspecified),
      message_event_with_role("I am working", Role::Assistant, MessageDelivery::Commentary),
      message_event("First answer"),
      usage_event(UsageKind::SessionSnapshot, Provider::Codex),
      settings_event(),
      metadata_event("post-final context"),
      message_event_with_role("Second request", Role::User, MessageDelivery::Unspecified),
    ];
    let directory = tempfile::tempdir().unwrap();
    let session_key = key_for_cached_source(&directory, "fixture");
    let service = service_with_session(loaded_session(events));
    let first = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(4),
      })
      .unwrap();

    assert_eq!(first.total_events, 7);
    assert_eq!(
      first
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["message", "trajectory", "message", "usage"]
    );
    assert_eq!(first.events[1].event_key, encode_trajectory_key(1));
    assert_eq!(first.events[3].event_key, encode_event_key(3));
    assert!(first.events[3].trajectory.is_none());

    let second = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key,
        cursor: first.next_cursor,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(4),
      })
      .unwrap();
    assert_eq!(
      second
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["session_settings_applied", "metadata", "message"]
    );
    assert!(second.events.iter().all(|event| event.trajectory.is_none()));
  }

  #[test]
  fn post_final_session_bookkeeping_stays_flat_at_end_of_session() {
    let events = vec![
      message_event_with_role("Request", Role::User, MessageDelivery::Unspecified),
      message_event_with_role("I am working", Role::Assistant, MessageDelivery::Commentary),
      message_event("Answer"),
      usage_event(UsageKind::SessionSnapshot, Provider::Codex),
      settings_event(),
      metadata_event("post-final context"),
    ];
    let page = service_with_session(loaded_session(events))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(
      page
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      [
        "message",
        "trajectory",
        "message",
        "usage",
        "session_settings_applied",
        "metadata"
      ]
    );
    assert_eq!(
      page
        .events
        .iter()
        .filter(|event| event.event_type == "trajectory")
        .count(),
      1
    );
  }

  #[test]
  fn session_snapshot_alone_does_not_form_a_trajectory() {
    let events = vec![
      message_event_with_role("Request", Role::User, MessageDelivery::Unspecified),
      usage_event(UsageKind::SessionSnapshot, Provider::Codex),
    ];
    let page = service_with_session(loaded_session(events))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(
      page
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["message", "usage"]
    );
  }

  #[test]
  fn model_call_usage_is_flat_after_a_final_but_substantive_before_one() {
    let pre_final = service_with_session(loaded_session(vec![
      message_event_with_role("Request", Role::User, MessageDelivery::Unspecified),
      usage_event(UsageKind::ModelCall, Provider::Codex),
      message_event("Answer"),
    ]))
    .load_event_page(EventPageRequest {
      window_mode: None,
      session_key: key_for("fixture"),
      cursor: None,
      offset: None,
      direction: PageDirection::Forward,
      limit: None,
    })
    .unwrap();
    assert_eq!(
      pre_final
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["message", "trajectory", "message"]
    );
    assert_eq!(pre_final.events[1].trajectory.as_ref().unwrap().usage_count, 1);

    let post_final = service_with_session(loaded_session(vec![
      message_event("Answer"),
      usage_event(UsageKind::ModelCall, Provider::Codex),
    ]))
    .load_event_page(EventPageRequest {
      window_mode: None,
      session_key: key_for("fixture"),
      cursor: None,
      offset: None,
      direction: PageDirection::Forward,
      limit: None,
    })
    .unwrap();
    assert_eq!(
      post_final
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["message", "usage"]
    );
  }

  #[test]
  fn genuine_progress_after_a_final_reply_forms_the_next_trajectory() {
    let events = vec![
      message_event("First answer"),
      usage_event(UsageKind::SessionSnapshot, Provider::Codex),
      message_event_with_role("I will continue", Role::Assistant, MessageDelivery::Commentary),
      tool_call(
        Provider::Codex,
        "shell",
        "follow-up-tool",
        ToolKind::Shell,
        None,
        Phase::Finished,
        None,
      ),
      message_event("Second answer"),
    ];
    let page = service_with_session(loaded_session(events))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(
      page
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["message", "usage", "trajectory", "message"]
    );
    let trajectory = page.events[2].trajectory.as_ref().expect("follow-up work is folded");
    assert_eq!(trajectory.event_count, 2);
    assert_eq!(trajectory.tool_count, 1);
    assert_eq!(page.events[2].event_key, encode_trajectory_key(2));
  }

  #[test]
  fn active_work_after_a_user_prompt_still_folds_at_end_of_session() {
    let events = vec![
      message_event_with_role("Request", Role::User, MessageDelivery::Unspecified),
      message_event_with_role("I am working", Role::Assistant, MessageDelivery::Commentary),
    ];
    let page = service_with_session(loaded_session(events))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(
      page
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["message", "trajectory"]
    );
    assert_eq!(page.events[1].event_key, encode_trajectory_key(1));
  }

  #[test]
  fn top_level_trajectory_paging_uses_projected_cards_without_losing_messages() {
    let events = vec![
      message_event_with_role("before", Role::User, MessageDelivery::Unspecified),
      with_timestamp(
        reasoning_event(Some("one"), None, None, None, None),
        "2026-09-01T00:01:00Z",
      ),
      with_timestamp(
        usage_event(UsageKind::ModelCall, Provider::Codex),
        "2026-09-01T00:02:00Z",
      ),
      message_event("between"),
      message_event_with_role("follow-up progress", Role::Assistant, MessageDelivery::Commentary),
      with_timestamp(lifecycle_event(), "2026-09-01T00:03:00Z"),
      with_timestamp(
        AgentEvent::Unknown(UnknownEvent {
          provider: Provider::Codex,
          session_id: Some("fixture".to_string()),
          native_type: Some("future_event".to_string()),
          native: None,
          timestamp: None,
        }),
        "2026-09-01T00:04:00Z",
      ),
      message_event("after"),
    ];
    let directory = tempfile::tempdir().unwrap();
    let session_key = key_for_cached_source(&directory, "fixture");
    let service = service_with_session(loaded_session(events));

    let first = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(2),
      })
      .unwrap();
    assert_eq!(first.total_events, 5);
    assert_eq!(
      first
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["message", "trajectory"]
    );
    assert_eq!(first.events[1].event_key, encode_trajectory_key(1));

    let second = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: first.next_cursor.clone(),
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(2),
      })
      .unwrap();
    assert_eq!(
      second
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["message", "trajectory"]
    );
    assert_eq!(second.events[0].summary, "between");
    assert_eq!(second.events[1].event_key, encode_trajectory_key(4));

    let third = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key,
        cursor: second.next_cursor.clone(),
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(2),
      })
      .unwrap();
    assert_eq!(third.events.len(), 1);
    assert_eq!(third.events[0].event_type, "message");
    assert_eq!(third.events[0].summary, "after");
    assert!(third.next_cursor.is_none());
  }

  #[test]
  fn hidden_records_are_boundaries_and_metadata_only_runs_stay_flat() {
    let hidden_reasoning = AgentEvent::Reasoning(ReasoningEvent {
      provenance: Some(MessageProvenance {
        source: json!({"kind": "fixture"}),
        display: Some(false),
        native: Some(json!({"secret": "hidden"})),
        surface_op: None,
        source_event_seqs: None,
      }),
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      message_id: Some("hidden-reasoning".to_string()),
      parent_id: None,
      phase: Phase::Finished,
      text: Some("hidden".to_string()),
      summary: None,
      redacted: None,
      encrypted_content: None,
      signature: None,
      timestamp: None,
    });
    let hidden_message = AgentEvent::Message(MessageEvent {
      provenance: Some(MessageProvenance {
        source: json!({"kind": "fixture"}),
        display: Some(false),
        native: None,
        surface_op: None,
        source_event_seqs: None,
      }),
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      message_id: Some("hidden-message".to_string()),
      parent_id: None,
      role: Role::System,
      delivery: MessageDelivery::Unspecified,
      phase: Phase::Finished,
      text: "hidden".to_string(),
      timestamp: None,
    });
    let events = vec![
      metadata_event("metadata before hidden message"),
      hidden_message,
      reasoning_event(Some("visible work"), None, None, None, None),
      hidden_reasoning,
      metadata_event("metadata without work"),
      message_event("visible message"),
    ];

    let page = service_with_session(loaded_session(events))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(
      page
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>(),
      ["metadata", "message", "trajectory", "reasoning", "metadata", "message"]
    );
    assert!(page.events[1].is_hidden);
    assert!(page.events[3].is_hidden);
    assert!(page.events[2].trajectory.is_some());
    assert!(page.events[0].trajectory.is_none());
    assert!(page.events[4].trajectory.is_none());
  }

  #[test]
  fn trajectory_duration_requires_ordered_parseable_provider_timestamps() {
    let events = vec![
      reasoning_event(Some("missing"), None, None, None, None),
      with_timestamp(usage_event(UsageKind::ModelCall, Provider::Codex), "not-a-time"),
      message_event("boundary"),
      message_event_with_role("follow-up progress", Role::Assistant, MessageDelivery::Commentary),
      with_timestamp(
        reasoning_event(Some("late first"), None, None, None, None),
        "2026-09-01T00:02:00Z",
      ),
      with_timestamp(
        usage_event(UsageKind::ModelCall, Provider::Codex),
        "2026-09-01T00:01:00Z",
      ),
    ];

    let page = service_with_session(loaded_session(events))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    let missing = page.events[0].trajectory.as_ref().unwrap();
    assert!(missing.started_at.is_none());
    assert!(missing.ended_at.is_none());
    assert!(missing.duration_ms.is_none());

    let reversed = page.events[2].trajectory.as_ref().unwrap();
    assert_eq!(reversed.started_at.as_deref(), Some("2026-09-01T00:02:00Z"));
    assert_eq!(reversed.ended_at.as_deref(), Some("2026-09-01T00:01:00Z"));
    assert!(reversed.duration_ms.is_none());
  }

  #[test]
  fn trajectory_detail_is_aggregate_while_child_event_keys_keep_independent_details() {
    let mut invocation = tool_call(
      Provider::Codex,
      "exec_command",
      "call-1",
      ToolKind::Shell,
      None,
      Phase::Started,
      None,
    );
    let AgentEvent::ToolCall(invocation_event) = &mut invocation else {
      unreachable!();
    };
    invocation_event.native = Some(json!({"record": "invocation"}));
    let mut result = tool_call(
      Provider::Codex,
      "exec_command",
      "call-1",
      ToolKind::Shell,
      None,
      Phase::Finished,
      Some(json!({"text": "final"})),
    );
    let AgentEvent::ToolCall(result_event) = &mut result else {
      unreachable!();
    };
    result_event.native = Some(json!({"record": "result"}));

    let directory = tempfile::tempdir().unwrap();
    let session_key = key_for_cached_source(&directory, "fixture");
    let service = service_with_session(loaded_session(vec![invocation, result]));
    let page = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    let trajectory_key = page.events[0].event_key.clone();
    assert_eq!(trajectory_key, encode_trajectory_key(0));

    let outer = service
      .load_event_detail(LoadEventDetailRequest {
        session_key: session_key.clone(),
        event_key: trajectory_key.clone(),
      })
      .unwrap();
    assert_eq!(outer.event_key, trajectory_key);
    assert_eq!(outer.event["type"], "trajectory");
    assert_eq!(outer.event["source_event_count"], 2);
    assert_eq!(outer.event["source_records"].as_array().unwrap().len(), 2);
    assert_eq!(
      outer.native.as_ref().unwrap()["source_records"]
        .as_array()
        .unwrap()
        .len(),
      2
    );

    let children = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: session_key.clone(),
        trajectory_key,
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    assert_eq!(children.events.len(), 1);
    assert_eq!(children.events[0].event_type, "tool_call");
    let child_key = children.events[0].event_key.clone();
    assert_eq!(child_key, encode_event_key(0));

    let inner = service
      .load_event_detail(LoadEventDetailRequest {
        session_key: session_key.clone(),
        event_key: child_key,
      })
      .unwrap();
    assert_eq!(inner.event["source_event_indices"], json!([0, 1]));
    assert_ne!(inner.event["type"], "trajectory");

    let terminal_source = service
      .load_event_detail(LoadEventDetailRequest {
        session_key,
        event_key: encode_event_key(1),
      })
      .unwrap();
    assert_eq!(terminal_source.event["source_event_indices"], json!([0, 1]));
  }

  #[test]
  fn trajectory_pages_are_bounded_and_validate_their_stable_key_and_cursor() {
    let events = vec![
      reasoning_event(Some("first"), None, None, None, None),
      usage_event(UsageKind::ModelCall, Provider::Codex),
      lifecycle_event(),
    ];
    let directory = tempfile::tempdir().unwrap();
    let session_key = key_for_cached_source(&directory, "fixture");
    let service = service_with_session(loaded_session(events));
    let trajectory_key = encode_trajectory_key(0);

    let first = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: session_key.clone(),
        trajectory_key: trajectory_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(1),
      })
      .unwrap();
    assert_eq!(first.total_events, 3);
    assert_eq!(first.events[0].event_key, encode_event_key(0));
    let cursor = first.next_cursor.clone().unwrap();

    let second = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: session_key.clone(),
        trajectory_key: trajectory_key.clone(),
        cursor: Some(cursor),
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(1),
      })
      .unwrap();
    assert_eq!(second.events[0].event_key, encode_event_key(1));

    let wrong_key = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: session_key.clone(),
        trajectory_key: encode_trajectory_key(1),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(1),
      })
      .unwrap_err();
    assert_eq!(wrong_key, "trajectory key is outside the session");

    let wrong_cursor = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key,
        trajectory_key,
        cursor: Some(encode_trajectory_event_cursor(99, 1)),
        offset: None,
        direction: PageDirection::Forward,
        limit: Some(1),
      })
      .unwrap_err();
    assert_eq!(
      wrong_cursor,
      "trajectory cursor does not match the requested trajectory"
    );
  }

  #[test]
  fn trajectory_key_remains_resolvable_when_an_active_tool_finishes() {
    let invocation = || {
      tool_call(
        Provider::Codex,
        "exec_command",
        "call-1",
        ToolKind::Shell,
        None,
        Phase::Started,
        None,
      )
    };
    let commentary = || message_event_with_role("still working", Role::Assistant, MessageDelivery::Commentary);
    let result = tool_call(
      Provider::Codex,
      "exec_command",
      "call-1",
      ToolKind::Shell,
      None,
      Phase::Finished,
      Some(json!({"stdout": "done"})),
    );
    let before = loaded_session(vec![invocation(), commentary()]);
    let after = loaded_session(vec![invocation(), commentary(), result]);
    let service = ViewerService::new(Arc::new(SequenceRepository {
      loaded: Mutex::new(VecDeque::from([before, after])),
    }));
    let session_key = key_for("fixture");

    let outer = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    let trajectory_key = outer.events[0].event_key.clone();
    assert_eq!(trajectory_key, encode_trajectory_key(0));

    let inner = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key,
        trajectory_key,
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    assert_eq!(inner.total_events, 2);
    assert!(inner.events.iter().any(|event| event.event_type == "tool_call"));
  }

  #[test]
  fn event_page_resolves_agent_activity_to_its_canonical_direct_child() {
    let parent = session_header("parent", None, "/projects/Alpha", "2026-08-31T00:00:00Z");
    let mut child = session_header("child", Some("parent"), "/projects/Alpha", "2026-08-31T00:02:00Z");
    child.title = Some("Current child".to_string());
    child.agent_nickname = Some("Hubble".to_string());
    child.agent_role = Some("researcher".to_string());

    let mut stale_child = session_header("child", Some("parent"), "/projects/Alpha", "2026-08-31T00:01:00Z");
    stale_child.path = PathBuf::from("/fixtures/child-stale.jsonl");
    stale_child.title = Some("Stale child".to_string());

    let service = service_with_indexed_headers(
      Arc::new(FakeRepository {
        listings: HashMap::new(),
        loaded: Mutex::new(Some(loaded_session_for(
          "parent",
          vec![agent_activity("child", Some("/root/researcher"))],
        ))),
      }),
      vec![(ViewerProvider::Codex, vec![parent, stale_child, child])],
    );

    let page = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: key_for_header("parent"),
        trajectory_key: encode_trajectory_key(0),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    let activity = page.events[0]
      .agent_activity
      .as_ref()
      .expect("nested agent activity card should be projected");
    assert_eq!(activity.kind, "started");
    assert_eq!(activity.target_session_id.as_deref(), Some("child"));
    assert_eq!(activity.target_agent_path.as_deref(), Some("/root/researcher"));
    let target = activity.target.as_ref().expect("direct child should be safe to open");
    assert_eq!(target.session_id, "child");
    assert_eq!(target.title.as_deref(), Some("Current child"));
    assert_eq!(target.agent_nickname.as_deref(), Some("Hubble"));
    assert_eq!(target.agent_role.as_deref(), Some("researcher"));
    assert!(target.is_subagent);
    assert_eq!(
      decode_session_key(&target.session_key).unwrap().source_path,
      PathBuf::from("/fixtures/child.jsonl")
    );
  }

  #[test]
  fn event_page_keeps_non_child_agent_activity_target_unlinked() {
    let parent = session_header("parent", None, "/projects/Alpha", "2026-08-31T00:00:00Z");
    let unrelated = session_header(
      "target",
      Some("other-parent"),
      "/projects/Alpha",
      "2026-08-31T00:01:00Z",
    );
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::from([(ViewerProvider::Codex, Ok(vec![parent, unrelated]))]),
      loaded: Mutex::new(Some(loaded_session_for(
        "parent",
        vec![agent_activity("target", Some("/root/not-a-child"))],
      ))),
    }));

    let page = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: key_for_header("parent"),
        trajectory_key: encode_trajectory_key(0),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    let activity = page.events[0]
      .agent_activity
      .as_ref()
      .expect("nested agent activity card should remain visible");
    assert_eq!(activity.target_session_id.as_deref(), Some("target"));
    assert_eq!(activity.target_agent_path.as_deref(), Some("/root/not-a-child"));
    assert!(activity.target.is_none());
  }

  #[test]
  fn event_page_keeps_agent_activity_unlinked_for_a_noncanonical_parent_source() {
    let mut stale_parent = session_header("parent", None, "/projects/Alpha", "2026-08-31T00:00:00Z");
    stale_parent.path = PathBuf::from("/fixtures/parent-stale.jsonl");
    let current_parent = session_header("parent", None, "/projects/Alpha", "2026-08-31T00:01:00Z");
    let child = session_header("child", Some("parent"), "/projects/Alpha", "2026-08-31T00:02:00Z");
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::from([(ViewerProvider::Codex, Ok(vec![stale_parent, current_parent, child]))]),
      loaded: Mutex::new(Some(loaded_session_for(
        "parent",
        vec![agent_activity("child", Some("/root/researcher"))],
      ))),
    }));

    let stale_parent_key = encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "parent".to_string(),
      source_path: PathBuf::from("/fixtures/parent-stale.jsonl"),
    })
    .unwrap();
    let page = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: stale_parent_key,
        trajectory_key: encode_trajectory_key(0),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    let activity = page.events[0]
      .agent_activity
      .as_ref()
      .expect("nested agent activity card should remain visible");
    assert!(activity.target.is_none());
  }

  #[test]
  fn event_page_keeps_agent_activity_unlinked_when_header_lookup_fails() {
    let service = ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::from([(ViewerProvider::Codex, Err("session catalog unavailable".to_string()))]),
      loaded: Mutex::new(Some(loaded_session_for(
        "parent",
        vec![agent_activity("child", Some("/root/researcher"))],
      ))),
    }));

    let page = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: key_for_header("parent"),
        trajectory_key: encode_trajectory_key(0),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    let activity = page.events[0]
      .agent_activity
      .as_ref()
      .expect("nested agent activity card should remain visible");
    assert_eq!(activity.target_session_id.as_deref(), Some("child"));
    assert!(activity.target.is_none());
  }

  #[test]
  fn listing_orders_providers_by_explicit_update_time_not_creation_time() {
    let service = service_with_indexed_headers(
      Arc::new(FakeRepository {
        listings: HashMap::new(),
        loaded: Mutex::new(None),
      }),
      vec![
        (
          ViewerProvider::Codex,
          vec![session_header_with_updated(
            "created-first",
            "/projects/one",
            "2026-08-31T10:00:00Z",
            "100",
            100,
          )],
        ),
        (
          ViewerProvider::Pi,
          vec![session_header_with_updated(
            "updated-first",
            "/projects/two",
            "2026-08-30T10:00:00Z",
            "200",
            200,
          )],
        ),
      ],
    );

    let response = service
      .list_sessions(ListSessionsRequest {
        query: SessionQuery::default(),
        cursor: None,
        offset: None,
        limit: None,
      })
      .unwrap();

    assert_eq!(response.sessions[0].session_id, "updated-first");
    assert_eq!(response.sessions[0].updated_at_ms, Some(200));
    assert_eq!(response.sessions[1].session_id, "created-first");
  }

  #[test]
  fn backward_event_pages_return_chronological_slices() {
    let service = service_with_session(visible_session(5));
    let page = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Backward,
        limit: Some(2),
      })
      .unwrap();

    assert_eq!(decode_event_key(&page.events[0].event_key).unwrap(), 3);
    assert_eq!(decode_event_key(&page.events[1].event_key).unwrap(), 4);
    assert!(page.next_cursor.is_none());
    assert_eq!(
      decode_event_cursor(page.previous_cursor.as_deref().unwrap()).unwrap(),
      3
    );
  }

  #[test]
  fn substantive_events_are_exposed_as_a_synthetic_trajectory() {
    let tool = AgentEvent::ToolCall(ToolCallEvent {
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      turn_id: None,
      message_id: None,
      parent_id: None,
      record_kind: ToolRecordKind::Snapshot,
      tool_call_id: Some("call-1".to_string()),
      provider_tool_name: Some("shell".to_string()),
      tool_name: Some("shell".to_string()),
      tool_kind: ToolKind::Shell,
      transport: Some(ToolTransport::Native),
      summary: None,
      phase: Phase::Finished,
      input: None,
      output: None,
      is_error: Some(false),
      native: None,
      timestamp: None,
    });
    let page = service_with_session(loaded_session(vec![tool]))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(page.events[0].event_type, "trajectory");
    assert!(page.events[0].event_key.starts_with("trajectory.v1."));
    assert_eq!(page.events[0].trajectory.as_ref().unwrap().tool_count, 1);
  }

  #[test]
  fn tool_cards_project_every_known_summary_and_keep_an_unknown_fallback() {
    let events = vec![
      tool_call(
        Provider::Codex,
        "shell",
        "shell-1",
        ToolKind::Shell,
        Some(ToolSummary::Shell {
          command: Some("cargo test".to_string()),
          cwd: Some("/work".to_string()),
          exit_code: Some(0),
        }),
        Phase::Finished,
        None,
      ),
      tool_call(
        Provider::Codex,
        "read_file",
        "read-1",
        ToolKind::FileRead,
        Some(ToolSummary::FileRead {
          path: Some("src/lib.rs".to_string()),
        }),
        Phase::Finished,
        None,
      ),
      tool_call(
        Provider::Codex,
        "write_file",
        "write-1",
        ToolKind::FileWrite,
        Some(ToolSummary::FileWrite {
          path: Some("out.txt".to_string()),
          bytes: Some(42),
        }),
        Phase::Finished,
        None,
      ),
      tool_call(
        Provider::Codex,
        "apply_patch",
        "edit-1",
        ToolKind::FileEdit,
        Some(ToolSummary::FileEdit {
          path: Some("src/main.rs".to_string()),
          added: Some(4),
          removed: Some(2),
        }),
        Phase::Finished,
        None,
      ),
      tool_call(
        Provider::Codex,
        "search",
        "search-1",
        ToolKind::Search,
        Some(ToolSummary::Search {
          query: Some("ToolCallEvent".to_string()),
        }),
        Phase::Finished,
        None,
      ),
      tool_call(
        Provider::Codex,
        "fetch",
        "web-1",
        ToolKind::Web,
        Some(ToolSummary::Web {
          url: Some("https://example.test".to_string()),
        }),
        Phase::Finished,
        None,
      ),
      tool_call(
        Provider::Codex,
        "task",
        "task-1",
        ToolKind::Task,
        Some(ToolSummary::Task {
          title: Some("Run checks".to_string()),
        }),
        Phase::Finished,
        None,
      ),
      tool_call(
        Provider::Codex,
        "future_tool",
        "unknown-1",
        ToolKind::Unknown,
        None,
        Phase::Finished,
        None,
      ),
    ];

    let summaries: Vec<EventSummary> = events
      .iter()
      .enumerate()
      .map(|(index, event)| event_summary(&events, index, event))
      .collect();

    let shell = summaries[0].tool.as_ref().unwrap();
    assert_eq!(shell.kind, "shell");
    assert_eq!(shell.command.as_deref(), Some("cargo test"));
    assert_eq!(shell.cwd.as_deref(), Some("/work"));
    assert_eq!(shell.exit_code, Some(0));
    assert_eq!(summaries[1].tool.as_ref().unwrap().path.as_deref(), Some("src/lib.rs"));
    assert_eq!(summaries[2].tool.as_ref().unwrap().bytes, Some(42));
    assert_eq!(summaries[3].tool.as_ref().unwrap().added, Some(4));
    assert_eq!(summaries[3].tool.as_ref().unwrap().removed, Some(2));
    assert_eq!(
      summaries[4].tool.as_ref().unwrap().query.as_deref(),
      Some("ToolCallEvent")
    );
    assert_eq!(
      summaries[5].tool.as_ref().unwrap().url.as_deref(),
      Some("https://example.test")
    );
    assert_eq!(
      summaries[6].tool.as_ref().unwrap().task_title.as_deref(),
      Some("Run checks")
    );
    let unknown = summaries[7].tool.as_ref().unwrap();
    assert_eq!(unknown.kind, "unknown");
    assert_eq!(unknown.tool_name.as_deref(), Some("future_tool"));
    assert_eq!(unknown.tool_call_id.as_deref(), Some("unknown-1"));
  }

  #[test]
  fn tool_operation_cards_are_bounded_and_replace_lifecycle_fragments() {
    let long_command = "c".repeat(MAX_TOOL_CARD_STRING_CHARS + 100);
    let events = vec![
      tool_call(
        Provider::Codex,
        "exec_command",
        "call-1",
        ToolKind::Shell,
        Some(ToolSummary::Shell {
          command: Some(long_command),
          cwd: Some("/work".to_string()),
          exit_code: None,
        }),
        Phase::Started,
        None,
      ),
      AgentEvent::Message(MessageEvent {
        provenance: None,
        provider: Provider::Codex,
        session_id: Some("fixture".to_string()),
        message_id: Some("between".to_string()),
        parent_id: None,
        role: Role::Assistant,
        delivery: MessageDelivery::Commentary,
        phase: Phase::Finished,
        text: "working".to_string(),
        timestamp: None,
      }),
      tool_call(
        Provider::Codex,
        "exec_command",
        "call-1",
        ToolKind::Shell,
        Some(ToolSummary::Shell {
          command: None,
          cwd: None,
          exit_code: Some(7),
        }),
        Phase::Finished,
        Some(json!({"stdout": "failed"})),
      ),
    ];

    let trajectory = trajectory_for_start(&events, 0).expect("terminal tool operation is a trajectory");
    let operation_entry = trajectory
      .entries
      .iter()
      .find(|entry| matches!(entry, TimelineEntry::ToolOperation { .. }))
      .expect("assembled tool operation should stay inside the trajectory");
    let operation =
      timeline_entry_event_summary(operation_entry, &events, &ActivityTargets::default(), &HashSet::new());
    let operation_tool = operation.tool.as_ref().unwrap();

    let page = service_with_session(loaded_session(events))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    assert_eq!(page.total_events, 1);
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].event_type, "trajectory");

    assert_eq!(
      operation_tool.command.as_ref().unwrap().chars().count(),
      MAX_TOOL_CARD_STRING_CHARS
    );
    assert!(operation_tool.command.as_ref().unwrap().ends_with('\u{2026}'));
    assert_eq!(operation_tool.exit_code, Some(7));
    assert_eq!(operation_tool.status, "failed");
    assert_eq!(operation.is_error, Some(true));
    assert_eq!(operation_tool.cwd.as_deref(), Some("/work"));
    assert_eq!(decode_event_key(&operation.event_key).unwrap(), 0);
  }

  #[test]
  fn tool_operation_detail_exposes_final_output_without_a_private_viewer_join() {
    let mut invocation = tool_call(
      Provider::Codex,
      "exec_command",
      "call-1",
      ToolKind::Shell,
      None,
      Phase::Finished,
      None,
    );
    let AgentEvent::ToolCall(invocation_event) = &mut invocation else {
      unreachable!();
    };
    invocation_event.record_kind = ToolRecordKind::Invocation;
    invocation_event.input = Some(json!({"cmd": "cargo test"}));
    let events = vec![
      invocation,
      tool_call(
        Provider::Codex,
        "exec_command",
        "call-1",
        ToolKind::Shell,
        None,
        Phase::Delta,
        Some(Value::String("partial".to_string())),
      ),
      tool_call(
        Provider::Codex,
        "exec_command",
        "call-1",
        ToolKind::Shell,
        None,
        Phase::Finished,
        Some(json!({"output": {"content": [{"type": "text", "text": "final"}]}})),
      ),
    ];
    let detail = service_with_session(loaded_session(events))
      .load_event_detail(LoadEventDetailRequest {
        session_key: key_for("fixture"),
        event_key: encode_event_key(0),
      })
      .unwrap();
    let output = detail.tool_output.unwrap();

    assert_eq!(output.source_event_key, encode_event_key(0));
    assert!(!output.truncated);
    assert_eq!(output.sections.len(), 1);
    assert_eq!(output.sections[0].text, "final");
    assert_eq!(output.sections[0].format, "text");
    assert_eq!(detail.event["source_event_indices"], json!([0, 1, 2]));
  }

  #[test]
  fn terminal_operation_keeps_semantic_output_and_both_code_mode_records() {
    let mut invocation = tool_call(
      Provider::Codex,
      "write_stdin",
      "call-write",
      ToolKind::Terminal,
      Some(ToolSummary::Terminal {
        session_id: Some("90855".to_string()),
        action: Some(TerminalAction::Wait),
        chars_len: Some(0),
        wait_ms: Some(30_000),
      }),
      Phase::Started,
      None,
    );
    let AgentEvent::ToolCall(invocation_call) = &mut invocation else {
      unreachable!();
    };
    invocation_call.provider_tool_name = Some("exec".to_string());
    invocation_call.transport = Some(ToolTransport::CodeExecution);
    invocation_call.input = Some(json!({
      "session_id": 90855,
      "chars": "",
      "yield_time_ms": 30_000,
    }));
    invocation_call.native = Some(json!({
      "type": "custom_tool_call",
      "input": "const r = await tools.write_stdin(...);",
    }));

    let mut result = tool_call(
      Provider::Codex,
      "write_stdin",
      "call-write",
      ToolKind::Terminal,
      None,
      Phase::Finished,
      Some(json!({
        "session_id": 90855,
        "wall_time_seconds": 30.001,
        "text": "Refreshing checks status",
      })),
    );
    let AgentEvent::ToolCall(result_call) = &mut result else {
      unreachable!();
    };
    result_call.provider_tool_name = Some("exec".to_string());
    result_call.transport = Some(ToolTransport::CodeExecution);
    result_call.native = Some(json!({
      "type": "custom_tool_call_output",
      "output": ["Script completed", "{...}"],
    }));

    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("code-mode.jsonl");
    std::fs::write(
      &source_path,
      format!("{}\n", json!({"type": "session_meta", "payload": {"id": "fixture"}})),
    )
    .unwrap();
    let session_key = encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "fixture".to_string(),
      source_path,
    })
    .unwrap();
    let service = service_with_session(loaded_session(vec![invocation, result]));
    let page = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(page.total_events, 1);
    assert_eq!(page.events[0].event_type, "trajectory");
    let trajectory_page = service
      .load_trajectory_event_page(LoadTrajectoryEventPageRequest {
        session_key: session_key.clone(),
        trajectory_key: page.events[0].event_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    assert_eq!(trajectory_page.total_events, 1);
    let operation = &trajectory_page.events[0];
    let card = operation.tool.as_ref().unwrap();
    assert_eq!(card.kind, "terminal");
    assert_eq!(card.status, "completed");
    assert_eq!(card.provider_tool_name.as_deref(), Some("exec"));
    assert_eq!(card.terminal_session_id.as_deref(), Some("90855"));
    assert_eq!(card.terminal_action.as_deref(), Some("wait"));
    assert_eq!(card.wait_ms, Some(30_000));

    let detail = service
      .load_event_detail(LoadEventDetailRequest {
        session_key,
        event_key: operation.event_key.clone(),
      })
      .unwrap();
    assert_eq!(detail.event["output"]["text"], "Refreshing checks status");
    assert!(detail.event.get("native").is_none());
    assert_eq!(
      detail.tool_output.as_ref().unwrap().sections[0].text,
      "Refreshing checks status"
    );
    assert_eq!(
      detail.native.as_ref().unwrap()["source_records"]
        .as_array()
        .unwrap()
        .len(),
      2
    );
    assert_eq!(
      detail.native.as_ref().unwrap()["source_records"][0]["native"]["type"],
      "custom_tool_call"
    );
    assert_eq!(
      detail.native.as_ref().unwrap()["source_records"][1]["native"]["type"],
      "custom_tool_call_output"
    );
  }

  #[test]
  fn tool_operation_assembly_keeps_missing_and_ambiguous_ids_separate() {
    let missing_ids = vec![
      tool_call(
        Provider::Codex,
        "exec_command",
        "",
        ToolKind::Shell,
        None,
        Phase::Started,
        None,
      ),
      tool_call(
        Provider::Codex,
        "exec_command",
        "",
        ToolKind::Shell,
        None,
        Phase::Finished,
        Some(Value::String("wrong".to_string())),
      ),
    ];
    assert_eq!(tokn_session_core::assemble_tool_operations(&missing_ids).len(), 2);

    let mut first = with_tool_input(
      tool_call(
        Provider::Codex,
        "exec_command",
        "reused",
        ToolKind::Shell,
        None,
        Phase::Started,
        None,
      ),
      json!({"cmd": "first"}),
    );
    let mut second = with_tool_input(
      tool_call(
        Provider::Codex,
        "exec_command",
        "reused",
        ToolKind::Shell,
        None,
        Phase::Started,
        None,
      ),
      json!({"cmd": "second"}),
    );
    let AgentEvent::ToolCall(first_tool) = &mut first else {
      unreachable!();
    };
    first_tool.record_kind = ToolRecordKind::Invocation;
    let AgentEvent::ToolCall(second_tool) = &mut second else {
      unreachable!();
    };
    second_tool.record_kind = ToolRecordKind::Invocation;
    let events = vec![
      first,
      second,
      tool_call(
        Provider::Codex,
        "exec_command",
        "reused",
        ToolKind::Shell,
        None,
        Phase::Finished,
        Some(Value::String("ambiguous result".to_string())),
      ),
    ];
    let entries = base_timeline_entries(&events);
    assert_eq!(entries.len(), 3);
    assert!(matches!(entries[0], TimelineEntry::ToolOperation { .. }));
    assert!(matches!(entries[1], TimelineEntry::ToolOperation { .. }));
    assert!(matches!(entries[2], TimelineEntry::ToolOperation { .. }));
  }

  #[test]
  fn output_projection_handles_shell_opencode_dsh_and_json_fallbacks() {
    let shell = project_output(
      &json!({
        "formatted_output": "formatted",
        "aggregated_output": "aggregated",
        "stdout": "stdout",
        "stderr": "stderr"
      }),
      0,
    );
    assert_eq!(shell.len(), 2);
    assert_eq!(shell[0].label.as_deref(), Some("Output"));
    assert_eq!(shell[0].text, "formatted");
    assert_eq!(shell[1].label.as_deref(), Some("Stderr"));
    assert_eq!(shell[1].text, "stderr");

    let opencode = project_output(&json!({"output": {"result": "nested"}, "metadata": {"exit": 0}}), 0);
    assert_eq!(opencode[0].label.as_deref(), Some("Result"));
    assert_eq!(opencode[0].text, "nested");
    assert_eq!(opencode[0].format, "text");

    let dsh = project_output(
      &json!({
        "content": [{
          "type": "tool-result",
          "content": [
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
          ]
        }],
        "error": null
      }),
      0,
    );
    assert_eq!(dsh[0].text, "first\nsecond");
    assert_eq!(dsh[0].format, "text");

    let dynamic_error = project_output(
      &json!({
        "content_items": [{"type": "text", "text": "partial result"}],
        "success": false,
        "error": "tool failed"
      }),
      0,
    );
    assert_eq!(dynamic_error.len(), 2);
    assert_eq!(dynamic_error[0].text, "partial result");
    assert_eq!(dynamic_error[1].label.as_deref(), Some("Error"));
    assert_eq!(dynamic_error[1].text, "tool failed");

    let fallback = project_output(&json!({"future": {"value": 1}}), 0);
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].format, "json");
    assert!(fallback[0].text.contains("\"future\""));
  }

  #[test]
  fn tool_output_preview_is_utf8_safe_and_keeps_both_ends() {
    let text = format!("HEAD{}TAIL", "\u{1f642}".repeat(MAX_TOOL_OUTPUT_BYTES / 2));
    let event = tool_call(
      Provider::Codex,
      "exec_command",
      "call-1",
      ToolKind::Shell,
      None,
      Phase::Finished,
      Some(Value::String(text.clone())),
    );
    let output = tool_output_preview(std::slice::from_ref(&event), 0).unwrap();

    assert!(output.truncated);
    assert_eq!(output.original_size_bytes, text.len());
    assert!(output.sections[0].text.len() <= MAX_TOOL_OUTPUT_BYTES);
    assert!(output.sections[0].text.starts_with("HEAD"));
    assert!(output.sections[0].text.ends_with("TAIL"));
    assert!(output.sections[0].text.contains("output truncated"));
  }

  #[test]
  fn message_summaries_preserve_markdown_within_the_timeline_cap() {
    let markdown = "# Result\n\n```rust\nfn main() {}\n```\n";
    let page = service_with_session(loaded_session(vec![message_event(markdown)]))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert_eq!(page.events[0].summary, markdown);
    assert!(!page.events[0].summary_truncated);
  }

  #[test]
  fn reasoning_summaries_keep_only_the_safe_card_preview() {
    let markdown = "## Approach\n\n- inspect the source\n- verify the result\n";
    let event = AgentEvent::Reasoning(ReasoningEvent {
      provenance: None,
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      message_id: Some("reasoning".to_string()),
      parent_id: None,
      phase: Phase::Finished,
      text: Some("Longer private reasoning".to_string()),
      summary: Some(markdown.to_string()),
      redacted: None,
      encrypted_content: None,
      signature: None,
      timestamp: None,
    });
    let summary = event_summary(std::slice::from_ref(&event), 0, &event);

    assert_eq!(summary.summary, "## Approach - inspect the source - verify the result");
    assert!(!summary.summary_truncated);
    assert_eq!(
      summary.reasoning.as_ref().and_then(|card| card.preview.as_deref()),
      Some(summary.summary.as_str())
    );
    assert!(
      !serde_json::to_string(&summary)
        .unwrap()
        .contains("Longer private reasoning")
    );
  }

  #[test]
  fn usage_cards_preserve_scopes_optional_values_and_u64_precision() {
    let mut model_call = usage_event(UsageKind::ModelCall, Provider::Codex);
    let AgentEvent::Usage(model_call_usage) = &mut model_call else {
      panic!("fixture must be a usage event");
    };
    model_call_usage.input_tokens = u64::MAX;
    model_call_usage.output_tokens = 0;
    model_call_usage.total_tokens = None;
    model_call_usage.cache_read_tokens = Some(0);
    model_call_usage.cache_write_tokens = None;
    model_call_usage.reasoning_tokens = Some(u64::MAX);
    model_call_usage.turn_id = Some("turn-1".to_string());
    model_call_usage.step_id = Some("step-1".to_string());

    let mut operation_total = usage_event(UsageKind::OperationTotal, Provider::OpenCode);
    let AgentEvent::Usage(operation_total_usage) = &mut operation_total else {
      panic!("fixture must be a usage event");
    };
    operation_total_usage.input_tokens = 7;
    operation_total_usage.output_tokens = 11;
    operation_total_usage.total_tokens = Some(19);
    operation_total_usage.cache_read_tokens = Some(3);
    operation_total_usage.cache_write_tokens = Some(5);
    operation_total_usage.reasoning_tokens = Some(0);
    operation_total_usage.turn_id = Some("  ".to_string());
    operation_total_usage.step_id = None;

    let mut session_snapshot = usage_event(UsageKind::SessionSnapshot, Provider::Dsh);
    let AgentEvent::Usage(session_snapshot_usage) = &mut session_snapshot else {
      panic!("fixture must be a usage event");
    };
    session_snapshot_usage.input_tokens = 0;
    session_snapshot_usage.output_tokens = 0;
    session_snapshot_usage.total_tokens = Some(0);

    let events = vec![model_call, operation_total, session_snapshot];
    let summaries: Vec<EventSummary> = events
      .iter()
      .enumerate()
      .map(|(index, event)| event_summary(&events, index, event))
      .collect();

    let model_call = summaries[0].usage.as_ref().expect("model-call card");
    assert_eq!(model_call.kind, "model_call");
    assert_eq!(model_call.input_tokens, u64::MAX.to_string());
    assert_eq!(model_call.output_tokens, "0");
    assert_eq!(model_call.total_tokens, None);
    assert_eq!(model_call.cache_read_tokens.as_deref(), Some("0"));
    assert_eq!(model_call.cache_write_tokens, None);
    assert_eq!(model_call.reasoning_tokens, Some(u64::MAX.to_string()));
    assert_eq!(model_call.turn_id.as_deref(), Some("turn-1"));
    assert_eq!(model_call.step_id.as_deref(), Some("step-1"));

    let operation_total = summaries[1].usage.as_ref().expect("operation-total card");
    assert_eq!(operation_total.kind, "operation_total");
    assert_eq!(operation_total.total_tokens.as_deref(), Some("19"));
    assert_eq!(operation_total.cache_read_tokens.as_deref(), Some("3"));
    assert_eq!(operation_total.cache_write_tokens.as_deref(), Some("5"));
    assert_eq!(operation_total.reasoning_tokens.as_deref(), Some("0"));
    assert_eq!(operation_total.turn_id, None);
    assert_eq!(operation_total.step_id, None);

    let session_snapshot = summaries[2].usage.as_ref().expect("session-snapshot card");
    assert_eq!(session_snapshot.kind, "session_snapshot");
    assert_eq!(session_snapshot.input_tokens, "0");
    assert_eq!(session_snapshot.output_tokens, "0");
    assert_eq!(session_snapshot.total_tokens.as_deref(), Some("0"));

    let serialized = serde_json::to_value(&summaries[0]).unwrap();
    assert!(serialized["usage"]["input_tokens"].is_string());
    assert_eq!(serialized["usage"]["input_tokens"], u64::MAX.to_string());
    assert!(serialized["usage"]["total_tokens"].is_null());
    assert_eq!(serialized["usage"]["cache_read_tokens"], "0");
  }

  #[test]
  fn reasoning_cards_classify_safe_content_and_redaction() {
    let events = vec![
      reasoning_event(
        Some("Summary\nwith \u{001b}[31mcolor\u{001b}[0m"),
        Some("Detailed reasoning"),
        Some("encrypted-secret"),
        Some("signature-secret"),
        None,
      ),
      reasoning_event(None, Some("Text-only reasoning"), None, None, None),
      reasoning_event(None, None, Some("ciphertext-secret"), Some("signature-secret"), None),
      reasoning_event(
        Some("redacted-summary-secret"),
        Some("redacted-text-secret"),
        Some("redacted-ciphertext-secret"),
        Some("redacted-signature-secret"),
        Some(true),
      ),
    ];
    let summaries: Vec<EventSummary> = events
      .iter()
      .enumerate()
      .map(|(index, event)| event_summary(&events, index, event))
      .collect();

    let rich = summaries[0].reasoning.as_ref().expect("reasoning card");
    assert_eq!(rich.preview.as_deref(), Some("Summary with color"));
    assert!(rich.has_summary);
    assert!(rich.has_text);
    assert!(rich.has_encrypted_content);
    assert!(!rich.is_redacted);
    let rich_json = serde_json::to_string(&summaries[0]).unwrap();
    assert!(!rich_json.contains("encrypted-secret"));
    assert!(!rich_json.contains("signature-secret"));

    let text_only = summaries[1].reasoning.as_ref().expect("text-only reasoning card");
    assert_eq!(text_only.preview.as_deref(), Some("Text-only reasoning"));
    assert!(!text_only.has_summary);
    assert!(text_only.has_text);
    assert!(!text_only.has_encrypted_content);
    assert!(!text_only.is_redacted);

    let encrypted = summaries[2].reasoning.as_ref().expect("encrypted reasoning card");
    assert_eq!(encrypted.preview, None);
    assert!(!encrypted.has_summary);
    assert!(!encrypted.has_text);
    assert!(encrypted.has_encrypted_content);
    assert!(!encrypted.is_redacted);
    let encrypted_json = serde_json::to_string(&summaries[2]).unwrap();
    assert!(!encrypted_json.contains("ciphertext-secret"));
    assert!(!encrypted_json.contains("signature-secret"));

    let redacted = summaries[3].reasoning.as_ref().expect("redacted reasoning card");
    assert_eq!(redacted.preview, None);
    assert!(redacted.has_summary);
    assert!(redacted.has_text);
    assert!(redacted.has_encrypted_content);
    assert!(redacted.is_redacted);
    let redacted_json = serde_json::to_string(&summaries[3]).unwrap();
    assert!(!redacted_json.contains("redacted-summary-secret"));
    assert!(!redacted_json.contains("redacted-text-secret"));
    assert!(!redacted_json.contains("redacted-ciphertext-secret"));
    assert!(!redacted_json.contains("redacted-signature-secret"));
  }

  #[test]
  fn reasoning_card_previews_are_single_line_and_bounded() {
    let event = reasoning_event(
      Some(&format!(
        "\u{001b}[31mFirst\nsecond\u{202e} {}",
        "x".repeat(MAX_REASONING_CARD_PREVIEW_CHARS)
      )),
      None,
      None,
      None,
      None,
    );
    let summary = event_summary(std::slice::from_ref(&event), 0, &event);
    let preview = summary
      .reasoning
      .as_ref()
      .and_then(|reasoning| reasoning.preview.as_deref())
      .expect("safe preview");

    assert_eq!(preview.chars().count(), MAX_REASONING_CARD_PREVIEW_CHARS);
    assert!(preview.starts_with("First second "));
    assert!(preview.ends_with('\u{2026}'));
    assert!(!preview.contains('\n'));
    assert!(!preview.contains('\u{001b}'));
    assert!(!preview.contains('\u{202e}'));
  }

  #[test]
  fn truncated_message_summaries_keep_full_bounded_inspector_content() {
    let full_text = "m".repeat(MAX_MESSAGE_SUMMARY_CHARS + 1);
    let page = service_with_session(loaded_session(vec![message_event(&full_text)]))
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: key_for("fixture"),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();

    assert!(page.events[0].summary_truncated);
    assert_eq!(page.events[0].summary.chars().count(), MAX_MESSAGE_SUMMARY_CHARS);
    assert!(page.events[0].summary.ends_with('…'));

    let detail = service_with_session(loaded_session(vec![message_event(&full_text)]))
      .load_event_detail(LoadEventDetailRequest {
        session_key: key_for("fixture"),
        event_key: encode_event_key(0),
      })
      .unwrap();
    assert_eq!(detail.event["text"], full_text);
  }

  #[test]
  fn technical_summaries_keep_the_compact_timeline_cap() {
    let event = AgentEvent::Error(ErrorEvent {
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      message: "e".repeat(MAX_TECHNICAL_SUMMARY_CHARS + 1),
      timestamp: None,
    });
    let summary = event_summary(std::slice::from_ref(&event), 0, &event);

    assert!(summary.summary_truncated);
    assert_eq!(summary.summary.chars().count(), MAX_TECHNICAL_SUMMARY_CHARS);
    assert!(summary.summary.ends_with('…'));
  }

  #[test]
  fn page_and_detail_share_one_bounded_snapshot_until_the_source_changes() {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("session.jsonl");
    std::fs::write(
      &source_path,
      format!("{}\n", json!({"type": "session_meta", "payload": {"id": "fixture"}})),
    )
    .unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "fixture".to_string(),
      source_path: source_path.clone(),
    };
    let session_key = encode_session_key(&locator).unwrap();
    let loads = Arc::new(AtomicUsize::new(0));
    let service = ViewerService::new(Arc::new(CountingRepository {
      loads: Arc::clone(&loads),
    }));

    let page = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    service
      .load_event_detail(LoadEventDetailRequest {
        session_key: session_key.clone(),
        event_key: page.events[0].event_key.clone(),
      })
      .unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 1);

    std::fs::write(
      &source_path,
      format!(
        "{}\n",
        json!({"type": "session_meta", "payload": {"id": "fixture", "cwd": "/changed"}})
      ),
    )
    .unwrap();
    service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key,
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    assert_eq!(loads.load(Ordering::SeqCst), 2);
  }

  #[tokio::test]
  async fn automatic_listing_and_snapshots_use_durable_index_even_when_feed_fails() {
    let repository = indexing_repository(Vec::new());
    let service = service_with_indexed_headers(
      repository.clone(),
      vec![(
        ViewerProvider::Codex,
        vec![session_header("indexed", None, "/project", "2026-09-01T00:00:00Z")],
      )],
    );
    service.relay.configure(crate::relay::RelaySettings::default()).unwrap();
    service.relay.configuration_failed("fixture feed unavailable".into());
    assert!(service.relay.covers(ViewerProvider::Codex));
    let inventory = service
      .indexed_session_inventory(ViewerProvider::Codex)
      .unwrap()
      .unwrap();
    assert_eq!(inventory.headers[0].id, "indexed");
    assert!(service.index_error_for(ViewerProvider::Codex).is_none());
    // Stop the scheduled child before yielding; this test needs only its mode.
    service
      .relay
      .configure(crate::relay::RelaySettings {
        mode: crate::relay::RelayMode::Local,
        ..Default::default()
      })
      .unwrap();
    let snapshots =
      crate::service_server::Service::from_index(crate::RelayConfig::new(Vec::new()), service.session_index.clone())
        .unwrap();
    let catalog = crate::service_client::load_catalog_from(&crate::service_client::Connection::Embedded(snapshots))
      .await
      .unwrap();
    assert!(catalog.warnings.is_empty());
    assert_eq!(catalog.entries.len(), 1);
    assert_eq!(catalog.entries[0].header.id, "indexed");
    assert_eq!(repository.header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(repository.load_calls.load(Ordering::SeqCst), 0);
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn shared_index_runtime_has_one_owner_forwards_retry_and_takes_over() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let first_repository = indexing_repository(Vec::new());
    let second_repository = indexing_repository(Vec::new());
    let make_service = |repository: Arc<IndexingRepository>| {
      let mut service = ViewerService::new_with_index(repository, Arc::new(SessionIndex::open(&path).unwrap()));
      service.indexer_lock = Some(crate::indexer::IndexerLock::new(&path).unwrap());
      service
    };
    let first = crate::runtime::ViewerRuntime::start(make_service(first_repository.clone()));
    async fn until(mut ready: impl FnMut() -> bool) {
      tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while !ready() {
          tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
      })
      .await
      .expect("indexer should make progress");
    }
    until(|| {
      first
        .service
        .session_index_progress()
        .catalog
        .pending_providers
        .is_empty()
    })
    .await;
    let second = crate::runtime::ViewerRuntime::start(make_service(second_repository.clone()));
    until(|| second.service.session_index_progress().activity == IndexActivity::WaitingForIndexer).await;
    assert_eq!(second_repository.header_calls.load(Ordering::SeqCst), 0);
    let mut updates = second.events.subscribe();
    let source = SourceKey::new("codex", "fixture-shared");
    first
      .service
      .session_index
      .replace_sources(&[SourceReplacement::new(
        SourceState::new(source, "fixture", 0),
        vec![SessionMetadata::new(
          IndexedSessionKey::new("codex", "fixture-shared", "shared"),
          "/fixture/shared.jsonl",
        )],
      )])
      .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
      loop {
        let update = updates.recv().await.unwrap();
        if update.event == "session-index-changed" {
          assert_eq!(update.payload["catalog_refresh_required"], true);
          break;
        }
      }
    })
    .await
    .expect("shared reader should publish committed index updates");
    assert_eq!(
      second
        .service
        .indexed_session_inventory(ViewerProvider::Codex)
        .unwrap()
        .unwrap()
        .headers[0]
        .id,
      "shared"
    );
    let before = first_repository.header_calls.load(Ordering::SeqCst);
    second.service.request_session_index_retry().unwrap();
    until(|| first_repository.header_calls.load(Ordering::SeqCst) > before).await;
    assert_eq!(second_repository.header_calls.load(Ordering::SeqCst), 0);
    drop(first);
    until(|| second_repository.header_calls.load(Ordering::SeqCst) > 0).await;
    drop(second);
  }

  #[tokio::test]
  async fn relay_covered_provider_never_falls_back_to_the_repository() {
    let loads = Arc::new(AtomicUsize::new(0));
    let service = ViewerService::new(Arc::new(CountingRepository { loads: loads.clone() }));
    service
      .relay
      .configure(crate::relay::RelaySettings {
        endpoint: "tcp://127.0.0.1:1".into(),
        mode: crate::relay::RelayMode::External,
        ..Default::default()
      })
      .unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "fixture".into(),
      source_path: "/missing/session.jsonl".into(),
    };
    assert!(service.load_verified(&locator).is_err());
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    assert!(
      service
        .indexed_session_inventory(ViewerProvider::Codex)
        .unwrap()
        .is_none()
    );
    assert!(
      service
        .pending_body_jobs(&HashSet::new())
        .unwrap()
        .iter()
        .all(|job| job.provider != ViewerProvider::Codex)
    );
    service
      .relay
      .configure(crate::relay::RelaySettings {
        endpoint: "tcp://127.0.0.1:1".into(),
        mode: crate::relay::RelayMode::Local,
        ..Default::default()
      })
      .unwrap();
  }

  #[test]
  fn opencode_compatible_source_revisions_ignore_the_transient_shm_index() {
    let directory = tempfile::tempdir().unwrap();
    for provider in [ViewerProvider::OpenCode, ViewerProvider::ZCode] {
      let database = directory.path().join(format!("{}.db", provider.as_str()));
      let wal = directory.path().join(format!("{}.db-wal", provider.as_str()));
      let shm = directory.path().join(format!("{}.db-shm", provider.as_str()));
      std::fs::write(&database, b"database").unwrap();
      std::fs::write(&wal, b"wal").unwrap();
      std::fs::write(&shm, b"shm").unwrap();
      let locator = SessionLocator {
        version: 1,
        provider,
        session_id: "fixture".to_string(),
        source_path: database,
      };

      let revision = source_revision(&locator);
      std::fs::write(&shm, b"reader-owned shm change").unwrap();
      assert_eq!(source_revision(&locator), revision);

      std::fs::write(&wal, b"durable wal change").unwrap();
      assert_ne!(source_revision(&locator), revision);
    }
  }

  #[test]
  fn hidden_event_pages_and_details_never_expose_content_or_native_data() {
    let service = service_with_session(hidden_message_session());
    let session_key = key_for("fixture");
    let page = service
      .load_event_page(EventPageRequest {
        window_mode: None,
        session_key: session_key.clone(),
        cursor: None,
        offset: None,
        direction: PageDirection::Forward,
        limit: None,
      })
      .unwrap();
    let page_json = serde_json::to_string(&page).unwrap();
    assert!(page.events[0].is_hidden);
    assert!(!page_json.contains("message-secret"));
    assert!(!page_json.contains("native-secret"));

    let detail = service_with_session(hidden_message_session())
      .load_event_detail(LoadEventDetailRequest {
        session_key,
        event_key: page.events[0].event_key.clone(),
      })
      .unwrap();
    let detail_json = serde_json::to_string(&detail).unwrap();
    assert!(detail.is_hidden);
    assert!(detail.native.is_none());
    assert_eq!(detail.event["redacted"], true);
    assert!(!detail_json.contains("message-secret"));
    assert!(!detail_json.contains("native-secret"));
  }

  #[test]
  fn hidden_unknown_pi_records_are_also_redacted() {
    let hidden = AgentEvent::Unknown(UnknownEvent {
      provider: Provider::Pi,
      session_id: Some("fixture".to_string()),
      native_type: Some("custom_message".to_string()),
      native: Some(json!({"type": "custom_message", "display": false, "content": "secret"})),
      timestamp: None,
    });
    let service = service_with_session(loaded_session(vec![hidden]));
    let detail = service
      .load_event_detail(LoadEventDetailRequest {
        session_key: key_for("fixture"),
        event_key: encode_event_key(0),
      })
      .unwrap();

    assert!(detail.is_hidden);
    assert!(detail.native.is_none());
    assert!(!serde_json::to_string(&detail).unwrap().contains("secret"));
  }

  #[test]
  fn redacted_reasoning_details_never_expose_readable_or_native_payloads() {
    let event = AgentEvent::Reasoning(ReasoningEvent {
      provenance: Some(MessageProvenance {
        source: json!({"kind": "fixture"}),
        display: None,
        native: Some(json!({"native-secret": "provider-withheld native"})),
        surface_op: None,
        source_event_seqs: None,
      }),
      provider: Provider::Pi,
      session_id: Some("fixture".to_string()),
      message_id: Some("reasoning".to_string()),
      parent_id: None,
      phase: Phase::Finished,
      text: Some("provider-withheld text".to_string()),
      summary: Some("provider-withheld summary".to_string()),
      redacted: Some(true),
      encrypted_content: Some("provider-withheld encrypted content".to_string()),
      signature: Some("provider-withheld signature".to_string()),
      timestamp: None,
    });
    let detail = service_with_session(loaded_session(vec![event]))
      .load_event_detail(LoadEventDetailRequest {
        session_key: key_for("fixture"),
        event_key: encode_event_key(0),
      })
      .unwrap();

    let serialized = serde_json::to_string(&detail).unwrap();
    assert!(!detail.is_hidden);
    assert_eq!(detail.event["type"], "reasoning");
    assert_eq!(detail.event["redacted"], true);
    assert!(detail.native.is_none());
    for secret in [
      "provider-withheld text",
      "provider-withheld summary",
      "provider-withheld encrypted content",
      "provider-withheld signature",
      "provider-withheld native",
    ] {
      assert!(!serialized.contains(secret));
    }
  }

  #[test]
  fn non_hidden_detail_separates_native_from_normalized_data() {
    let event = AgentEvent::Unknown(UnknownEvent {
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      native_type: Some("future_event".to_string()),
      native: Some(json!({"future": true})),
      timestamp: None,
    });
    let service = service_with_session(loaded_session(vec![event]));
    let detail = service
      .load_event_detail(LoadEventDetailRequest {
        session_key: key_for("fixture"),
        event_key: encode_event_key(0),
      })
      .unwrap();

    assert_eq!(detail.native, Some(json!({"future": true})));
    assert!(detail.event.get("native").is_none());
  }

  #[test]
  fn oversized_detail_representations_are_replaced_with_bounded_json_placeholders() {
    let oversized = "x".repeat(MAX_DETAIL_VALUE_BYTES + 1);
    let event = AgentEvent::Message(MessageEvent {
      provenance: Some(MessageProvenance {
        source: json!({"kind": "fixture"}),
        display: None,
        native: Some(json!({"payload": oversized.clone()})),
        surface_op: None,
        source_event_seqs: None,
      }),
      provider: Provider::Pi,
      session_id: Some("fixture".to_string()),
      message_id: Some("oversized".to_string()),
      parent_id: None,
      role: Role::Assistant,
      delivery: MessageDelivery::Final,
      phase: Phase::Finished,
      text: oversized,
      timestamp: None,
    });
    let detail = service_with_session(loaded_session(vec![event]))
      .load_event_detail(LoadEventDetailRequest {
        session_key: key_for("fixture"),
        event_key: encode_event_key(0),
      })
      .unwrap();
    let native = detail
      .native
      .as_ref()
      .expect("native placeholder should remain available");

    assert_eq!(detail.event["truncated"], true);
    assert_eq!(detail.event["representation"], "normalized_event");
    assert!(detail.event["original_size_bytes"].as_u64().unwrap() > MAX_DETAIL_VALUE_BYTES as u64);
    assert_eq!(detail.event["limit_bytes"], MAX_DETAIL_VALUE_BYTES);
    assert_eq!(native["truncated"], true);
    assert_eq!(native["representation"], "provider_native");
    assert!(native["original_size_bytes"].as_u64().unwrap() > MAX_DETAIL_VALUE_BYTES as u64);
    assert_eq!(native["limit_bytes"], MAX_DETAIL_VALUE_BYTES);
    assert!(serde_json::to_vec(&detail).unwrap().len() < 4 * 1024);
  }

  pub(super) fn service_with_session(session: LoadedSession) -> ViewerService {
    ViewerService::new(Arc::new(FakeRepository {
      listings: HashMap::new(),
      loaded: Mutex::new(Some(session)),
    }))
  }

  fn visible_session(count: usize) -> LoadedSession {
    loaded_session(
      (0..count)
        .map(|index| {
          AgentEvent::Message(MessageEvent {
            provenance: None,
            provider: Provider::Codex,
            session_id: Some("fixture".to_string()),
            message_id: Some(format!("message-{index}")),
            parent_id: None,
            role: Role::Assistant,
            delivery: MessageDelivery::Final,
            phase: Phase::Finished,
            text: format!("message {index}"),
            timestamp: None,
          })
        })
        .collect(),
    )
  }

  fn message_event(text: &str) -> AgentEvent {
    message_event_with_role(text, Role::Assistant, MessageDelivery::Final)
  }

  pub(super) fn message_event_with_role(text: &str, role: Role, delivery: MessageDelivery) -> AgentEvent {
    AgentEvent::Message(MessageEvent {
      provenance: None,
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      message_id: Some("message".to_string()),
      parent_id: None,
      role,
      delivery,
      phase: Phase::Finished,
      text: text.to_string(),
      timestamp: None,
    })
  }

  fn usage_event(kind: UsageKind, provider: Provider) -> AgentEvent {
    AgentEvent::Usage(UsageEvent {
      kind,
      provider,
      session_id: Some("fixture".to_string()),
      turn_id: None,
      step_id: None,
      message_id: None,
      record_id: None,
      input_tokens: 0,
      output_tokens: 0,
      total_tokens: None,
      cache_read_tokens: None,
      cache_write_tokens: None,
      reasoning_tokens: None,
      native: json!({}),
      timestamp: None,
    })
  }

  fn settings_event() -> AgentEvent {
    AgentEvent::SessionSettingsApplied(SessionSettingsApplied {
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      model_provider: None,
      model_id: None,
      service_tier: None,
      cwd: None,
      reasoning_effort: None,
      reasoning_summary: None,
      personality: None,
      collaboration_mode: None,
      approval_policy: None,
      approvals_reviewer: None,
      active_permission_profile_id: None,
      native: None,
      timestamp: None,
    })
  }

  fn metadata_event(summary: &str) -> AgentEvent {
    AgentEvent::Metadata(MetadataEvent {
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      kind: MetadataKind::Diagnostic,
      native_type: "fixture_metadata".to_string(),
      summary: summary.to_string(),
      native: json!({}),
      timestamp: None,
    })
  }

  fn lifecycle_event() -> AgentEvent {
    AgentEvent::Lifecycle(LifecycleEvent {
      provider: Provider::Codex,
      session_id: Some("fixture".to_string()),
      turn_id: "turn-1".to_string(),
      step_id: None,
      scope: LifecycleScope::Turn,
      phase: Phase::Finished,
      outcome: None,
      native: json!({}),
      timestamp: None,
    })
  }

  fn with_timestamp(mut event: AgentEvent, timestamp: &str) -> AgentEvent {
    let timestamp = Some(timestamp.to_string());
    match &mut event {
      AgentEvent::SessionStarted(event) => event.timestamp = timestamp,
      AgentEvent::ProviderChanged(event) => event.timestamp = timestamp,
      AgentEvent::SessionSettingsApplied(event) => event.timestamp = timestamp,
      AgentEvent::Message(event) => event.timestamp = timestamp,
      AgentEvent::QuestionRequest(event) => event.timestamp = timestamp,
      AgentEvent::QuestionReply(event) => event.timestamp = timestamp,
      AgentEvent::Reasoning(event) => event.timestamp = timestamp,
      AgentEvent::GoalUpdated(event) => event.timestamp = timestamp,
      AgentEvent::AgentActivity(event) => event.timestamp = timestamp,
      AgentEvent::ToolCall(event) => event.timestamp = timestamp,
      AgentEvent::Lifecycle(event) => event.timestamp = timestamp,
      AgentEvent::Usage(event) => event.timestamp = timestamp,
      AgentEvent::Metadata(event) => event.timestamp = timestamp,
      AgentEvent::Error(event) => event.timestamp = timestamp,
      AgentEvent::Unknown(event) => event.timestamp = timestamp,
      AgentEvent::Compaction(event) => event.timestamp = timestamp,
    }
    event
  }

  fn reasoning_event(
    summary: Option<&str>,
    text: Option<&str>,
    encrypted_content: Option<&str>,
    signature: Option<&str>,
    redacted: Option<bool>,
  ) -> AgentEvent {
    AgentEvent::Reasoning(ReasoningEvent {
      provenance: None,
      provider: Provider::Pi,
      session_id: Some("fixture".to_string()),
      message_id: Some("reasoning".to_string()),
      parent_id: None,
      phase: Phase::Finished,
      text: text.map(str::to_string),
      summary: summary.map(str::to_string),
      redacted,
      encrypted_content: encrypted_content.map(str::to_string),
      signature: signature.map(str::to_string),
      timestamp: None,
    })
  }

  #[allow(clippy::too_many_arguments)]
  fn tool_call(
    provider: Provider,
    tool_name: &str,
    tool_call_id: &str,
    tool_kind: ToolKind,
    summary: Option<ToolSummary>,
    phase: Phase,
    output: Option<Value>,
  ) -> AgentEvent {
    let is_error = match summary.as_ref() {
      Some(ToolSummary::Shell {
        exit_code: Some(exit_code),
        ..
      }) => Some(*exit_code != 0),
      _ => None,
    };
    AgentEvent::ToolCall(ToolCallEvent {
      provider,
      session_id: Some("fixture".to_string()),
      turn_id: None,
      message_id: None,
      parent_id: None,
      record_kind: match phase {
        Phase::Started => ToolRecordKind::Invocation,
        Phase::Delta | Phase::Updated => ToolRecordKind::Progress,
        Phase::Finished if output.is_some() => ToolRecordKind::Result,
        Phase::Finished => ToolRecordKind::Snapshot,
      },
      tool_call_id: Some(tool_call_id.to_string()),
      provider_tool_name: Some(tool_name.to_string()),
      tool_name: Some(tool_name.to_string()),
      tool_kind,
      transport: Some(ToolTransport::Native),
      summary,
      phase,
      input: None,
      output,
      is_error,
      native: None,
      timestamp: None,
    })
  }

  fn with_tool_input(mut event: AgentEvent, input: Value) -> AgentEvent {
    let AgentEvent::ToolCall(tool) = &mut event else {
      panic!("fixture must be a tool event");
    };
    tool.input = Some(input);
    event
  }

  fn hidden_message_session() -> LoadedSession {
    loaded_session(vec![AgentEvent::Message(MessageEvent {
      provenance: Some(MessageProvenance {
        source: json!({"kind": "extension"}),
        display: Some(false),
        native: Some(json!({"secret": "native-secret"})),
        surface_op: None,
        source_event_seqs: None,
      }),
      provider: Provider::Pi,
      session_id: Some("fixture".to_string()),
      message_id: Some("hidden".to_string()),
      parent_id: None,
      role: Role::System,
      delivery: MessageDelivery::Unspecified,
      phase: Phase::Finished,
      text: "message-secret".to_string(),
      timestamp: None,
    })])
  }

  pub(super) fn loaded_session(events: Vec<AgentEvent>) -> LoadedSession {
    loaded_session_for("fixture", events)
  }

  fn loaded_session_for(id: &str, events: Vec<AgentEvent>) -> LoadedSession {
    LoadedSession {
      reference: session_ref(id, None, "/projects/fixture", "2026-06-01T00:00:00Z"),
      events,
      history_status: SessionHistoryStatus::Complete,
    }
  }

  fn agent_activity(target_session_id: &str, target_agent_path: Option<&str>) -> AgentEvent {
    AgentEvent::AgentActivity(AgentActivity {
      provider: Provider::Codex,
      session_id: Some("parent".to_string()),
      event_id: Some("activity-1".to_string()),
      actor_session_id: None,
      actor_agent_path: None,
      target_session_id: Some(target_session_id.to_string()),
      target_agent_path: target_agent_path.map(str::to_string),
      kind: "started".to_string(),
      communication: None,
      occurred_at_ms: Some(1_788_112_800_000),
      native: None,
      timestamp: Some("2026-08-31T00:03:00Z".to_string()),
    })
  }

  pub(super) fn key_for(session_id: &str) -> String {
    encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: session_id.to_string(),
      source_path: PathBuf::from("/fixtures/session.jsonl"),
    })
    .unwrap()
  }

  fn key_for_cached_source(directory: &tempfile::TempDir, session_id: &str) -> String {
    let source_path = directory.path().join(format!("{session_id}.jsonl"));
    std::fs::write(
      &source_path,
      format!("{}\n", json!({"type": "session_meta", "payload": {"id": session_id}})),
    )
    .unwrap();
    encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: session_id.to_string(),
      source_path,
    })
    .unwrap()
  }

  fn key_for_header(session_id: &str) -> String {
    encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: session_id.to_string(),
      source_path: PathBuf::from(format!("/fixtures/{session_id}.jsonl")),
    })
    .unwrap()
  }

  fn session_ref(id: &str, parent: Option<&str>, cwd: &str, timestamp: &str) -> SessionRef {
    SessionRef {
      id: id.to_string(),
      title: None,
      preview: None,
      parent_session_id: parent.map(str::to_string),
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      path: PathBuf::from(format!("/fixtures/{id}.jsonl")),
      cwd: Some(cwd.to_string()),
      timestamp: Some(timestamp.to_string()),
      message_count: 2,
    }
  }

  fn session_header(id: &str, parent: Option<&str>, cwd: &str, timestamp: &str) -> SessionHeader {
    let updated_at_ms = parse_updated_at_ms(Some(timestamp));
    SessionHeader {
      id: id.to_string(),
      title: None,
      preview: None,
      parent_session_id: parent.map(str::to_string),
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      path: PathBuf::from(format!("/fixtures/{id}.jsonl")),
      cwd: Some(cwd.to_string()),
      timestamp: Some(timestamp.to_string()),
      updated_at: Some(timestamp.to_string()),
      updated_at_ms,
    }
  }

  fn session_header_with_updated(
    id: &str,
    cwd: &str,
    timestamp: &str,
    updated_at: &str,
    updated_at_ms: i64,
  ) -> SessionHeader {
    SessionHeader {
      id: id.to_string(),
      title: None,
      preview: None,
      parent_session_id: None,
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      path: PathBuf::from(format!("/fixtures/{id}.jsonl")),
      cwd: Some(cwd.to_string()),
      timestamp: Some(timestamp.to_string()),
      updated_at: Some(updated_at.to_string()),
      updated_at_ms: Some(updated_at_ms),
    }
  }

  #[test]
  fn encoded_session_key_contains_expected_locator() {
    let key = key_for("fixture");
    let locator = decode_session_key(&key).unwrap();
    assert_eq!(locator.provider, ViewerProvider::Codex);
    assert_eq!(locator.session_id, "fixture");
  }
}
