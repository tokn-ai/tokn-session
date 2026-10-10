import { invoke, isDesktop, listen, type CommandInvoker, type UnlistenFn } from "./transport";
import type {
  RelaySettings, RelayStatus, RelayChange,
  AcknowledgeSessionAttentionRequest,
  AcknowledgeSessionAttentionResponse,
  EventPageResponse,
  ListSessionChildrenRequest,
  ListSessionChildrenResponse,
  ListSessionsRequest,
  ListSessionsResponse,
  LoadEventDetailRequest,
  EventDetail,
  LoadEventPageRequest,
  LoadTrajectoryEventPageRequest,
  SessionViewRequest,
  SessionIndexChangedEvent,
  SessionIndexProgress,
  SessionInputStatus,
  SessionInputStatusRequest,
  SubmitSessionInputRequest,
  SubmitSessionInputResponse,
  TrajectoryEventPageResponse,
  TranslationStatus,
  TranslateTextRequest,
  TranslateTextResponse,
} from "./types";

export function getSessionInputStatus(request: SessionInputStatusRequest): Promise<SessionInputStatus> {
  return invoke<SessionInputStatus>("get_session_input_status", { request });
}

export function submitSessionInput(request: SubmitSessionInputRequest): Promise<SubmitSessionInputResponse> {
  return invoke<SubmitSessionInputResponse>("submit_session_input", { request });
}

export function getTranslationStatus(): Promise<TranslationStatus> {
  return invoke<TranslationStatus>("get_translation_status");
}

export function translateText(request: TranslateTextRequest): Promise<TranslateTextResponse> {
  return invoke<TranslateTextResponse>("translate_text", { request });
}

export function cancelTranslation(requestId: string): Promise<void> {
  return invoke<void>("cancel_translation", { request_id: requestId });
}

export function getRelayStatus(): Promise<RelayStatus> {
  return invoke<RelayStatus>("get_relay_status");
}
export function configureRelay(settings: RelaySettings): Promise<RelayStatus> {
  return invoke<RelayStatus>("configure_relay", { settings });
}
export function listenForRelayStatus(handler: (status: RelayStatus) => void): Promise<UnlistenFn> {
  return listen<RelayStatus>("relay-status", (event) => handler(event.payload));
}
export function listenForRelayChanges(handler: (change: RelayChange) => void): Promise<UnlistenFn> {
  return listen<RelayChange>("relay-changed", (event) => handler(event.payload));
}

/** Browser SSE reconnects require fresh state snapshots; desktop events stay live. */
export function listenForTransportReconnect(handler: () => void): Promise<UnlistenFn> {
  if (isDesktop()) return Promise.resolve(() => {});
  return listen("transport-reconnected", handler);
}

export function listSessions(request: ListSessionsRequest): Promise<ListSessionsResponse> {
  return invoke<ListSessionsResponse>("list_sessions", { request });
}

export function listSessionChildren(
  request: ListSessionChildrenRequest,
): Promise<ListSessionChildrenResponse> {
  return invoke<ListSessionChildrenResponse>("list_session_children", { request });
}

export function updateSessionView(request: SessionViewRequest, send: CommandInvoker = invoke): Promise<void> {
  return send<void>("update_session_view", { request });
}

export function loadEventPage(request: LoadEventPageRequest): Promise<EventPageResponse> {
  return invoke<EventPageResponse>("load_event_page", { request });
}

export function loadTrajectoryEventPage(
  request: LoadTrajectoryEventPageRequest,
): Promise<TrajectoryEventPageResponse> {
  return invoke<TrajectoryEventPageResponse>("load_trajectory_event_page", { request });
}

export function loadEventDetail(request: LoadEventDetailRequest): Promise<EventDetail> {
  return invoke<EventDetail>("load_event_detail", { request });
}

/**
 * Advances the local seen cursor only through the attention revision that was
 * included in an event page the UI actually accepted.
 */
export function acknowledgeSessionAttention(
  request: AcknowledgeSessionAttentionRequest,
): Promise<AcknowledgeSessionAttentionResponse> {
  return invoke<AcknowledgeSessionAttentionResponse>("acknowledge_session_attention", { request });
}

/**
 * The backend emits this after committing a background index refresh. Keeping
 * the subscription here makes the React state hook testable without exposing
 * Tauri event details throughout the UI.
 */
export function listenForSessionIndexChanges(
  handler: (change: SessionIndexChangedEvent) => void,
): Promise<UnlistenFn> {
  return listen<SessionIndexChangedEvent>("session-index-changed", (event) => handler(event.payload));
}

/**
 * Reads the lightweight in-memory operational state for the background index
 * scheduler. It never reads a provider session body.
 */
export function getSessionIndexProgress(): Promise<SessionIndexProgress> {
  return invoke<SessionIndexProgress>("get_session_index_progress");
}

/**
 * Requests an immediate scheduler wake and returns the progress state after it
 * was queued. A later event can still supersede this response.
 */
export function retrySessionIndex(): Promise<SessionIndexProgress> {
  return invoke<SessionIndexProgress>("retry_session_index");
}

/**
 * This subscription is deliberately separate from the sidebar change signal:
 * it reports scheduler progress even when no durable catalog transaction has
 * happened yet.
 */
export function listenForSessionIndexProgress(
  handler: (progress: SessionIndexProgress) => void,
): Promise<UnlistenFn> {
  return listen<SessionIndexProgress>("session-index-progress", (event) => handler(event.payload));
}

export function loadSessionUpdates(request: import("./types").SessionUpdatesRequest): Promise<import("./types").SessionUpdate> {
  return invoke("load_session_updates", { request });
}
export function listenForSessionUpdates(handler: (update: import("./types").SessionUpdate) => void): Promise<UnlistenFn> {
  return listen<import("./types").SessionUpdate>("session-updated", (event) => handler(event.payload));
}

export function listenForSessionNotifications(handler: (notification: import("./types").SessionNotification) => void): Promise<UnlistenFn> {
  return listen<import("./types").SessionNotification>("session-notification", (event) => handler(event.payload));
}
