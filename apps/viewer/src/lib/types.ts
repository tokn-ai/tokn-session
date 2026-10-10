export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

/** Availability of the viewer's local translation engine. */
export interface TranslationStatus {
  available: boolean;
  reason: string | null;
}

export interface TranslateTextRequest {
  request_id: string;
  texts: string[];
  target_language: "zh-Hans";
}

export interface TranslateTextResponse {
  texts: string[];
}

export const PROVIDERS = ["codex", "pi", "opencode", "zcode", "workbuddy", "dsh"] as const;

export type ViewerProvider = (typeof PROVIDERS)[number];

export type EventType =
  | "session_started"
  | "provider_changed"
  | "session_settings_applied"
  | "message"
  | "reasoning"
  | "compaction"
  | "goal_updated"
  | "agent_activity"
  | "question_request"
  | "question_reply"
  | "tool_call"
  | "lifecycle"
  | "usage"
  | "metadata"
  | "error"
  | "unknown"
  /** A projected whole-turn timeline entry with lazily loaded source events. */
  | "trajectory";

export type EventPhase = "started" | "delta" | "updated" | "finished";
export type MessageRole = "user" | "assistant" | "system" | "tool" | "unknown";

export type ToolKind =
  | "code_execution"
  | "shell"
  | "terminal"
  | "file_read"
  | "file_write"
  | "file_edit"
  | "search"
  | "web"
  | "task"
  | "unknown";

export type ToolOperationStatus = "pending" | "running" | "completed" | "failed";

export interface ToolCardSummary {
  kind: ToolKind | string;
  tool_name: string | null;
  tool_call_id: string | null;
  /** Derived operation state; absent only when connected to an older backend. */
  status?: ToolOperationStatus | string;
  provider_tool_name?: string | null;
  language?: string | null;
  command: string | null;
  cwd: string | null;
  terminal_session_id?: string | null;
  terminal_action?: "send" | "wait" | string | null;
  chars_len?: number | null;
  wait_ms?: number | null;
  path: string | null;
  query: string | null;
  url: string | null;
  task_title: string | null;
  exit_code: number | null;
  bytes: number | null;
  added: number | null;
  removed: number | null;
}

/**
 * Token counters cross IPC as decimal strings so Rust u64 values retain their
 * exact value in a JavaScript renderer.
 */
export interface UsageCardSummary {
  kind: string;
  input_tokens: string;
  output_tokens: string;
  total_tokens: string | null;
  cache_read_tokens: string | null;
  cache_write_tokens: string | null;
  reasoning_tokens: string | null;
  turn_id: string | null;
  step_id: string | null;
}

/** Safe presentation metadata only; detailed reasoning stays behind event detail. */
export interface ReasoningCardSummary {
  preview: string | null;
  has_summary: boolean;
  has_text: boolean;
  has_encrypted_content: boolean;
  is_redacted: boolean;
}

export type ToolOutputFormat = "text" | "json";

export interface ToolOutputSection {
  label: string | null;
  text: string;
  format: ToolOutputFormat;
}

export interface ToolOutputPreview {
  sections: ToolOutputSection[];
  truncated: boolean;
  original_size_bytes: number;
  source_event_key: string;
}

export type SessionHistoryStatus =
  | "complete"
  | "filtered_subagent"
  | "subagent_body_unavailable";

export interface SessionSummary {
  session_key: string;
  session_id: string;
  parent_session_id: string | null;
  /** True only when the source-neutral parent link resolved safely. */
  is_subagent: boolean;
  provider: ViewerProvider;
  title: string | null;
  preview: string | null;
  project: string | null;
  project_order_ms?: number | null;
  project_key?: string | null;
  cwd: string | null;
  updated_at_ms: number | null;
  timestamp: string | null;
  agent_path: string | null;
  agent_nickname: string | null;
  agent_role: string | null;
  /** Direct descendants discovered from headers; this is not runtime status. */
  child_count: number;
  /** Null for metadata-only listings; event pages provide total_events. */
  message_count: number | null;
  event_count: number | null;
  history_status: SessionHistoryStatus | null;
  /** True when this session has a visible message the viewer has not opened. */
  has_unread: boolean;
  /** Legacy compatibility field; ignored. New servers always return false. */
  has_unread_descendant?: boolean;
  /** Additive fields: older remote hosts still expose the boolean indicator. */
  unread_final_count?: number;
  /** Legacy compatibility field; ignored. New servers always return zero. */
  unread_descendant_count?: number;
  is_running?: boolean;
  has_running_descendant?: boolean;
  question_attention?: { required_count: number; available_count: number };
}

export type SessionOrder = "time" | "project";

export interface SessionListQuery {
  order?: SessionOrder;
  providers?: ViewerProvider[];
  search?: string;
}

export interface SessionInputStatusRequest {
  session_key: string;
}

export interface SessionInputStatus {
  available: boolean;
  message: string;
  /** Maximum number of Unicode code points in one message. */
  max_length: number;
}

export interface SubmitSessionInputRequest {
  session_key: string;
  request_id: string;
  text: string;
}

export interface SubmitSessionInputResponse {
  request_id: string;
  status: "accepted" | "not_sent" | "unknown" | "pending";
  message: string;
}

export interface ListSessionsRequest {
  query: SessionListQuery;
  cursor?: string;
  offset?: number;
  limit?: number;
}

export interface SourceError {
  provider: ViewerProvider;
  message: string;
}

export interface ListSessionsResponse {
  sessions: SessionSummary[];
  next_cursor: string | null;
  source_errors: SourceError[];
  /** Selected provider catalogs that have not completed their first durable index pass. */
  pending_providers: ViewerProvider[];
}

export interface ListSessionChildrenRequest {
  parent_session_key: string;
  cursor?: string;
  offset?: number;
  limit?: number;
}

export interface ListSessionChildrenResponse {
  sessions: SessionSummary[];
  next_cursor: string | null;
}

/**
 * Metadata-only signal emitted after a durable sidebar index refresh.
 * The keys identify only sessions with newly eligible visible messages; they
 * let an already open matching timeline refresh without disturbing one that
 * belongs to an unrelated provider or source.
 */
export interface SessionIndexChangedEvent {
  updated_session_keys?: string[];
  changed: boolean;
  attention_session_keys: string[];
}

export type RelayMode = "automatic" | "external" | "local";
export interface RelaySettings { mode: RelayMode; endpoint: string; include_native: boolean; }
export interface RelayStatus {
  settings: RelaySettings;
  active_endpoint: string | null;
  phase: "local" | "starting" | "connecting" | "live" | "reconnecting" | "retrying" | "failed";
  native: boolean;
  error: string | null;
}
export interface RelayChange { session_key: string | null; reset: boolean; }

/**
 * One in-memory operational snapshot for the durable session index.
 *
 * `revision` is a decimal string because it originates from an in-memory,
 * monotonic progress-store counter and must not lose precision in the browser. It protects
 * an event that arrives while the initial command snapshot is still in flight
 * from being replaced by that older snapshot.
 */
export type SessionIndexActivity = "idle" | "catalog" | "body" | "waiting_to_retry" | "waiting_for_indexer";
/** Whether a catalog activity is complete discovery or a targeted change check. */
export type SessionIndexCatalogScope = "full" | "targeted";
/** Sanitized scheduler-wide failure categories; no provider path or raw error crosses IPC. */
export type SessionIndexWorkerError = "refresh_failed" | "task_failed";

export interface SessionIndexCatalogProgress {
  /**
   * Older hot-reloaded backends omit this field; treat that conservatively as
   * a complete discovery pass in the UI.
   */
  scope?: SessionIndexCatalogScope;
  active_provider: ViewerProvider | null;
  processed_providers: number;
  total_providers: number;
  pending_providers: ViewerProvider[];
  error_providers: ViewerProvider[];
}

export interface SessionIndexBodyProviderProgress {
  provider: ViewerProvider;
  pending_jobs: number;
  failed_jobs: number;
  /** Completed detail loads in the provider's current catalog baseline. */
  completed_jobs: number;
  /** Total detail loads in the provider's current catalog baseline. */
  total_jobs: number;
}

export interface SessionIndexBodyProgress {
  active_provider: ViewerProvider | null;
  pending_jobs: number;
  failed_jobs: number;
  completed_in_run: number;
  stale_in_run: number;
  batch_size: number;
  providers: SessionIndexBodyProviderProgress[];
}

export interface SessionIndexProgress {
  revision: string;
  is_refreshing: boolean;
  activity: SessionIndexActivity;
  catalog: SessionIndexCatalogProgress;
  body: SessionIndexBodyProgress;
  worker_error: SessionIndexWorkerError | null;
  /** Unix milliseconds for a scheduled retry, or null when no retry is queued. */
  retry_at_ms: number | null;
}

/** Local sidebar state for one lazily loaded direct-child page sequence. */
export interface SessionChildrenState {
  sessions: SessionSummary[];
  next_cursor: string | null;
  is_loading: boolean;
  is_loading_more: boolean;
  error: string | null;
}

/**
 * Compact, historical delegation metadata for an `agent_activity` event.
 *
 * `target` is present only when the backend can prove that the activity
 * points at a known direct child in the same provider. It is deliberately not
 * a live subagent-state assertion.
 */
export interface AgentActivityCardSummary {
  kind: string;
  event_id: string | null;
  actor_session_id?: string | null;
  actor_agent_path?: string | null;
  /** Present only when the sender resolves unambiguously relative to this task. */
  actor?: SessionSummary | null;
  target_session_id: string | null;
  target_agent_path: string | null;
  target: SessionSummary | null;
  /** Safe metadata only; readable message text stays behind event detail. */
  communication?: AgentCommunicationCardSummary | null;
}

export interface AgentCommunicationCardSummary {
  has_text: boolean;
  has_encrypted_content: boolean;
  trigger_turn: boolean | null;
}

/**
 * Compact aggregate metadata for a projected whole-turn trajectory.
 *
 * `duration_ms` stays a decimal string because the backend may calculate a
 * duration larger than JavaScript can represent exactly as a Number.
 */
export interface TrajectoryCardSummary {
  status?: "working" | "complete" | "unknown";
  event_count: number;
  tool_count: number;
  reasoning_count: number;
  agent_activity_count: number;
  error_count: number;
  unknown_count: number;
  started_at: string | null;
  ended_at: string | null;
  duration_ms: string | null;
}

export interface EventSummary {
  delivery?: "commentary" | "final" | "unspecified";
  event_key: string;
  /** Stable source slot, used only to restore disclosure after a generation reset. */
  slot_key?: string | null;
  type: EventType | string;
  provider: ViewerProvider;
  timestamp: string | null;
  phase: EventPhase | string | null;
  role: MessageRole | string | null;
  title: string;
  summary: string;
  summary_truncated: boolean;
  is_hidden: boolean;
  is_error: boolean | null;
  /** Explicit backend classification; absent on older servers. */
  is_bookkeeping?: boolean;
  /** Optional while the viewer remains compatible with older backends. */
  agent_activity?: AgentActivityCardSummary | null;
  /** Present only for a projected whole-turn timeline entry. */
  trajectory?: TrajectoryCardSummary | null;
  compaction?: CompactionCardSummary | null;
  tool: ToolCardSummary | null;
  usage: UsageCardSummary | null;
  reasoning: ReasoningCardSummary | null;
}

export interface CompactionCardSummary {
  state: string;
  trigger: string | null;
  reason: string | null;
  has_summary: boolean;
  summary_opaque: boolean;
  measurements: {
    scope: string;
    tokens: string;
    estimated: boolean | null;
  }[];
}

export type EventPageDirection = "forward" | "backward";

export type LoadEventPageRequest = {
  session_key: string;
  limit?: number;
} & ({
  window_mode?: undefined;
  cursor?: string;
  offset?: number;
  direction?: EventPageDirection;
} | {
  /** Retain complete user turns until this session is evicted. */
  window_mode: "retained";
  direction: "backward";
  cursor?: never;
  offset?: never;
} | {
  window_mode: "earlier";
  direction: "backward";
  cursor: string;
  offset?: never;
});

export interface SessionViewRequest {
  view_id: string;
  session_key: string | null;
  candidate_session_keys: string[];
  revision: number;
}

export interface AcknowledgeSessionAttentionRequest {
  session_key: string;
  /** Decimal string to retain an arbitrary SQLite revision exactly. */
  attention_revision: string;
}

export interface AcknowledgeSessionAttentionResponse {
  changed: boolean;
}

export interface EventPageResponse {
  events: EventSummary[];
  next_cursor: string | null;
  previous_cursor: string | null;
  total_events: number;
  history_status: SessionHistoryStatus;
  /** Last follow failure while the server keeps its previous good snapshot. */
  follow_error?: string | null;
  /** Opaque indexed snapshot; absent while connected to an older backend. */
  attention_revision?: string | null;
  outstanding_questions?: OutstandingQuestion[];
}

export interface OutstandingQuestion {
  event_key: string;
  requires_input: boolean;
  unanswered_count: number;
}

export interface LoadTrajectoryEventPageRequest {
  session_key: string;
  trajectory_key: string;
  cursor?: string;
  direction?: EventPageDirection;
  limit?: number;
}

export interface TrajectoryEventPageResponse {
  events: EventSummary[];
  next_cursor: string | null;
  previous_cursor: string | null;
  total_events: number;
}

export type TrajectoryPageLoadDirection = "initial" | "older" | "newer";

/**
 * One locally cached, bounded source-event sequence for a whole-turn card.
 * It is intentionally separate from the parent session's event page state.
 */
export interface TrajectoryEventPageState {
  events: EventSummary[];
  next_cursor: string | null;
  previous_cursor: string | null;
  total_events: number | null;
  has_loaded: boolean;
  is_loading: boolean;
  is_loading_older: boolean;
  is_loading_newer: boolean;
  error: string | null;
  error_direction: TrajectoryPageLoadDirection | null;
  error_cursor: string | null;
}

export interface LoadEventDetailRequest {
  session_key: string;
  event_key: string;
}

export interface EventDetail {
  event_key: string;
  event: JsonValue;
  native: JsonValue | null;
  is_hidden: boolean;
  tool_output: ToolOutputPreview | null;
}

export interface AsyncState<T> {
  data: T;
  error: string | null;
  is_loading: boolean;
}

export interface ExpandedActivityState {
  detail: EventDetail | null;
  error: string | null;
  is_loading: boolean;
}

export type UpdateLevel = "final" | "steps" | "details";
export interface SessionUpdateItem {
  item_id: string;
  kind: "user_message" | "assistant_message" | "notification" | "tool_summary" | "detail" | "work_summary";
  level: UpdateLevel;
  summary?: EventSummary;
  event_key?: string;
  detail?: EventDetail;
  notification?: { type: string; message: string };
}
export interface SessionUpdate {
  subscription_id: string;
  session_key: string;
  level: UpdateLevel;
  generation: string;
  base_revision: string | null;
  revision: string;
  snapshot: boolean;
  items: SessionUpdateItem[];
  groups?: SessionUpdateItem[];
  semantic_order?: string[] | null;
  removed_items: string[];
  item_order: string[] | null;
  state: Omit<EventPageResponse, "events"> & { is_running?: boolean; error?: string };
}
export interface SessionUpdatesRequest {
  subscription_id: string;
  session_key: string;
  level: UpdateLevel;
  cursor: string | null;
  detail_keys: string[];
  unsubscribe?: boolean;
}

export interface SessionNotification {
  session_key: string;
  has_unread: boolean;
  unread_final_count: number;
  is_running: boolean;
  question_attention: SessionSummary["question_attention"];
}
