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
  SessionSubscriptionLoss,
  SessionSubscriptionReconnect,
  SessionSubscriptionRenewal,
  SessionSubscriptionRenewalResult,
  SubmitSessionInputRequest,
  SubmitSessionInputResponse,
  TrajectoryEventPageResponse,
  TranslationStatus,
  TranslateTextRequest,
  TranslateTextResponse,
  DevicePreferencesSnapshot,
  InitializeDevicePreferencesRequest,
  MachinePreferences,
  NativePreferenceUpdate,
  ViewerPreferences,
  RegisteredProject, ProjectWorkspaceSnapshot, RegisterProjectRequest, CreateProjectSessionRequest,
  ManagedSessionRequest, ManagedSessionSnapshot, ManagedSessionDialogResponse,
  RuntimeInventory, RuntimeRestartRequest, RuntimeRestartResponse,
  RpcRuntimeProfilesSnapshot, SaveRpcRuntimeProfileRequest, DeleteRpcRuntimeProfileRequest,
  RpcRuntimeProfileInput, RpcRuntimeProfileValidation, RpcRuntimeInventory, RpcRuntimeInfo,
  StartRpcRuntimeRequest, CloseRpcRuntimeRequest, CloseRpcRuntimeResponse,
  RpcRuntimeAccess,
  SessionModelStatusRequest, SessionModelStatus, ListSessionModelsResponse,
} from "./types";

export function getRpcRuntimeProfiles(send: CommandInvoker = invoke): Promise<RpcRuntimeProfilesSnapshot> {
  return send("get_rpc_runtime_profiles");
}
export function saveRpcRuntimeProfile(request: SaveRpcRuntimeProfileRequest, send: CommandInvoker = invoke): Promise<RpcRuntimeProfilesSnapshot> {
  return send("save_rpc_runtime_profile", { request });
}
export function deleteRpcRuntimeProfile(request: DeleteRpcRuntimeProfileRequest, send: CommandInvoker = invoke): Promise<RpcRuntimeProfilesSnapshot> {
  return send("delete_rpc_runtime_profile", { request });
}
export function validateRpcRuntimeProfile(request: RpcRuntimeProfileInput, send: CommandInvoker = invoke): Promise<RpcRuntimeProfileValidation> {
  return send("validate_rpc_runtime_profile", { request });
}
export function listRpcRuntimes(send: CommandInvoker = invoke): Promise<RpcRuntimeInventory> {
  return send("list_rpc_runtimes");
}
export function startRpcRuntime(request: StartRpcRuntimeRequest, send: CommandInvoker = invoke): Promise<RpcRuntimeInfo> {
  return send("start_rpc_runtime", { request });
}
export function closeRpcRuntime(request: CloseRpcRuntimeRequest, send: CommandInvoker = invoke): Promise<CloseRpcRuntimeResponse> {
  return send("close_rpc_runtime", { request });
}
export function getRpcRuntimeAccess(): Promise<RpcRuntimeAccess> {
  return viewerRuntimeCommand("get_rpc_runtime_access");
}
export function setRpcRuntimeAccess(request: { enabled: boolean }): Promise<RpcRuntimeAccess> {
  return viewerRuntimeCommand("set_rpc_runtime_access", { request });
}

export function listRegisteredProjects(send: CommandInvoker = invoke): Promise<ProjectWorkspaceSnapshot> {
  return send("list_registered_projects");
}
export function registerProject(request: RegisterProjectRequest, send: CommandInvoker = invoke): Promise<RegisteredProject> {
  return send("register_project", { request });
}
export function createProjectSession(request: CreateProjectSessionRequest, send: CommandInvoker = invoke): Promise<ManagedSessionSnapshot> {
  return send("create_project_session", { request });
}
export function getManagedSession(request: ManagedSessionRequest, send: CommandInvoker = invoke): Promise<ManagedSessionSnapshot | null> {
  return send("get_managed_session", { request });
}
export function stopManagedSession(request: ManagedSessionRequest, send: CommandInvoker = invoke): Promise<ManagedSessionSnapshot> {
  return send("stop_managed_session", { request });
}
export function resumeManagedSession(request: ManagedSessionRequest, send: CommandInvoker = invoke): Promise<ManagedSessionSnapshot> {
  return send("resume_managed_session", { request });
}
export function respondManagedSessionDialog(request: ManagedSessionDialogResponse, send: CommandInvoker = invoke): Promise<ManagedSessionSnapshot> {
  return send("respond_managed_session_dialog", { request });
}
export function listenForManagedSessionChanges(handler: (snapshot: ManagedSessionSnapshot) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<ManagedSessionSnapshot>("managed-session-changed", (event) => handler(event.payload));
}

export function getSessionInputStatus(request: SessionInputStatusRequest): Promise<SessionInputStatus> {
  return invoke<SessionInputStatus>("get_session_input_status", { request });
}

export function getSessionModelStatus(request: SessionModelStatusRequest, send: CommandInvoker = invoke): Promise<SessionModelStatus> {
  return send("get_session_model_status", { request });
}
export function listSessionModels(request: SessionModelStatusRequest, send: CommandInvoker = invoke): Promise<ListSessionModelsResponse> {
  return send("list_session_models", { request });
}

/** Prepare this device's viewer independently of the selected remote transport. */
export async function initializeLocalViewer(): Promise<void> {
  if (!isDesktop()) throw new Error("This machine is available in the desktop app.");
  await (await import("@tauri-apps/api/core")).invoke<void>("initialize_local_viewer");
}

/** Runtime ownership belongs to this desktop viewer, not the selected machine. */
async function viewerRuntimeCommand<T>(command: string, payload?: Record<string, unknown>): Promise<T> {
  if (!isDesktop()) throw new Error("Runtime management is available in the desktop app.");
  return (await import("@tauri-apps/api/core")).invoke<T>(command, payload);
}
export function listViewerRuntimes(): Promise<RuntimeInventory> {
  return viewerRuntimeCommand("list_viewer_runtimes");
}
export function restartViewerRuntime(request: RuntimeRestartRequest): Promise<RuntimeRestartResponse> {
  return viewerRuntimeCommand("restart_viewer_runtime", { request });
}
export async function listenForViewerRuntimeChanges(handler: () => void): Promise<UnlistenFn> {
  if (!isDesktop()) throw new Error("Runtime management is available in the desktop app.");
  return (await import("@tauri-apps/api/event")).listen("viewer-runtimes-changed", handler);
}

async function nativeDeviceCommand<T>(command: string, payload: Record<string, unknown>): Promise<T> {
  if (!isDesktop()) throw new Error("Native device preferences are available in the desktop app.");
  return (await import("@tauri-apps/api/core")).invoke<T>(command, payload);
}

export function initializeDevicePreferences(request: InitializeDevicePreferencesRequest): Promise<DevicePreferencesSnapshot> {
  return nativeDeviceCommand("initialize_device_preferences", { request });
}

export function writeViewerPreferences(preferences: ViewerPreferences, write_id: string): Promise<NativePreferenceUpdate<ViewerPreferences>> {
  return nativeDeviceCommand("write_viewer_preferences", { preferences, write_id });
}

export function writeMachinePreferences(preferences: MachinePreferences, write_id: string): Promise<NativePreferenceUpdate<MachinePreferences>> {
  return nativeDeviceCommand("write_machine_preferences", { preferences, write_id });
}

async function nativeDeviceListener<T>(name: string, handler: (preferences: T) => void): Promise<UnlistenFn> {
  if (!isDesktop()) throw new Error("Native device preferences are available in the desktop app.");
  return (await import("@tauri-apps/api/event")).listen<T>(name, (event) => handler(event.payload));
}

export function listenForViewerPreferences(handler: (preferences: NativePreferenceUpdate<ViewerPreferences>) => void): Promise<UnlistenFn> {
  return nativeDeviceListener("viewer-preferences-changed", handler);
}

export function listenForMachinePreferences(handler: (preferences: NativePreferenceUpdate<MachinePreferences>) => void): Promise<UnlistenFn> {
  return nativeDeviceListener("machine-preferences-changed", handler);
}

export function submitSessionInput(request: SubmitSessionInputRequest, send: CommandInvoker = invoke): Promise<SubmitSessionInputResponse> {
  return send<SubmitSessionInputResponse>("submit_session_input", { request });
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

export async function openTranslationSettings(): Promise<void> {
  if (!isDesktop()) throw new Error("Translation Settings are available in the macOS desktop app.");
  await (await import("@tauri-apps/api/core")).invoke<void>("open_translation_settings");
}

async function localTranslation<T>(command: string, payload?: Record<string, unknown>): Promise<T> {
  // Translation belongs to the viewing device even when session data is remote.
  if (isDesktop()) return (await import("@tauri-apps/api/core")).invoke<T>(command,payload);
  return invoke<T>(command,payload);
}

export function getRelayStatus(): Promise<RelayStatus> {
  return invoke<RelayStatus>("get_relay_status");
}
export function configureRelay(settings: RelaySettings, send: CommandInvoker = invoke): Promise<RelayStatus> {
  return send<RelayStatus>("configure_relay", { settings });
}
export function getMachineSettings(send: CommandInvoker = invoke): Promise<import("./types").MachineSettingsSnapshot> {
  return send("get_machine_settings");
}
export function listenForRelayStatus(handler: (status: RelayStatus) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<RelayStatus>("relay-status", (event) => handler(event.payload));
}
export function listenForRelayChanges(handler: (change: RelayChange) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<RelayChange>("relay-changed", (event) => handler(event.payload));
}

/** Browser SSE reconnects require fresh state snapshots; desktop events stay live. */
export function listenForTransportReconnect(handler: (scope?: SessionSubscriptionReconnect) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<SessionSubscriptionReconnect>("transport-reconnected", (event) => handler(event.payload));
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

async function loadLegacyUpdate(request: import("./types").SessionUpdatesRequest, send: CommandInvoker): Promise<import("./types").SessionUpdate> {
  const update = await send<import("./types").SessionUpdate>("load_session_updates", { request });
  const subscriptions = legacySubscriptionsFor(send);
  if (request.unsubscribe) subscriptions.delete(request.subscription_id);
  else subscriptions.set(request.subscription_id, { ...request, cursor: update.revision });
  while (subscriptions.size > 24) subscriptions.delete(subscriptions.keys().next().value!);
  return update;
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
    return loadLegacyUpdate(request, send);
  }
}
export async function loadGroupDetails(request: import("./types").SessionUpdatesRequest, send: CommandInvoker = invoke): Promise<import("./types").SessionUpdate> {
  try {
    await send("subscribe_session", { request });
    return await send("load_session_details", { request: { kind: "group", request } });
  } catch (error) {
    if (!unknownDelivery(error)) throw error;
    return loadLegacyUpdate(request, send);
  }
}
export async function loadToolDetails(request: LoadEventDetailRequest, send: CommandInvoker = invoke): Promise<EventDetail> {
  try { return await send("load_session_details", { request: { kind: "tool", request } }); }
  catch (error) { if (!unknownDelivery(error)) throw error; return loadEventDetail(request, send); }
}
export async function inspectSessionEvent(request: LoadEventDetailRequest, send: CommandInvoker = invoke): Promise<EventDetail> {
  try { return await send("inspect_session_event", { request }); }
  catch (error) { if (!unknownDelivery(error)) throw error; return loadEventDetail(request, send); }
}
export async function renewSessionSubscriptions(ids: string[], send: CommandInvoker = invoke): Promise<SessionSubscriptionRenewalResult> {
  try {
    const result = await send<SessionSubscriptionRenewal | null | undefined>("renew_session_subscriptions", { ids });
    // Older modern hosts return no acknowledgement. Local lease expiry still
    // forces a baseline after a long pause instead of trusting this fallback.
    return { renewed_ids: result?.renewed_ids ?? ids, missing_ids: result?.missing_ids ?? [], updates: [] };
  }
  catch (error) {
    if (!unknownDelivery(error)) throw error;
    const subscriptions = legacySubscriptionsFor(send);
    const known = ids.filter((id) => subscriptions.has(id));
    const updates = await Promise.all(known.map(async (id) => {
      const update = await send<import("./types").SessionUpdate>("load_session_updates", { request: subscriptions.get(id)! });
      const request = subscriptions.get(id);
      if (request) subscriptions.set(id, { ...request, cursor: update.revision });
      return update;
    }));
    return { renewed_ids: known, missing_ids: ids.filter((id) => !subscriptions.has(id)), updates };
  }
}

export function listenForSubscriptionLoss(handler: (loss: SessionSubscriptionLoss) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<SessionSubscriptionLoss>("session-subscriptions-lost", (event) => handler(event.payload));
}

export function listenForSessionUpdates(handler: (update: import("./types").SessionUpdate) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<import("./types").SessionUpdate>("session-updated", (event) => handler(event.payload));
}

export function listenForSessionNotifications(handler: (notification: import("./types").SessionNotification) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<import("./types").SessionNotification>("session-notification", (event) => handler(event.payload));
}

export function subscribeBackgroundUpdates(request: import("./types").BackgroundUpdatesRequest, send: CommandInvoker = invoke): Promise<import("./types").BackgroundUpdatesSubscription> {
  return send<import("./types").BackgroundUpdatesSubscription>("subscribe_background_updates", { request });
}
export function renewBackgroundSubscriptions(ids: string[], send: CommandInvoker = invoke): Promise<void> {
  return send<void>("renew_background_subscriptions", { ids });
}
export function listenForBackgroundUpdates(handler: (update: import("./types").BackgroundSessionUpdate) => void, subscribe: EventSubscriber = listen): Promise<UnlistenFn> {
  return subscribe<import("./types").BackgroundSessionUpdate>("background-session-updated", (event) => handler(event.payload));
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
export function removeExternalLocalHost(): Promise<import("./types").LocalHostStatus> {
  return localHostCommand("local_host_remove_external");
}
export function getLocalHostPairing(): Promise<import("./types").LocalHostPairing> {
  return localHostCommand("local_host_pairing");
}
export async function listenForLocalHostStatus(handler: (status: import("./types").LocalHostStatus) => void): Promise<UnlistenFn> {
  if (!isDesktop()) throw new Error("Host setup is available in the desktop app.");
  return (await import("@tauri-apps/api/event")).listen<import("./types").LocalHostStatus>("local-host-status", (event) => handler(event.payload));
}

export function getSessionQuestionStatus(request: import("./types").SessionQuestionStatusRequest, send: CommandInvoker = invoke): Promise<import("./types").SessionQuestionStatus> {
  return send("get_session_question_status", { request });
}
export function submitSessionQuestionAnswers(request: import("./types").SubmitSessionQuestionAnswersRequest, send: CommandInvoker = invoke): Promise<SubmitSessionInputResponse> {
  return send("submit_session_question_answers", { request });
}

export function loadSessionImage(request: import("./types").LoadSessionImageRequest, send: CommandInvoker = invoke): Promise<import("./types").LoadSessionImageResponse> {
  return send("load_session_image", { request });
}
export function uploadImageChunk(request: import("./types").UploadImageChunkRequest, send: CommandInvoker = invoke): Promise<import("./types").UploadImageChunkResponse> {
  return send("upload_image_chunk", { request });
}
export function releaseImageUpload(request: import("./types").ReleaseImageUploadRequest,
  send: (command: string, payload?: Record<string, unknown>) => Promise<unknown> = invoke): Promise<void> {
  return send("release_image_upload", { request }).then(() => {});
}
