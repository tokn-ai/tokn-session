import { invoke, isDesktop, listen, type CommandInvoker, type EventSubscriber, type UnlistenFn } from "./transport";
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

/** Prepare this device's viewer independently of the selected remote transport. */
export async function initializeLocalViewer(): Promise<void> {
  if (!isDesktop()) throw new Error("This machine is available in the desktop app.");
  await (await import("@tauri-apps/api/core")).invoke<void>("initialize_local_viewer");
}

export function submitSessionInput(request: SubmitSessionInputRequest): Promise<SubmitSessionInputResponse> {
  return invoke<SubmitSessionInputResponse>("submit_session_input", { request });
}

export function getTranslationStatus(): Promise<TranslationStatus> {
  return localTranslation<TranslationStatus>("get_translation_status");
}

export function translateText(request: TranslateTextRequest): Promise<TranslateTextResponse> {
  return localTranslation<TranslateTextResponse>("translate_text", { request });
}

export function cancelTranslation(requestId: string): Promise<void> {
  return localTranslation<void>("cancel_translation", { request_id: requestId });
}

async function localTranslation<T>(command: string, payload?: Record<string, unknown>): Promise<T> {
  // Translation belongs to the viewing device even when session data is remote.
  if (isDesktop()) return (await import("@tauri-apps/api/core")).invoke<T>(command,payload);
  return invoke<T>(command,payload);
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
export function listenForRelayChanges(handler: (change: RelayChange) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<RelayChange>("relay-changed", (event) => handler(event.payload));
}

/** Browser SSE reconnects require fresh state snapshots; desktop events stay live. */
export function listenForTransportReconnect(handler: () => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe("transport-reconnected", handler);
}

export function listSessions(request: ListSessionsRequest, send: CommandInvoker = invoke): Promise<ListSessionsResponse> {
  return send<ListSessionsResponse>("list_sessions", { request });
}

export function listSessionChildren(
  request: ListSessionChildrenRequest,
  send: CommandInvoker = invoke,
): Promise<ListSessionChildrenResponse> {
  return send<ListSessionChildrenResponse>("list_session_children", { request });
}

export function updateSessionView(request: SessionViewRequest, send: CommandInvoker = invoke): Promise<void> {
  return send<void>("update_session_view", { request });
}

export function loadEventPage(request: LoadEventPageRequest, send: CommandInvoker = invoke): Promise<EventPageResponse> {
  return send<EventPageResponse>("load_event_page", { request });
}

export function loadTrajectoryEventPage(
  request: LoadTrajectoryEventPageRequest,
  send: CommandInvoker = invoke,
): Promise<TrajectoryEventPageResponse> {
  return send<TrajectoryEventPageResponse>("load_trajectory_event_page", { request });
}

export function loadEventDetail(request: LoadEventDetailRequest, send: CommandInvoker = invoke): Promise<EventDetail> {
  return send<EventDetail>("load_event_detail", { request });
}

/**
 * Advances the local seen cursor only through the attention revision that was
 * included in an event page the UI actually accepted.
 */
export function acknowledgeSessionAttention(
  request: AcknowledgeSessionAttentionRequest,
  send: CommandInvoker = invoke,
): Promise<AcknowledgeSessionAttentionResponse> {
  return send<AcknowledgeSessionAttentionResponse>("acknowledge_session_attention", { request });
}

/**
 * The backend emits this after committing a background index refresh. Keeping
 * the subscription here makes the React state hook testable without exposing
 * Tauri event details throughout the UI.
 */
export function listenForSessionIndexChanges(
  handler: (change: SessionIndexChangedEvent) => void,
  subscribe: EventSubscriber = listen,
): Promise<UnlistenFn> {
  return subscribe<SessionIndexChangedEvent>("session-index-changed", (event) => handler(event.payload));
}

/**
 * Reads the lightweight in-memory operational state for the background index
 * scheduler. It never reads a provider session body.
 */
export function getSessionIndexProgress(send: CommandInvoker = invoke): Promise<SessionIndexProgress> {
  return send<SessionIndexProgress>("get_session_index_progress");
}

/**
 * Requests an immediate scheduler wake and returns the progress state after it
 * was queued. A later event can still supersede this response.
 */
export function retrySessionIndex(send: CommandInvoker = invoke): Promise<SessionIndexProgress> {
  return send<SessionIndexProgress>("retry_session_index");
}

/**
 * This subscription is deliberately separate from the sidebar change signal:
 * it reports scheduler progress even when no durable catalog transaction has
 * happened yet.
 */
export function listenForSessionIndexProgress(
  handler: (progress: SessionIndexProgress) => void,
  subscribe: EventSubscriber = listen,
): Promise<UnlistenFn> {
  return subscribe<SessionIndexProgress>("session-index-progress", (event) => handler(event.payload));
}

// Compatibility leases belong to the same captured machine as their requests.
const legacySubscriptions = new WeakMap<CommandInvoker, Map<string, import("./types").SessionUpdatesRequest>>();

function legacySubscriptionsFor(send: CommandInvoker): Map<string, import("./types").SessionUpdatesRequest> {
  let subscriptions = legacySubscriptions.get(send);
  if (!subscriptions) {
    subscriptions = new Map();
    legacySubscriptions.set(send, subscriptions);
  }
  return subscriptions;
}

function unknownDelivery(error: unknown): boolean {
  return /unknown.*command|unknown.*variant|command.*not found|unknown viewer api route|unavailable for a session share/i.test(String(error));
}

/** Compatibility facade: modern transports subscribe live, then read backward. */
export async function loadSessionUpdates(request: import("./types").SessionUpdatesRequest, send: CommandInvoker = invoke): Promise<import("./types").SessionUpdate> {
  try {
    await send("subscribe_session", { request });
    if (request.unsubscribe) return { ...request, generation: "released", revision: "0", base_revision: null, snapshot: true,
      items: [], groups: [], removed_items: [], item_order: [], state: { total_events: 0, history_status: "complete", previous_cursor: null, next_cursor: null } };
    return await send("load_session_backward", { request });
  } catch (error) {
    if (!unknownDelivery(error)) throw error;
    const update = await send<import("./types").SessionUpdate>("load_session_updates", { request });
    const subscriptions = legacySubscriptionsFor(send);
    if (request.unsubscribe) subscriptions.delete(request.subscription_id);
    else subscriptions.set(request.subscription_id, { ...request, cursor: update.revision });
    while (subscriptions.size > 24) subscriptions.delete(subscriptions.keys().next().value!);
    return update;
  }
}
export async function loadGroupDetails(request: import("./types").SessionUpdatesRequest, send: CommandInvoker = invoke): Promise<import("./types").SessionUpdate> {
  try { return await send("load_session_details", { request: { kind: "group", request } }); }
  catch (error) { if (!unknownDelivery(error)) throw error; return send("load_session_updates", { request }); }
}
export async function loadToolDetails(request: LoadEventDetailRequest, send: CommandInvoker = invoke): Promise<EventDetail> {
  try { return await send("load_session_details", { request: { kind: "tool", request } }); }
  catch (error) { if (!unknownDelivery(error)) throw error; return loadEventDetail(request, send); }
}
export async function inspectSessionEvent(request: LoadEventDetailRequest, send: CommandInvoker = invoke): Promise<EventDetail> {
  try { return await send("inspect_session_event", { request }); }
  catch (error) { if (!unknownDelivery(error)) throw error; return loadEventDetail(request, send); }
}
export async function renewSessionSubscriptions(ids: string[], send: CommandInvoker = invoke): Promise<import("./types").SessionUpdate[]> {
  try { await send("renew_session_subscriptions", { ids }); return []; }
  catch (error) {
    if (!unknownDelivery(error)) throw error;
    const subscriptions = legacySubscriptionsFor(send);
    return Promise.all(ids.filter((id) => subscriptions.has(id)).map(async (id) => {
      const update = await send<import("./types").SessionUpdate>("load_session_updates", { request: subscriptions.get(id)! });
      const request = subscriptions.get(id);
      if (request) subscriptions.set(id, { ...request, cursor: update.revision });
      return update;
    }));
  }
}

export function listenForSessionUpdates(handler: (update: import("./types").SessionUpdate) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<import("./types").SessionUpdate>("session-updated", (event) => handler(event.payload));
}

export function listenForSessionNotifications(handler: (notification: import("./types").SessionNotification) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<import("./types").SessionNotification>("session-notification", (event) => handler(event.payload));
}

// Hosting belongs to this app, independent of the currently selected machine.
async function localHostCommand<T>(command: string, payload?: Record<string, unknown>): Promise<T> {
  if (!isDesktop()) throw new Error("Host setup is available in the desktop app.");
  return (await import("@tauri-apps/api/core")).invoke<T>(command, payload);
}
export function getLocalHostStatus(): Promise<import("./types").LocalHostStatus> {
  return localHostCommand("local_host_status");
}
export function startLocalHost(request: import("./types").StartLocalHostRequest): Promise<import("./types").LocalHostStatus> {
  return localHostCommand("local_host_start", { request });
}
export function stopLocalHost(): Promise<import("./types").LocalHostStatus> {
  return localHostCommand("local_host_stop");
}
export function stopExternalLocalHost(): Promise<import("./types").LocalHostStatus> {
  return localHostCommand("local_host_stop_external");
}
export function getLocalHostPairing(): Promise<import("./types").LocalHostPairing> {
  return localHostCommand("local_host_pairing");
}
export async function listenForLocalHostStatus(handler: (status: import("./types").LocalHostStatus) => void): Promise<UnlistenFn> {
  if (!isDesktop()) throw new Error("Host setup is available in the desktop app.");
  return (await import("@tauri-apps/api/event")).listen<import("./types").LocalHostStatus>("local-host-status", (event) => handler(event.payload));
}
