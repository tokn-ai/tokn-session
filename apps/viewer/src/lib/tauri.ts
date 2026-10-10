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

const legacySubscriptions = new Map<string, import("./types").SessionUpdatesRequest>();

function unknownDelivery(error: unknown): boolean {
  return /unknown.*command|unknown.*variant|command.*not found|unknown viewer api route|unavailable for a session share/i.test(String(error));
}

/** Compatibility facade: modern transports subscribe live, then read backward. */
export async function loadSessionUpdates(request: import("./types").SessionUpdatesRequest): Promise<import("./types").SessionUpdate> {
  try {
    await invoke("subscribe_session", { request });
    if (request.unsubscribe) return { ...request, generation: "released", revision: "0", base_revision: null, snapshot: true,
      items: [], groups: [], removed_items: [], item_order: [], state: { total_events: 0, history_status: "complete", previous_cursor: null, next_cursor: null } };
    return await invoke("load_session_backward", { request });
  } catch (error) {
    if (!unknownDelivery(error)) throw error;
    const update = await invoke<import("./types").SessionUpdate>("load_session_updates", { request });
    if (request.unsubscribe) legacySubscriptions.delete(request.subscription_id);
    else legacySubscriptions.set(request.subscription_id, { ...request, cursor: update.revision });
    while (legacySubscriptions.size > 24) legacySubscriptions.delete(legacySubscriptions.keys().next().value!);
    return update;
  }
}
export async function loadGroupDetails(request: import("./types").SessionUpdatesRequest): Promise<import("./types").SessionUpdate> {
  try { return await invoke("load_session_details", { request: { kind: "group", request } }); }
  catch (error) { if (!unknownDelivery(error)) throw error; return invoke("load_session_updates", { request }); }
}
export async function loadToolDetails(request: LoadEventDetailRequest): Promise<EventDetail> {
  try { return await invoke("load_session_details", { request: { kind: "tool", request } }); }
  catch (error) { if (!unknownDelivery(error)) throw error; return loadEventDetail(request); }
}
export async function inspectSessionEvent(request: LoadEventDetailRequest): Promise<EventDetail> {
  try { return await invoke("inspect_session_event", { request }); }
  catch (error) { if (!unknownDelivery(error)) throw error; return loadEventDetail(request); }
}
export async function renewSessionSubscriptions(ids: string[]): Promise<import("./types").SessionUpdate[]> {
  try { await invoke("renew_session_subscriptions", { ids }); return []; }
  catch (error) {
    if (!unknownDelivery(error)) throw error;
    return Promise.all(ids.filter((id) => legacySubscriptions.has(id)).map(async (id) => {
      const update = await invoke<import("./types").SessionUpdate>("load_session_updates", { request: legacySubscriptions.get(id)! });
      const request = legacySubscriptions.get(id);
      if (request) legacySubscriptions.set(id, { ...request, cursor: update.revision });
      return update;
    }));
  }
}

export function listenForSessionUpdates(handler: (update: import("./types").SessionUpdate) => void): Promise<UnlistenFn> {
  return listen<import("./types").SessionUpdate>("session-updated", (event) => handler(event.payload));
}

export function listenForSessionNotifications(handler: (notification: import("./types").SessionNotification) => void): Promise<UnlistenFn> {
  return listen<import("./types").SessionNotification>("session-notification", (event) => handler(event.payload));
}
