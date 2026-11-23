import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { selectMachine } from "./transport";
import type { BackgroundSessionUpdate, BackgroundUpdatesRequest, EventSummary, OpenSessionListRequest, SessionSummary, SessionUpdate, OpenSessionRequest } from "./types";
import { useViewerState } from "./useViewerState";

const { nativeInvoke, handlers, observers } = vi.hoisted(() => ({
  nativeInvoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>(),
  observers: new Map<string, Set<(event: { payload: unknown }) => void>>(),
}));
// Exercise the real command facade and hook while keeping the captured local
// bridge deterministic. Native module loading is covered by switching tests.
vi.mock("./transport", async (original) => ({
  ...await original<typeof import("./transport")>(),
  captureTransport: () => ({
    invoke: <T,>(command: string, payload?: Record<string, unknown>) => nativeInvoke(command, payload) as Promise<T>,
    listen: async <T,>(event: string, handler: (event: { payload: T }) => void) => {
      const observer = handler as (event: { payload: unknown }) => void;
      const listeners = observers.get(event) ?? new Set();
      listeners.add(observer); observers.set(event, listeners);
      handlers.set(event, (value) => { for (const listener of listeners) listener(value); });
      return () => {
        listeners.delete(observer);
        if (!listeners.size) { observers.delete(event); handlers.delete(event); }
      };
    },
    release: async () => {}, on_close: () => () => {},
  }),
}));

beforeEach(() => {
  selectMachine(); localStorage.clear(); handlers.clear(); observers.clear();
  vi.stubGlobal("__TAURI_INTERNALS__", { invoke: nativeInvoke });
  nativeInvoke.mockReset();
});
afterEach(() => { cleanup(); selectMachine(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function row(session_key: string): SessionSummary {
  return { session_key, session_id: session_key, parent_session_id: null, is_subagent: false,
    provider: session_key === "live" ? "codex" : "pi", title: session_key, preview: null, project: "repo", cwd: "/repo",
    updated_at_ms: 1, timestamp: null, agent_path: null, agent_nickname: null, agent_role: null,
    child_count: 0, message_count: null, event_count: 1, history_status: "complete", has_unread: false };
}

function baseline(request: OpenSessionRequest, running: boolean, revision: number): SessionUpdate {
  const summary: EventSummary = { event_key: "reply", type: "message", provider: "codex", timestamp: null,
    phase: "finished", role: "assistant", delivery: "final", title: "Assistant", summary: "Cached reply",
    summary_truncated: false, is_hidden: false, is_error: false, tool: null, usage: null, reasoning: null };
  return { ...request, generation: "delivery", revision: String(revision), base_revision: null, base_state_hash: null, state_hash: String(revision).padStart(64, "0"), snapshot: true,
    items: [{ item_id: "reply", kind: "assistant_message", level: "final", summary }], groups: [],
    removed_items: [], item_order: ["reply"], semantic_order: ["reply"], event_order: [], state: { total_events: 1, previous_cursor: null, next_cursor: null,
      history_status: "complete", is_running: running, source_revision: `source:${revision}` } };
}

function host(initial: boolean, session_keys = ["live", "other"]) {
  let running = initial;
  let revision = 1;
  let blocked: ((request: OpenSessionRequest) => Promise<SessionUpdate>) | undefined;
  const subscriptions = new Map<string, OpenSessionRequest>();
  let listId: string | undefined;
  let listRevision = 1;
  nativeInvoke.mockImplementation(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "open_session_list") {
      const request = payload?.request as OpenSessionListRequest;
      listId = request.subscription_id;
      const sessions = session_keys.map(row).filter((session) => !request.query?.providers || request.query.providers.includes(session.provider))
        .map((session) => ({ ...session, is_running: session.session_key === "live" && running }));
      return { subscription_id: listId, generation: "list", revision: String(listRevision), base_revision: null, rows: sessions, removed_keys: [], order_edits: [], next_cursor: null, source_errors: [], pending_providers: [] };
    }
    if (command === "release_session") {
      const request = payload?.request as OpenSessionRequest;
      subscriptions.delete(request.subscription_id);
      return { subscription_id: request.subscription_id, unsubscribed: true };
    }
    if (command === "open_session") {
      const request = payload?.request as OpenSessionRequest;
      subscriptions.set(request.subscription_id, request);
      if (blocked && request.session_key === "live") return blocked(request);
      return baseline(request, request.session_key === "live" && running, revision);
    }
    if (command === "renew_session_subscriptions") {
      const ids = payload?.ids as string[];
      return { renewed_ids: ids.filter((id) => subscriptions.has(id)), missing_ids: ids.filter((id) => !subscriptions.has(id)) };
    }
    if (command === "subscribe_background_updates") {
      const request = payload?.request as { subscription_id: string; unsubscribe?: boolean };
      return { subscription_id: request.subscription_id, subscribed: !request.unsubscribe, generation: "compact", level: "messages" };
    }
    if (command === "renew_session_lists") return { renewed_ids: payload?.ids, missing_ids: [] };
    if (command === "release_session_list") return true;
    if (command === "renew_background_subscriptions") return { renewed: true };
    if (command === "acknowledge_session_attention") return { changed: false };
    if (command === "update_session_view") return;
    return new Promise(() => {});
  });
  return {
    subscriptions,
    change(value: boolean) {
      running = value; revision++;
      if (listId) {
        const base_revision = String(listRevision++);
        act(() => handlers.get("session-list-updated")?.({ payload: {
          subscription_id: listId, generation: "list", revision: String(listRevision), base_revision,
          rows: [{ ...row("live"), is_running: running }], removed_keys: [], order_edits: [],
          next_cursor: null, source_errors: [], pending_providers: [],
        } }));
      }
    },
    block(load: typeof blocked) { blocked = load; },
    baseline(request: OpenSessionRequest) { return baseline(request, running, revision); },
  };
}

async function open(result: { current: ReturnType<typeof useViewerState> }, key: string) {
  await waitFor(() => expect(result.current.sessions.some((session) => session.session_key === key)).toBe(true));
  act(() => result.current.selectSession(key));
  await waitFor(() => expect(result.current.eventsLoading).toBe(false));
}

async function heartbeat(timers: ReturnType<typeof vi.spyOn>) {
  await act(async () => {
    for (const [callback, delay] of timers.mock.calls) if (delay === 30_000 && typeof callback === "function") callback();
  });
}

it("recovers a same-revision hash conflict with a fresh snapshot", async () => {
  const backend = host(false);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  const [first] = backend.subscriptions.values();
  let recovery: OpenSessionRequest | undefined;
  backend.change(true);
  backend.block(async (request) => {
    recovery = request;
    return backend.baseline(request);
  });
  act(() => handlers.get("session-updated")?.({ payload: {
    ...baseline(first, false, 1), state_hash: "f".repeat(64),
    snapshot: false, base_revision: "0", base_state_hash: "0".repeat(64),
    items: [], groups: [], item_order: null, semantic_order: null, event_order: null,
  } }));
  await waitFor(() => expect(recovery?.subscription_id).toBe(first.subscription_id));
  expect(recovery?.resume).toBeUndefined();
  await waitFor(() => expect(result.current.sessions.find((row) => row.session_key === "live")?.is_running).toBe(true));
});

it("reuses the retained window and applies a live update that beats the reopen response", async () => {
  const backend = host(false);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  const first = [...backend.subscriptions.values()][0];
  const cached = result.current.events;
  await open(result, "other");

  let reopened!: OpenSessionRequest;
  let acknowledge!: (update: SessionUpdate) => void;
  backend.block((request) => {
    reopened = request;
    return new Promise((resolve) => { acknowledge = resolve; });
  });
  act(() => result.current.selectSession("live"));
  expect(result.current.events).toBe(cached);
  await waitFor(() => expect(reopened?.resume).toEqual({
    subscription_id: first.subscription_id, generation: "delivery", revision: "1", state_hash: "1".padStart(64, "0"),
  }));
  expect(reopened.subscription_id).not.toBe(first.subscription_id);
  const snapshot = backend.baseline(reopened);
  const ack = { ...snapshot, snapshot: false, base_revision: "1", base_state_hash: "1".padStart(64, "0"), items: [], item_order: null, semantic_order: null, event_order: null };
  const delta = { ...ack, revision: "2", state_hash: "2".padStart(64, "0"), items: snapshot.items.map((item) => ({
    ...item, summary: { ...item.summary!, summary: "A newer reply" },
  })), state: { ...ack.state, source_revision: "source:2", is_running: true } };
  act(() => handlers.get("session-updated")?.({ payload: delta }));
  expect(result.current.events).toBe(cached);
  await act(async () => acknowledge(ack));
  await waitFor(() => expect(result.current.events[0]?.summary).toBe("A newer reply"));
  expect(result.current.sessions.find((row) => row.session_key === "live")?.is_running).toBe(true);
  expect(nativeInvoke.mock.calls.filter(([command]) => command === "open_session")).toHaveLength(3);
});

it("rebinds an unchanged projection after stream reconnect with one open and no duplicate rows", async () => {
  const backend = host(true);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  const before = result.current.events;
  const first = [...backend.subscriptions.values()][0];
  backend.block(async (request) => {
    expect(request.resume).toEqual({ subscription_id: first.subscription_id, generation: "delivery", revision: "1", state_hash: "1".padStart(64, "0") });
    return { ...backend.baseline(request), snapshot: false, base_revision: "1", base_state_hash: "1".padStart(64, "0"), items: [], item_order: null, semantic_order: null, event_order: null };
  });
  await act(async () => handlers.get("transport-reconnected")?.({ payload: {} }));
  expect(result.current.events).toBe(before);
  expect(result.current.sessions.find((row) => row.session_key === "live")?.is_running).toBe(true);
  expect(nativeInvoke.mock.calls.filter(([command]) => command === "open_session")).toHaveLength(2);
});

it("keeps the replacement lease healthy when an earlier open fails late", async () => {
  const backend = host(true);
  let rejectEarlier: ((error: Error) => void) | undefined;
  backend.block((request) => {
    if (rejectEarlier) return Promise.resolve(backend.baseline(request));
    return new Promise((_resolve, reject) => { rejectEarlier = reject; });
  });
  const { result } = renderHook(() => useViewerState());
  await waitFor(() => expect(result.current.sessions).toHaveLength(2));
  act(() => result.current.selectSession("live"));
  await waitFor(() => expect(rejectEarlier).toBeDefined());
  await act(async () => handlers.get("transport-reconnected")?.({ payload: {} }));
  await act(async () => rejectEarlier!(new Error("Session open was superseded")));
  await waitFor(() => expect(result.current.events[0]?.summary).toBe("Cached reply"));
  act(() => handlers.get("session-notification")?.({ payload: { session_key: "live", is_running: false } }));
  expect(result.current.sessions.find((row) => row.session_key === "live")?.is_running).toBe(true);
  expect(result.current.eventsError).toBeNull();
  expect(nativeInvoke.mock.calls.filter(([command]) => command === "open_session")).toHaveLength(2);
});

it("renews healthy selected interests without body reads", async () => {
  const timers = vi.spyOn(window, "setInterval");
  const backend = host(true);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  const reads = nativeInvoke.mock.calls.filter(([command]) => command === "open_session").length;
  await heartbeat(timers);
  await waitFor(() => expect(nativeInvoke.mock.calls.some(([command]) => command === "renew_session_subscriptions")).toBe(true));
  expect(nativeInvoke.mock.calls.filter(([command]) => command === "open_session")).toHaveLength(reads);
  expect(backend.subscriptions.size).toBe(1);
});

it.each([true, false])("refreshes index activity after compact ownership is lost without a selection (cached=%s)", async (initial) => {
  const backend = host(initial);
  const { result } = renderHook(() => useViewerState());
  await waitFor(() => expect(result.current.sessions.some((session) => session.session_key === "live")).toBe(true));
  await waitFor(() => expect(result.current.backgroundUpdatesStatus).toBeNull());
  const request = nativeInvoke.mock.calls.find(([command]) => command === "subscribe_background_updates")?.[1]?.request as BackgroundUpdatesRequest;
  const update: BackgroundSessionUpdate = { subscription_id: request.subscription_id, generation: "compact", revision: "1", session_key: "live",
    has_unread: false, unread_final_count: 0, is_running: initial, has_running_descendant: false,
    question_attention: { required_count: 0, available_count: 0 }, message_revision: "1",
    latest_assistant_message: { message_id: "reply", delivery: "final", phase: "finished", preview: "Retained reply" } };
  act(() => handlers.get("background-session-updated")?.({ payload: update }));
  backend.change(!initial);
  act(() => handlers.get("background-subscriptions-lost")?.({ payload: { subscription_ids: [request.subscription_id] } }));
  await waitFor(() => expect(result.current.sessions.find((session) => session.session_key === "live")?.is_running).toBe(!initial));
  act(() => handlers.get("background-session-updated")?.({ payload: { ...update, revision: "2" } }));
  expect(result.current.sessions.find((session) => session.session_key === "live")?.is_running).toBe(!initial);
  expect(result.current.sessions.find((session) => session.session_key === "live")?.latest_assistant_preview).toBe("Retained reply");
  expect(result.current.selectedSessionKey).toBeNull();
  expect(nativeInvoke.mock.calls.some(([command]) => command === "open_session")).toBe(false);
});

it.each([true, false])("recovers expired source state while displaying the current index (cached=%s)", async (initial) => {
  const timers = vi.spyOn(window, "setInterval");
  const backend = host(initial);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  backend.change(initial);
  const queued_revision = 2;
  backend.change(!initial); backend.subscriptions.clear();
  let recovery_request: OpenSessionRequest | undefined;
  let complete!: (update: SessionUpdate) => void;
  const pending = new Promise<SessionUpdate>((resolve) => { complete = resolve; });
  backend.block((request) => { recovery_request = request; return pending; });
  await heartbeat(timers);
  await waitFor(() => expect(recovery_request?.level).toBe("steps"));
  await waitFor(() => expect(result.current.sessions.find((session) => session.session_key === "live")?.is_running).toBe(!initial));
  // A frame queued before the interruption can be newer than the retained
  // replica while still trailing the index. Suspended delivery has no authority.
  act(() => handlers.get("session-updated")?.({ payload: baseline(recovery_request!, initial, queued_revision) }));
  expect(result.current.sessions.find((session) => session.session_key === "live")?.is_running).toBe(!initial);
  await act(async () => { complete(backend.baseline(recovery_request!)); await pending; });
  const notification = { session_key: "live", has_unread: false, unread_final_count: 0, is_running: initial,
    has_running_descendant: false, question_attention: { required_count: 0, available_count: 0 } };
  act(() => handlers.get("session-notification")?.({ payload: notification }));
  expect(result.current.sessions.find((session) => session.session_key === "live")?.is_running).toBe(!initial);
});

it("recovers a background reset without disturbing the selected conversation", async () => {
  const backend = host(true);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live"); await open(result, "other");
  await waitFor(() => expect([...backend.subscriptions.values()].some((request) => request.session_key === "live")).toBe(false));
  const selected_events = result.current.events;
  backend.change(false);
  expect(handlers.has("relay-changed")).toBe(true);
  act(() => handlers.get("relay-changed")?.({ payload: { session_key: "live", reset: true } }));
  expect(nativeInvoke.mock.calls.filter(([command, payload]) => command === "open_session"
    && (payload?.request as OpenSessionRequest).session_key === "live")).toHaveLength(1);
  await waitFor(() => expect(result.current.sessions.find((session) => session.session_key === "live")?.is_running).toBe(false));
  expect(result.current.selectedSessionKey).toBe("other");
  expect(result.current.events).toBe(selected_events);
});

it("keeps healthy selected interests when retired or compact subscriptions reconnect", async () => {
  const backend = host(true);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  const subscription_ids = [...backend.subscriptions].filter(([, request]) => request.session_key === "live").map(([id]) => id);
  await open(result, "other");
  const selected_reads = () => nativeInvoke.mock.calls.filter(([command, payload]) => command === "open_session"
    && (payload?.request as OpenSessionRequest).session_key === "other").length;
  const reads = selected_reads();
  const selected_events = result.current.events;
  act(() => {
    handlers.get("session-subscriptions-lost")?.({ payload: { subscription_ids } });
    handlers.get("transport-reconnected")?.({ payload: { subscription_ids } });
    handlers.get("transport-reconnected")?.({ payload: { background_subscription_ids: ["compact"] } });
  });
  expect(selected_reads()).toBe(reads);
  expect(result.current.events).toBe(selected_events);
});

it("releases old live interests while retaining recent display pages", async () => {
  const session_keys = ["live", ...Array.from({ length: 11 }, (_, index) => `other-${index}`)];
  const backend = host(false, session_keys);
  const { result } = renderHook(() => useViewerState());
  for (const key of session_keys) await open(result, key);
  await waitFor(() => expect(backend.subscriptions.size).toBe(1));
  expect([...backend.subscriptions.values()].some((request) => request.session_key === "live")).toBe(false);
  expect(nativeInvoke.mock.calls.some(([command, payload]) => command === "release_session"
    && (payload?.request as OpenSessionRequest).session_key === "live")).toBe(true);
  expect(result.current.selectedSessionKey).toBe(session_keys[session_keys.length - 1]);
});

it("retires an old recovery identity when its cold read completes after selection changes", async () => {
  const timers = vi.spyOn(window, "setInterval");
  const backend = host(true);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  backend.change(false); backend.subscriptions.clear();
  let recovery_request: OpenSessionRequest | undefined;
  let complete!: (update: SessionUpdate) => void;
  const pending = new Promise<SessionUpdate>((resolve) => { complete = resolve; });
  backend.block((request) => {
    recovery_request = request;
    return pending.then((update) => {
      // Native cold reads register under the publication lock after warming.
      backend.subscriptions.set(request.subscription_id, request);
      return update;
    });
  });
  await heartbeat(timers);
  await waitFor(() => expect(recovery_request).toBeDefined());
  await open(result, "other");
  const selected_events = result.current.events;
  await act(async () => { complete(backend.baseline(recovery_request!)); await pending; });
  await waitFor(() => expect(backend.subscriptions.has(recovery_request!.subscription_id)).toBe(false));
  expect(result.current.events).toBe(selected_events);
  expect(result.current.selectedSessionKey).toBe("other");
});
