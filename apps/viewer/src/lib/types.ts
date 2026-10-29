export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

/** A remembered encryption identity; readable names never replace its pin. */
export interface HubMachineSelection {
  kind: "hub";
  hub_url: string;
  host_id: string;
}

export type MachineSelection = { kind: "local" } | HubMachineSelection;

/** Network path is independent of viewer lifecycle and always carries Noise E2EE. */
export interface TransportState {
  kind: "direct" | "relay";
  reason?: string;
}

/** Navigation metadata only. Encryption identities stay in device storage. */
export interface MachinePreferences {
  version: 1;
  selected_machine: MachineSelection | null;
  hub_urls: string[];
}

/** Display defaults owned by this viewing device, never by a session host. */
export interface ViewerPreferences {
  version: 1;
  sidebar_order: SessionOrder;
  background_updates_scope: BackgroundUpdatesScope;
  background_update_level: BackgroundUpdateLevel;
  conversation_update_level: ConversationUpdateLevel;
  show_lifecycle: boolean;
}

/** Native viewing-device configuration, independent of the selected machine. */
export interface DevicePreferencesSnapshot {
  version: 1;
  revision: number;
  viewer_preferences: ViewerPreferences;
  machine_preferences: MachinePreferences;
}

/** Ordered updates within this desktop app, shared across its native windows. */
export interface NativePreferenceUpdate<T> {
  revision: number;
  write_id: string;
  preferences: T;
}

export interface InitializeDevicePreferencesRequest {
  viewer_preferences: ViewerPreferences | null;
  machine_preferences: MachinePreferences | null;
}

export interface SavedHubHost {
  host_id: string;
  host_public_key: string;
  machine_address?: string;
  name?: string;
}

/** Public Hub routing metadata. It deliberately contains no encryption key. */
export interface ResolvedMachine {
  host_id: string;
  machine_address: string;
  name: string;
  online: boolean;
}

export interface HubNamespace {
  username: string;
}

/** Availability of the viewer's local translation engine. */
export interface TranslationStatus {
  available: boolean;
  reason: string | null;
}

export type TranslationRecovery = "translation_settings";

/** Native failures may offer a local recovery action without inferring one from text. */
export interface TranslationFailure {
  message: string;
  recovery: TranslationRecovery | null;
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
  | "model_changed"
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
  latest_assistant_preview?: string | null;
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

export interface RegisteredProject {
  project_id: string;
  group_key: string;
  folder_path: string;
  name: string;
  created_at_ms: number;
  available: boolean;
  origin?: "manual" | "discovered";
  discovery_state?: "present" | "absent" | "unknown";
  unavailable_reason?: string | null;
}

export interface ProjectWorkspaceSnapshot {
  available: boolean;
  unavailable_reason: string | null;
  projects: RegisteredProject[];
}

export interface RegisterProjectRequest { folder_path: string; name?: string | null }
export interface CreateProjectSessionRequest {
  request_id: string; project_id: string; prompt: string; image_ids?: string[];
  provider?: RpcProvider; runtime_profile_id?: string | null; runtime_revision?: number | null;
  model?: SessionCreationModelSelection | null;
}
export interface ListSessionCreationModelsRequest extends SessionCreationSelection { project_id: string }
export interface SessionCreationModels {
  models: ModelChoice[]; catalog_revision: string | null; reason: string | null;
  runtime_profile_id: string | null; runtime_revision: number | null;
}
export interface SessionCreationModelSelection {
  model_provider: string | null; model_id: string; catalog_revision: string;
  runtime_profile_id?: string | null; runtime_revision?: number | null;
}
export interface SessionCreationRuntime { profile_id: string; revision: number; name: string; available: boolean; reason: string | null }
export interface SessionCreationProvider {
  provider: RpcProvider; available: boolean; reason: string | null; supports_images: boolean;
  default_profile_id: string | null; runtimes: SessionCreationRuntime[];
}
export interface SessionCreationOptions { project_id: string; providers: SessionCreationProvider[] }
export interface SessionCreationSelection { provider: RpcProvider; runtime_profile_id?: string | null; runtime_revision?: number | null }
export interface SessionCreationError { message: string; may_have_started: boolean }
export type ManagedSessionStatus = "starting" | "running" | "idle" | "stopping" | "stopped" | "failed";
export interface ManagedSessionDialog {
  dialog_id: string;
  kind: string;
  title: string;
  message: string | null;
  options: string[];
  initial_value: string | null;
}
export interface ManagedSessionSnapshot {
  revision: number;
  request_id: string;
  project_id: string;
  provider: string;
  status: ManagedSessionStatus;
  session_id: string | null;
  session_key: string | null;
  session: SessionSummary | null;
  message: string | null;
  /** A provider turn failure can leave its RPC process healthy and idle. */
  last_turn_error?: string | null;
  pending_dialog: ManagedSessionDialog | null;
  creation_input_state?: "not_attempted" | "uncertain" | "accepted" | "rejected" | null;
  creation_phase?: "admitted" | "native_create_uncertain" | "native_known" | "first_input_uncertain" | "history_pending" | "ready" | "failed" | null;
}
export interface ManagedSessionRequest { request_id?: string | null; session_key?: string | null }
export interface ManagedSessionDialogResponse {
  request_id: string;
  dialog_id: string;
  value?: string | null;
  confirmed?: boolean | null;
  cancelled: boolean;
}

export interface SessionListQuery {
  order?: SessionOrder;
  providers?: ViewerProvider[];
  search?: string;
}

export interface SessionInputStatusRequest {
  session_key: string;
}

export type SessionTransport = "rpc" | "desktop";
export interface SessionControlRequest { session_key: string }
export interface SetSessionControlRequest extends SessionControlRequest {
  request_id: string;
  transport: SessionTransport;
}
export interface CancelSessionTurnRequest extends SessionControlRequest { request_id: string }
export interface SessionControlStatus {
  session_key: string;
  provider: ViewerProvider;
  transport: SessionTransport;
  can_select: boolean;
  can_cancel: boolean;
  available: boolean;
  message: string;
  runtime_id: string | null;
  instance_id: string | null;
}

export interface ModelIdentity {
  model_provider: string | null;
  model_id: string;
  display_name: string | null;
}

export interface SessionModelState {
  model: ModelIdentity | null;
  source: "live" | "recorded" | "catalog" | "unknown";
  revision: string;
}

export interface SessionModelStatusRequest { session_key: string }
export interface SessionModelStatus {
  state: SessionModelState;
  can_select: boolean;
  can_list: boolean;
  reason: string | null;
  runtime_generation: string | null;
  updates_pi_default: boolean;
  /** Omitted by older hosts; draft model selection then remains read-only. */
  supports_model_input?: boolean;
}
export interface ModelChoice { model: ModelIdentity; supports_images: boolean }
export interface ListSessionModelsResponse { status: SessionModelStatus; models: ModelChoice[] }
export interface MessageModelSelection {
  model_provider: string | null;
  model_id: string;
  expected_revision: string;
  runtime_generation: string | null;
}
export interface MessageModelResult {
  outcome: "unchanged" | "applied" | "ignored_while_running";
  state: SessionModelState;
}

export interface SessionInputStatus {
  available: boolean;
  message: string;
  /** Sending will start the saved viewer-owned process before delivering text. */
  resume_required?: boolean;
  /** Maximum number of Unicode code points in one message. */
  max_length: number;
  /** Omitted by older hosts, which must remain text-only. */
  images_available?: boolean;
  /** Upload staging is independent of the current model's image support. */
  images_stageable?: boolean;
  max_images?: number;
  max_image_bytes?: number;
  max_total_image_bytes?: number;
  image_mime_types?: string[];
}

export interface SubmitSessionInputRequest {
  session_key: string;
  request_id: string;
  text: string;
  image_ids?: string[];
  model_selection?: MessageModelSelection;
}

export interface SubmitSessionInputResponse {
  request_id: string;
  status: "accepted" | "not_sent" | "unknown" | "pending";
  message: string;
  model_result?: MessageModelResult;
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
  catalog_refresh_required?: boolean;
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

/** Machine-owned metadata, never viewer preferences or pairing credentials. */
export interface MachineSettingsSnapshot {
  version: 1;
  reader: { status: RelayStatus; editable: boolean; edit_reason: string | null };
  sources: {
    provider: ViewerProvider;
    locations: { path: string; kind: "directory" | "database"; exists: boolean }[];
    error: string | null;
  }[];
  sources_editable: boolean;
  sources_edit_reason: string | null;
  hosting: { available: boolean; editable: boolean; reason: string | null };
}
export interface RelayChange { session_key: string | null; reset: boolean; }

export type RuntimeKind = "pi" | "reader" | "hosting" | "hub_connection" | "background_api" | "background_connector";
export interface RuntimeInfo {
  runtime_id: string;
  instance_id: string;
  name: string;
  kind: RuntimeKind;
  execution_kind: "subprocess" | "embedded" | "external";
  ownership: "viewer" | "external";
  status: string;
  pid: number | null;
  started_at_ms: number | null;
  session_key: string | null;
  session_id: string | null;
  project_id: string | null;
  working_directory: string | null;
  endpoint: string | null;
  detail: string | null;
  last_error: string | null;
  can_restart: boolean;
  restart_disabled_reason: string | null;
}
export interface RuntimeInventory { version: 1; runtimes: RuntimeInfo[]; }
export interface RuntimeRestartRequest { request_id: string; runtime_id: string; expected_instance_id: string; }
export interface RuntimeRestartResponse { request_id: string; runtime_id: string; message: string; }

export type RpcProvider = "codex" | "pi" | "open_code" | "dsh";
export type RpcRuntimeMode = "managed" | "attached";
export interface RpcRuntimeProfile {
  profile_id: string;
  name: string;
  provider: RpcProvider;
  mode: RpcRuntimeMode;
  executable: string | null;
  arguments: string[];
  working_directory: string | null;
  endpoint: string | null;
  environment_keys: string[];
  credential_kind: "bearer" | "basic" | null;
  is_default: boolean;
}
export type RpcRuntimeCredentials = { type: "none" } | { type: "bearer"; token: string } | { type: "basic"; username: string; password: string };
export interface RpcRuntimeProfileInput {
  profile_id: string | null;
  name: string;
  provider: RpcProvider;
  mode: RpcRuntimeMode;
  executable: string | null;
  arguments: string[];
  working_directory: string | null;
  endpoint: string | null;
  environment: Record<string, string> | null;
  credentials: RpcRuntimeCredentials | null;
  is_default: boolean;
}
export interface RpcRuntimeProfilesSnapshot { version: 1; revision: number; profiles: RpcRuntimeProfile[]; }
export interface SaveRpcRuntimeProfileRequest { expected_revision: number; profile: RpcRuntimeProfileInput; }
export interface DeleteRpcRuntimeProfileRequest { expected_revision: number; profile_id: string; }
export interface RpcRuntimeProfileValidation {
  valid: boolean;
  issues: { field: string; message: string }[];
  executable_exists: boolean | null;
  working_directory_exists: boolean | null;
}
export type RpcOperation = "metadata" | "state" | "models" | "message" | "steer" | "queue" | "cancel_turn" | "respond" | "pending_interactions";
export interface RpcRuntimeInfo {
  runtime_id: string;
  instance_id: string;
  profile_id: string;
  profile_revision: number;
  name: string;
  provider: RpcProvider;
  ownership: "managed" | "attached";
  status: "connected" | "disconnected" | "recovery_required" | "closing" | "cleanup_failed";
  capabilities: { operations: Partial<Record<RpcOperation, { supported: boolean; reason: string | null }>>; atomic_model_on_send: boolean };
  session_count: number;
  last_error: string | null;
}
export interface RpcRuntimeInventory { version: 1; runtimes: RpcRuntimeInfo[]; }
export interface StartRpcRuntimeRequest { request_id: string; profile_id: string; expected_revision: number; }
export interface CloseRpcRuntimeRequest { request_id: string; runtime_id: string; expected_instance_id: string; }
export interface CloseRpcRuntimeResponse { runtime_id: string; instance_id: string; closed: true; }
export interface RpcRuntimeAccess { available: boolean; enabled: boolean; reason: string | null; }

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
  /** Present sessions whose detail baseline is durably current. */
  completed_jobs: number;
  /** Present sessions with a current or pending detail baseline. */
  total_jobs: number;
}

export interface SessionIndexBodyProgress {
  active_provider: ViewerProvider | null;
  pending_jobs: number;
  failed_jobs: number;
  /** Successful commits in the current or most recent refresh batch. */
  completed_in_run: number;
  /** Outdated attempts in that batch; these do not count as completed jobs. */
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

export interface QuestionSelectionSummary {
  question_id: string;
  option_indices: number[];
  written_answer?: string;
  written_answer_truncated?: boolean;
}

export interface MessageImageSummary {
  /** Exact full-message Markdown span, measured in UTF-16 code units. */
  markdown_range?: { start: number; end: number };
  /** Raw owner of an image projected into a grouped tool operation. */
  source_event_key?: string;
  image_id: string;
  mime_type: string | null;
  name: string | null;
  availability: "available" | "unavailable";
}

export interface LoadSessionImageRequest {
  session_key: string;
  event_key: string;
  image_id: string;
  variant: "thumbnail" | "full";
}
export interface LoadSessionImageResponse {
  mime_type: string;
  data_base64: string;
  width: number | null;
  height: number | null;
}
export type ImageUploadTarget = { type: "session"; session_key: string } | { type: "project"; project_id: string };
export interface UploadImageChunkRequest {
  target: ImageUploadTarget;
  upload_id: string;
  offset: number;
  total_bytes: number;
  mime_type: string;
  name: string | null;
  data_base64: string;
}
export interface UploadImageChunkResponse {
  upload_id: string;
  received_bytes: number;
  image_id: string | null;
}
export interface ReleaseImageUploadRequest { target: ImageUploadTarget; upload_id: string }

export interface EventSummary {
  model_change?: { previous: ModelIdentity; current: ModelIdentity };
  images?: MessageImageSummary[];
  question_selections?: QuestionSelectionSummary[];
  child_keys?: string[];
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
  /** Digest of the full message text when the summary is truncated. */
  content_revision?: string | null;
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
  current_model?: SessionModelState;
  is_running?: boolean;
  source_revision?: string;
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

/** Complete semantic children used to identify independently loaded inner groups. */
export interface TrajectoryEventPageState {
  events: EventSummary[];
  total_events: number | null;
  has_loaded: boolean;
  is_loading: boolean;
  error: string | null;
}

export interface LoadEventDetailRequest {
  session_key: string;
  event_key: string;
}

export interface EventDetail {
  event_key: string;
  event: JsonValue;
  /** Digest of the full assistant message text, when supplied by the backend. */
  content_revision?: string | null;
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

export type UpdateLevel = "final" | "messages" | "steps" | "details" | "all";
export type ConversationUpdateLevel = Exclude<UpdateLevel, "details">;
export interface SessionUpdateItem {
  item_id: string;
  kind: "user_message" | "assistant_message" | "notification" | "tool_summary" | "detail" | "work_summary" | "event";
  level: UpdateLevel;
  summary?: EventSummary;
  event_key?: string;
  detail?: EventDetail;
  event?: EventDetail;
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
  event_order?: string[] | null;
  removed_items: string[];
  item_order: string[] | null;
  state: Omit<EventPageResponse, "events"> & { is_running?: boolean; error?: string; scope?: SessionUpdateScope };
}
export interface SessionUpdateScope {
  history: "latest_turn" | "recent_turns" | "retained";
  turn_key?: string | null;
  group_keys: string[];
}
export interface SessionUpdatesRequest {
  subscription_id: string;
  session_key: string;
  level: UpdateLevel;
  cursor: string | null;
  detail_keys: string[];
  scope?: SessionUpdateScope;
  history_cursor?: string | null;
  unsubscribe?: boolean;
}

export interface SessionSubscriptionRenewal {
  renewed_ids: string[];
  missing_ids: string[];
}

export interface SessionSubscriptionRenewalResult extends SessionSubscriptionRenewal {
  updates: SessionUpdate[];
}

export interface SessionSubscriptionLoss {
  subscription_ids: string[];
}

export interface SessionSubscriptionReconnect {
  subscription_ids?: string[];
  background_subscription_ids?: string[];
}

export interface SessionNotification {
  session_key: string;
  has_unread: boolean;
  unread_final_count: number;
  is_running: boolean;
  /** Additive for compatibility with hosts that only report direct activity. */
  has_running_descendant?: boolean;
  question_attention: SessionSummary["question_attention"];
}

/** Compact live interests never imply historical or detail loading. */
export type BackgroundUpdatesScope = "all" | "sidebar";
export type BackgroundUpdateLevel = "final" | "messages";
export interface BackgroundUpdatesRequest {
  subscription_id: string;
  scope: BackgroundUpdatesScope;
  /** Omitted for the default messages preset to support older hosts. */
  level?: BackgroundUpdateLevel;
  session_keys: string[];
  unsubscribe?: boolean;
}
export interface BackgroundUpdatesSubscription {
  subscription_id: string;
  generation: string;
  revision: string;
  scope: BackgroundUpdatesScope;
  level?: BackgroundUpdateLevel;
  subscribed?: boolean;
  unsubscribed?: boolean;
}
export interface BackgroundAssistantMessage {
  message_id: string | null;
  delivery: string | null;
  phase: string | null;
  preview: string | null;
}
export interface BackgroundSessionUpdate extends SessionNotification {
  level?: BackgroundUpdateLevel;
  removed?: boolean;
  subscription_id: string;
  generation: string;
  revision: string;
  latest_assistant_message: BackgroundAssistantMessage | null;
  message_revision: string | null;
}

export interface BackgroundServiceStatus {
  registered: boolean;
  loaded: boolean;
  running: boolean;
  verified: boolean;
  error: string | null;
}
export interface LocalHostStatus {
  background_services?: { api: BackgroundServiceStatus; connector: BackgroundServiceStatus };
  external_remove_supported?: boolean;
  external_stop_supported?: boolean;
  phase: "stopped" | "external" | "background" | "unavailable" | "connecting" | "online" | "reconnecting" | "error";
  hub_url: string;
  name: string;
  allow_control: boolean;
  machine_reference: string | null;
  error: string | null;
}
export interface StartLocalHostRequest {
  hub_url: string;
  name: string;
  allow_control: boolean;
}
export interface LocalHostPairing {
  machine_reference: string;
  qr_data_url: string;
  setup_uri: string;
  current_code: string;
  host_time: number;
  expires_at: number;
}

/** Read-only projection of normalized question details. */
export interface QuestionDisplayOption {
  option_index: number;
  label: string;
  description: string | null;
  is_recommended: boolean;
  is_selected: boolean;
}

export interface QuestionDisplayItem {
  question_id: string | null;
  header: string | null;
  question: string | null;
  options: QuestionDisplayOption[];
  answers: string[];
  written_answer: string | null;
  written_answer_truncated: boolean;
  allows_free_text: boolean;
  is_secret: boolean;
}

export interface QuestionDisplay {
  request_id: string | null;
  kind: "request" | "reply";
  is_blocking: boolean | null;
  questions: QuestionDisplayItem[];
}

export interface SessionQuestionStatusRequest {
  session_key: string;
  question_request_id: string;
}
export interface SessionQuestionStatus {
  available: boolean;
  message: string;
  question_ids: string[];
  /** Live callbacks can request secrecy absent from the recorded projection. */
  secret_question_ids?: string[];
  max_length: number;
}
export interface SessionQuestionAnswer {
  question_id: string;
  option_index?: number;
  text?: string;
}
export interface SubmitSessionQuestionAnswersRequest extends SessionQuestionStatusRequest {
  request_id: string;
  answers: SessionQuestionAnswer[];
}
