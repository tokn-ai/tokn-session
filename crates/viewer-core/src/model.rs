use std::path::PathBuf;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokn_session_client::Source;
use tokn_session_core::SessionHistoryStatus;

pub const DEFAULT_PAGE_LIMIT: usize = 50;
pub const MAX_PAGE_LIMIT: usize = 200;
const MAX_SESSION_KEY_BYTES: usize = 64 * 1024;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewerProvider {
  Codex,
  Pi,
  #[serde(rename = "opencode")]
  OpenCode,
  #[serde(rename = "zcode")]
  ZCode,
  #[serde(rename = "workbuddy")]
  WorkBuddy,
  Dsh,
}

impl ViewerProvider {
  pub const ALL: [Self; 6] = [
    Self::Codex,
    Self::Pi,
    Self::OpenCode,
    Self::ZCode,
    Self::WorkBuddy,
    Self::Dsh,
  ];

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Codex => "codex",
      Self::Pi => "pi",
      Self::OpenCode => "opencode",
      Self::ZCode => "zcode",
      Self::WorkBuddy => "workbuddy",
      Self::Dsh => "dsh",
    }
  }

  pub fn source(self) -> Source {
    match self {
      Self::Codex => Source::Codex,
      Self::Pi => Source::Pi,
      Self::OpenCode => Source::OpenCode,
      Self::ZCode => Source::ZCode,
      Self::WorkBuddy => Source::WorkBuddy,
      Self::Dsh => Source::Dsh,
    }
  }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrder {
  #[default]
  Time,
  Project,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SessionQuery {
  #[serde(default)]
  pub order: SessionOrder,
  #[serde(default)]
  pub providers: Vec<ViewerProvider>,
  pub search: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ListSessionsRequest {
  #[serde(default)]
  pub query: SessionQuery,
  pub cursor: Option<String>,
  pub offset: Option<usize>,
  pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ListSessionsResponse {
  pub sessions: Vec<SessionSummary>,
  pub next_cursor: Option<String>,
  /// Providers whose durable header catalog has not yet committed. Their
  /// rows are deliberately absent from this response rather than triggering
  /// a synchronous provider scan on the UI request path.
  pub pending_providers: Vec<ViewerProvider>,
  pub source_errors: Vec<SourceError>,
}

/// A bounded, metadata-only page of direct descendants for one session.
///
/// The sidebar loads these on demand rather than materializing an entire
/// session family in the root listing. That keeps a single unusually broad
/// delegation tree from making the initial IPC response unbounded.
#[derive(Clone, Debug, Deserialize)]
pub struct ListSessionChildrenRequest {
  pub parent_session_key: String,
  pub cursor: Option<String>,
  pub offset: Option<usize>,
  pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ListSessionChildrenResponse {
  pub sessions: Vec<SessionSummary>,
  pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SourceError {
  pub provider: ViewerProvider,
  pub message: String,
}

/// A cheap, in-memory snapshot of the background session-index worker.
///
/// This intentionally contains counts and provider identities only. Detailed
/// error text remains part of the existing index-backed sidebar response, so
/// a status-bar poll or progress event never leaks provider paths or causes a
/// provider read.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionIndexProgress {
  /// A monotonic decimal string. Keeping this as text avoids JavaScript's
  /// integer precision boundary and lets the frontend discard stale events.
  pub revision: String,
  pub is_refreshing: bool,
  pub activity: IndexActivity,
  pub catalog: CatalogIndexProgress,
  pub body: BodyIndexProgress,
  /// A scheduler-level failure category. Provider-specific readable failures
  /// remain in the existing sidebar source errors instead of this live status
  /// payload.
  pub worker_error: Option<IndexWorkerError>,
  /// Epoch milliseconds for a scheduler-selected retry, when known. `None`
  /// also represents an immediately queued manual retry.
  pub retry_at_ms: Option<i64>,
}

impl SessionIndexProgress {
  pub(crate) fn initial(body_batch_size: usize) -> Self {
    Self {
      revision: "0".to_owned(),
      is_refreshing: false,
      activity: IndexActivity::Idle,
      catalog: CatalogIndexProgress {
        // Startup always establishes a complete durable catalog before the
        // scheduler can use targeted change checks.
        scope: CatalogRefreshScope::Full,
        active_provider: None,
        processed_providers: 0,
        total_providers: ViewerProvider::ALL.len(),
        // Before the first durable catalog pass, every provider is pending.
        pending_providers: ViewerProvider::ALL.to_vec(),
        error_providers: Vec::new(),
      },
      body: BodyIndexProgress {
        active_provider: None,
        pending_jobs: 0,
        failed_jobs: 0,
        completed_in_run: 0,
        stale_in_run: 0,
        batch_size: body_batch_size,
        providers: ViewerProvider::ALL
          .into_iter()
          .map(|provider| ProviderBody {
            provider,
            total_jobs: 0,
            completed_jobs: 0,
            pending_jobs: 0,
            failed_jobs: 0,
          })
          .collect(),
      },
      worker_error: None,
      retry_at_ms: None,
    }
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexActivity {
  Idle,
  Catalog,
  Body,
  WaitingToRetry,
  WaitingForIndexer,
}

/// A sanitized failure from the scheduler task itself rather than a single
/// provider catalog/body job. It intentionally carries no platform error text
/// or path information across the Tauri boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexWorkerError {
  RefreshFailed,
  TaskFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CatalogIndexProgress {
  /// Whether this is a complete provider discovery pass or a targeted
  /// follow-up for known changed session files. The UI uses this to describe
  /// a normal lightweight refresh without implying a full rescan.
  pub scope: CatalogRefreshScope,
  pub active_provider: Option<ViewerProvider>,
  pub processed_providers: usize,
  pub total_providers: usize,
  /// Providers without a committed durable catalog sentinel.
  pub pending_providers: Vec<ViewerProvider>,
  /// Providers whose most recent catalog attempt failed. Detail text stays in
  /// `ListSessionsResponse.source_errors`.
  pub error_providers: Vec<ViewerProvider>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogRefreshScope {
  Full,
  Targeted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BodyIndexProgress {
  /// At most one provider is actively loading a body job because the index
  /// scheduler has a single bounded global queue. Providers with pending jobs
  /// other than this one are queued, not concurrently loading.
  pub active_provider: Option<ViewerProvider>,
  /// Pending work after the latest observed queue reconciliation. Failed jobs
  /// remain pending and are counted again in `failed_jobs`.
  pub pending_jobs: usize,
  pub failed_jobs: usize,
  pub completed_in_run: usize,
  pub stale_in_run: usize,
  pub batch_size: usize,
  pub providers: Vec<ProviderBody>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProviderBody {
  pub provider: ViewerProvider,
  /// Work in the current catalog baseline. A catalog change that stages body
  /// work establishes a new baseline; body-only passes retain it and can grow
  /// it if new staged work appears.
  pub total_jobs: usize,
  /// Jobs durably handled since the current catalog baseline. This is derived
  /// from the staged source cursor, so another process's completion survives a
  /// restart and an exhausted durable queue reaches `total_jobs / total_jobs`.
  pub completed_jobs: usize,
  /// Remaining queued or retryable work. A nonzero count does not imply this
  /// provider is active; compare `BodyIndexProgress.active_provider`.
  pub pending_jobs: usize,
  /// Failed jobs are still pending and are therefore a subset of
  /// `pending_jobs`.
  pub failed_jobs: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
  pub session_key: String,
  pub session_id: String,
  pub provider: ViewerProvider,
  pub title: Option<String>,
  pub preview: Option<String>,
  pub project: Option<String>,
  pub project_order_ms: Option<i64>,
  pub project_key: Option<String>,
  pub cwd: Option<String>,
  pub updated_at_ms: Option<i64>,
  pub timestamp: Option<String>,
  pub parent_session_id: Option<String>,
  /// True only when the parent relation resolves to a canonical header in the
  /// same provider. Orphaned and cycle-broken records retain their raw parent
  /// ID but remain visible as roots.
  pub is_subagent: bool,
  pub agent_path: Option<String>,
  pub agent_nickname: Option<String>,
  pub agent_role: Option<String>,
  /// Number of direct, canonical descendants known from metadata-only
  /// discovery. It is intentionally not a runtime state or event count.
  pub child_count: usize,
  /// Unknown for metadata-only listings. Loading an event page returns the
  /// authoritative normalized event count for the selected session.
  pub message_count: Option<usize>,
  pub event_count: Option<usize>,
  pub history_status: Option<HistoryStatus>,
  /// True when this session has a newly indexed, visible final assistant message that has not yet been acknowledged by opening
  /// its event page.
  pub has_unread: bool,
  /// Compatibility field, always false. Unread attention belongs only to this session.
  pub has_unread_descendant: bool,
  pub unread_final_count: u64,
  /// Compatibility field, always zero. Subagent replies never contribute.
  pub unread_descendant_count: u64,
  pub is_running: bool,
  pub has_running_descendant: bool,
  pub question_attention: QuestionAttention,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct QuestionAttention {
  pub required_count: u64,
  pub available_count: u64,
}

#[derive(Debug, Serialize)]
pub struct OutstandingQuestion {
  pub event_key: String,
  pub requires_input: bool,
  pub unanswered_count: usize,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStatus {
  Complete,
  FilteredSubagent,
  SubagentBodyUnavailable,
}

impl From<SessionHistoryStatus> for HistoryStatus {
  fn from(value: SessionHistoryStatus) -> Self {
    match value {
      SessionHistoryStatus::Complete => Self::Complete,
      SessionHistoryStatus::FilteredSubagent => Self::FilteredSubagent,
      SessionHistoryStatus::SubagentBodyUnavailable => Self::SubagentBodyUnavailable,
    }
  }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageDirection {
  #[default]
  Forward,
  Backward,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EventPageRequest {
  pub session_key: String,
  /// Retained windows are paged by user turns in the source cache. Omission
  /// preserves the older row-based API for non-viewer consumers.
  #[serde(default)]
  pub window_mode: Option<HistoryWindowMode>,
  pub cursor: Option<String>,
  pub offset: Option<usize>,
  #[serde(default)]
  pub direction: PageDirection,
  pub limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryWindowMode {
  Retained,
  Earlier,
}

/// One desktop window/browser tab's short-lived cache lease. Candidates are
/// already scoped by the sidebar's project and filters; they are not reads.
#[derive(Clone, Debug, Deserialize)]
pub struct SessionViewRequest {
  pub view_id: String,
  pub revision: u64,
  pub session_key: Option<String>,
  #[serde(default)]
  pub candidate_session_keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionInputStatusRequest {
  pub session_key: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionInputStatus {
  pub available: bool,
  pub message: String,
  pub max_length: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SubmitSessionInputRequest {
  pub session_key: String,
  pub request_id: String,
  pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeliveryStatus {
  Accepted,
  NotSent,
  Unknown,
  Pending,
}

#[derive(Clone, Debug, Serialize)]
pub struct SubmitSessionInputResponse {
  pub request_id: String,
  pub status: InputDeliveryStatus,
  pub message: String,
}

#[derive(Debug, Serialize)]
pub struct EventPage {
  pub events: Vec<EventSummary>,
  pub next_cursor: Option<String>,
  pub previous_cursor: Option<String>,
  pub total_events: usize,
  pub history_status: HistoryStatus,
  /// Current follow failure, if the page is showing a last-good snapshot.
  pub follow_error: Option<String>,
  /// Opaque snapshot of the session's indexed attention state. The frontend
  /// acknowledges this only after React has committed the page, preventing a
  /// stale request from consuming a newer update.
  pub attention_revision: Option<String>,
  pub outstanding_questions: Vec<OutstandingQuestion>,
}

/// Acknowledges the exact attention revision represented by an accepted event
/// page. Revisions cross IPC as decimal strings so they never lose precision
/// in JavaScript.
#[derive(Clone, Debug, Deserialize)]
pub struct AcknowledgeSessionAttentionRequest {
  pub session_key: String,
  pub attention_revision: String,
}

#[derive(Debug, Serialize)]
pub struct AcknowledgeSessionAttentionResponse {
  pub changed: bool,
}

#[derive(Debug, Serialize)]
pub struct EventSummary {
  /// Ordered semantic children of a work group; independent of Inspector limits.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub child_keys: Option<Vec<String>>,
  /// Positional presentation slot across generation replacement. Never use
  /// this as a detail/cache identity; event_key owns that generation.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub slot_key: Option<String>,
  pub compaction: Option<CompactionCardSummary>,
  pub event_key: String,
  #[serde(rename = "type")]
  pub event_type: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub delivery: Option<String>,
  pub provider: ViewerProvider,
  pub timestamp: Option<String>,
  pub phase: Option<String>,
  pub role: Option<String>,
  pub title: String,
  pub summary: String,
  pub summary_truncated: bool,
  pub is_hidden: bool,
  /// A routine lifecycle/configuration row or intermediate usage record that
  /// can be hidden without loading detail. Final accounting, content, unknown
  /// diagnostics, and exceptional lifecycle outcomes remain visible.
  pub is_bookkeeping: bool,
  pub is_error: Option<bool>,
  pub tool: Option<ToolCardSummary>,
  pub usage: Option<UsageCardSummary>,
  pub reasoning: Option<ReasoningCardSummary>,
  /// Aggregate metadata for a synthetic, contiguous work trajectory. The
  /// individual normalized entries remain available through the trajectory
  /// page and retain their own `event.v1.*` detail keys.
  pub trajectory: Option<TrajectoryCardSummary>,
  /// Safe historical activity metadata. A target is present only when the
  /// activity's provider-native target ID resolves to a canonical direct child
  /// of the session being viewed.
  pub agent_activity: Option<AgentActivityCardSummary>,
}

#[derive(Debug, Serialize)]
pub struct CompactionCardSummary {
  pub state: String,
  pub trigger: Option<String>,
  pub reason: Option<String>,
  pub has_summary: bool,
  pub summary_opaque: bool,
  pub measurements: Vec<CompactionTokenSummary>,
}

#[derive(Debug, Serialize)]
pub struct CompactionTokenSummary {
  pub scope: String,
  pub tokens: String,
  pub estimated: Option<bool>,
}

/// Bounded, source-neutral presentation metadata for one collapsed run of
/// historical work. Counts describe visible base-timeline entries; one tool
/// operation can therefore represent several provider source records.
///
/// Timestamp strings come only from parseable provider event timestamps in
/// source chronology. `duration_ms` is a decimal string so a renderer never
/// loses precision when a provider reports a large interval.
#[derive(Debug, Serialize)]
pub struct TrajectoryCardSummary {
  /// Displayed work-segment state. Earlier segments can complete while their
  /// turn continues; unknown is not evidence of a running process.
  pub status: &'static str,
  pub event_count: usize,
  pub source_event_count: usize,
  pub reasoning_count: usize,
  pub tool_count: usize,
  pub agent_activity_count: usize,
  pub lifecycle_count: usize,
  pub usage_count: usize,
  pub error_count: usize,
  pub unknown_count: usize,
  pub started_at: Option<String>,
  pub ended_at: Option<String>,
  pub duration_ms: Option<String>,
}

/// Bounded presentation metadata for one historical agent-activity record.
///
/// Target navigation uses a verified direct child. Communication sender
/// navigation uses a unique canonical session in the current task tree.
#[derive(Clone, Debug, Serialize)]
pub struct AgentActivityCardSummary {
  pub kind: String,
  pub event_id: Option<String>,
  pub target_session_id: Option<String>,
  pub target_agent_path: Option<String>,
  pub target: Option<SessionSummary>,
  pub actor_session_id: Option<String>,
  pub actor_agent_path: Option<String>,
  pub actor: Option<SessionSummary>,
  pub communication: Option<AgentCommunicationCardSummary>,
}

/// Readable message bodies stay in lazy event detail, never in timeline pages.
#[derive(Clone, Debug, Serialize)]
pub struct AgentCommunicationCardSummary {
  pub has_text: bool,
  pub has_encrypted_content: bool,
  pub trigger_turn: Option<bool>,
}

/// Source-neutral token accounting for a usage event.
///
/// Token counts intentionally cross the IPC boundary as decimal strings:
/// JavaScript `number` cannot represent every `u64` exactly.
#[derive(Debug, Serialize)]
pub struct UsageCardSummary {
  pub kind: String,
  pub input_tokens: String,
  pub output_tokens: String,
  pub total_tokens: Option<String>,
  pub cache_read_tokens: Option<String>,
  pub cache_write_tokens: Option<String>,
  pub reasoning_tokens: Option<String>,
  pub turn_id: Option<String>,
  pub step_id: Option<String>,
}

/// Safe reasoning metadata for a collapsed event card.
///
/// Raw encrypted reasoning, signatures, and full reasoning text deliberately
/// remain out of this projection. The viewer can use the boolean flags to
/// select an appropriate disclosure state without exposing opaque payloads.
#[derive(Debug, Serialize)]
pub struct ReasoningCardSummary {
  pub preview: Option<String>,
  pub has_summary: bool,
  pub has_text: bool,
  pub has_encrypted_content: bool,
  pub is_redacted: bool,
}

#[derive(Debug, Serialize)]
pub struct ToolCardSummary {
  pub kind: String,
  pub tool_name: Option<String>,
  pub tool_call_id: Option<String>,
  /// Derived operation state, not the source record's transport phase.
  pub status: String,
  pub provider_tool_name: Option<String>,
  pub language: Option<String>,
  pub command: Option<String>,
  pub cwd: Option<String>,
  pub terminal_session_id: Option<String>,
  pub terminal_action: Option<String>,
  pub chars_len: Option<u64>,
  pub wait_ms: Option<u64>,
  pub path: Option<String>,
  pub query: Option<String>,
  pub url: Option<String>,
  pub task_title: Option<String>,
  pub exit_code: Option<i64>,
  pub bytes: Option<u64>,
  pub added: Option<u64>,
  pub removed: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LoadEventDetailRequest {
  pub session_key: String,
  pub event_key: String,
}

/// Loads a bounded page of the existing normalized entries represented by one
/// synthetic trajectory item. The trajectory key is separate from a raw event
/// key, so expanding an item never changes the detail identity of its children.
#[derive(Clone, Debug, Deserialize)]
pub struct LoadTrajectoryEventPageRequest {
  pub session_key: String,
  pub trajectory_key: String,
  pub cursor: Option<String>,
  pub offset: Option<usize>,
  #[serde(default)]
  pub direction: PageDirection,
  pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct TrajectoryEventPage {
  pub events: Vec<EventSummary>,
  pub next_cursor: Option<String>,
  pub previous_cursor: Option<String>,
  pub total_events: usize,
}

#[derive(Debug, Serialize)]
pub struct EventDetail {
  pub event_key: String,
  pub event: Value,
  pub native: Option<Value>,
  pub is_hidden: bool,
  pub tool_output: Option<ToolOutputPreview>,
}

#[derive(Debug, Serialize)]
pub struct ToolOutputPreview {
  pub sections: Vec<ToolOutputSection>,
  pub truncated: bool,
  pub original_size_bytes: usize,
  pub source_event_key: String,
}

#[derive(Debug, Serialize)]
pub struct ToolOutputSection {
  pub label: Option<String>,
  pub text: String,
  pub format: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct SessionLocator {
  pub version: u8,
  pub provider: ViewerProvider,
  pub session_id: String,
  pub source_path: PathBuf,
}

pub(crate) fn encode_session_key(locator: &SessionLocator) -> Result<String, String> {
  let bytes = serde_json::to_vec(locator).map_err(|error| format!("failed to encode session key: {error}"))?;
  if bytes.len() > MAX_SESSION_KEY_BYTES {
    return Err("session source identity is too large".to_string());
  }
  Ok(format!("session.v1.{}", hex_encode(&bytes)))
}

pub(crate) fn decode_session_key(key: &str) -> Result<SessionLocator, String> {
  let encoded = key
    .strip_prefix("session.v1.")
    .ok_or_else(|| "unsupported session key".to_string())?;
  let bytes = hex_decode(encoded)?;
  let locator: SessionLocator =
    serde_json::from_slice(&bytes).map_err(|_| "invalid session key payload".to_string())?;
  if locator.version != 1 || locator.session_id.is_empty() || locator.source_path.as_os_str().is_empty() {
    return Err("invalid session key payload".to_string());
  }
  Ok(locator)
}

pub(crate) fn encode_list_cursor(offset: usize) -> String {
  format!("sessions.v1.{offset:x}")
}

pub(crate) fn decode_list_cursor(cursor: &str) -> Result<usize, String> {
  decode_cursor(cursor, "sessions.v1.")
}

pub(crate) fn encode_event_cursor(offset: usize) -> String {
  format!("events.v1.{offset:x}")
}

pub(crate) fn decode_event_cursor(cursor: &str) -> Result<usize, String> {
  decode_cursor(cursor, "events.v1.")
}

pub(crate) fn encode_event_key(index: usize) -> String {
  format!("event.v1.{index:x}")
}

pub(crate) fn decode_event_key(key: &str) -> Result<usize, String> {
  decode_cursor(key, "event.v1.")
}

/// A synthetic trajectory identity is intentionally distinct from the stable
/// source-event identity. Its numeric payload is the first source position of
/// the collapsed run, which remains stable when more work is appended.
pub(crate) fn encode_trajectory_key(start: usize) -> String {
  format!("trajectory.v1.{start:x}")
}

pub(crate) fn decode_trajectory_key(key: &str) -> Result<usize, String> {
  decode_cursor(key, "trajectory.v1.")
}

pub(crate) fn encode_trajectory_event_cursor(anchor: usize, offset: usize) -> String {
  format!("trajectory-events.v1.{anchor:x}.{offset:x}")
}

pub(crate) fn decode_trajectory_event_cursor(cursor: &str) -> Result<(usize, usize), String> {
  let encoded = cursor
    .strip_prefix("trajectory-events.v1.")
    .filter(|value| !value.is_empty())
    .ok_or_else(|| "invalid trajectory pagination cursor".to_string())?;
  let (anchor, offset) = encoded
    .split_once('.')
    .filter(|(anchor, offset)| !anchor.is_empty() && !offset.is_empty())
    .ok_or_else(|| "invalid trajectory pagination cursor".to_string())?;
  if offset.contains('.') {
    return Err("invalid trajectory pagination cursor".to_string());
  }
  let anchor = usize::from_str_radix(anchor, 16).map_err(|_| "invalid trajectory pagination cursor".to_string())?;
  let offset = usize::from_str_radix(offset, 16).map_err(|_| "invalid trajectory pagination cursor".to_string())?;
  Ok((anchor, offset))
}

pub(crate) fn bounded_limit(limit: Option<usize>) -> Result<usize, String> {
  let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT);
  if limit == 0 {
    return Err("limit must be greater than zero".to_string());
  }
  Ok(limit.min(MAX_PAGE_LIMIT))
}

pub(crate) fn requested_offset(
  cursor: Option<&str>,
  offset: Option<usize>,
  decode: fn(&str) -> Result<usize, String>,
) -> Result<Option<usize>, String> {
  match (cursor, offset) {
    (Some(_), Some(_)) => Err("cursor and offset cannot be used together".to_string()),
    (Some(cursor), None) => decode(cursor).map(Some),
    (None, offset) => Ok(offset),
  }
}

pub(crate) fn parse_updated_at_ms(timestamp: Option<&str>) -> Option<i64> {
  let timestamp = timestamp?.trim();
  if timestamp.is_empty() {
    return None;
  }
  timestamp
    .parse::<i64>()
    .ok()
    .or_else(|| {
      DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|value| value.timestamp_millis())
    })
    .filter(|value| (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(value))
}

fn decode_cursor(cursor: &str, prefix: &str) -> Result<usize, String> {
  let encoded = cursor
    .strip_prefix(prefix)
    .filter(|value| !value.is_empty())
    .ok_or_else(|| "invalid pagination cursor".to_string())?;
  usize::from_str_radix(encoded, 16).map_err(|_| "invalid pagination cursor".to_string())
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
  const HEX: &[u8; 16] = b"0123456789abcdef";
  let mut encoded = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    encoded.push(HEX[(byte >> 4) as usize] as char);
    encoded.push(HEX[(byte & 0x0f) as usize] as char);
  }
  encoded
}

pub(crate) fn hex_decode(value: &str) -> Result<Vec<u8>, String> {
  if value.len() & 1 == 1 || value.len() / 2 > MAX_SESSION_KEY_BYTES {
    return Err("invalid session key encoding".to_string());
  }
  value
    .as_bytes()
    .chunks_exact(2)
    .map(|pair| {
      let high = hex_nibble(pair[0]).ok_or_else(|| "invalid session key encoding".to_string())?;
      let low = hex_nibble(pair[1]).ok_or_else(|| "invalid session key encoding".to_string())?;
      Ok(high << 4 | low)
    })
    .collect()
}

fn hex_nibble(value: u8) -> Option<u8> {
  match value {
    b'0'..=b'9' => Some(value - b'0'),
    b'a'..=b'f' => Some(value - b'a' + 10),
    b'A'..=b'F' => Some(value - b'A' + 10),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn session_keys_round_trip_source_identity() {
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::OpenCode,
      session_id: "session/a".to_string(),
      source_path: PathBuf::from("/stores/one/opencode.db"),
    };

    let key = encode_session_key(&locator).expect("key should encode");

    assert!(!key.contains("session/a"));
    assert_eq!(decode_session_key(&key).unwrap(), locator);
  }

  #[test]
  fn keys_reject_unknown_versions_and_malformed_payloads() {
    assert!(decode_session_key("session.v2.00").is_err());
    assert!(decode_session_key("session.v1.not-hex").is_err());

    let locator = SessionLocator {
      version: 2,
      provider: ViewerProvider::Pi,
      session_id: "session".to_string(),
      source_path: PathBuf::from("/tmp/session.jsonl"),
    };
    assert!(decode_session_key(&encode_session_key(&locator).unwrap()).is_err());
  }

  #[test]
  fn timestamps_retain_provider_milliseconds_and_parse_rfc3339() {
    assert_eq!(parse_updated_at_ms(Some("1787157590000")), Some(1_787_157_590_000));
    assert_eq!(
      parse_updated_at_ms(Some("2026-06-04T00:00:00Z")),
      Some(1_780_531_200_000)
    );
    assert_eq!(parse_updated_at_ms(Some("not-a-time")), None);
    assert_eq!(parse_updated_at_ms(Some("9007199254740992")), None);
  }

  #[test]
  fn provider_wire_names_match_the_frontend_contract() {
    for (provider, wire_name) in [
      (ViewerProvider::Codex, "codex"),
      (ViewerProvider::Pi, "pi"),
      (ViewerProvider::OpenCode, "opencode"),
      (ViewerProvider::ZCode, "zcode"),
      (ViewerProvider::WorkBuddy, "workbuddy"),
      (ViewerProvider::Dsh, "dsh"),
    ] {
      assert_eq!(serde_json::to_value(provider).unwrap(), wire_name);
      assert_eq!(
        serde_json::from_value::<ViewerProvider>(wire_name.into()).unwrap(),
        provider
      );
    }
  }

  #[test]
  fn index_progress_uses_the_snake_case_status_center_contract() {
    let progress = SessionIndexProgress::initial(8);
    let value = serde_json::to_value(progress).expect("progress should serialize");

    assert_eq!(value["revision"], "0");
    assert_eq!(value["is_refreshing"], false);
    assert_eq!(value["activity"], "idle");
    assert_eq!(value["catalog"]["scope"], "full");
    assert_eq!(serde_json::to_value(CatalogRefreshScope::Targeted).unwrap(), "targeted");
    assert_eq!(value["catalog"]["total_providers"], ViewerProvider::ALL.len());
    assert_eq!(value["catalog"]["pending_providers"][0], "codex");
    assert_eq!(value["body"]["batch_size"], 8);
    assert_eq!(value["body"]["providers"][0]["provider"], "codex");
    assert_eq!(value["body"]["providers"][0]["total_jobs"], 0);
    assert_eq!(value["body"]["providers"][0]["completed_jobs"], 0);
    assert_eq!(value["body"]["providers"][0]["pending_jobs"], 0);
    assert_eq!(value["worker_error"], serde_json::Value::Null);
    assert!(value.get("isRefreshing").is_none());
  }

  #[test]
  fn limit_is_bounded_and_cursor_cannot_mix_with_offset() {
    assert_eq!(bounded_limit(Some(MAX_PAGE_LIMIT + 1)).unwrap(), MAX_PAGE_LIMIT);
    assert!(bounded_limit(Some(0)).is_err());
    assert!(requested_offset(Some("sessions.v1.1"), Some(1), decode_list_cursor).is_err());
  }

  #[test]
  fn trajectory_keys_and_cursors_are_separate_from_raw_event_keys() {
    let key = encode_trajectory_key(42);
    assert_eq!(key, "trajectory.v1.2a");
    assert_eq!(decode_trajectory_key(&key).unwrap(), 42);
    assert!(decode_event_key(&key).is_err());

    let cursor = encode_trajectory_event_cursor(42, 7);
    assert_eq!(decode_trajectory_event_cursor(&cursor).unwrap(), (42, 7));
    assert!(decode_trajectory_event_cursor("trajectory-events.v1.2a").is_err());
    assert!(decode_trajectory_event_cursor("trajectory-events.v1.2a.7.extra").is_err());
  }
}
