import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { selectMachine } from "./transport";
import type { EventSummary, ListSessionsRequest, SessionSummary, SessionUpdate, SessionUpdatesRequest } from "./types";
import { useViewerState } from "./useViewerState";

const { nativeInvoke, handlers } = vi.hoisted(() => ({
  nativeInvoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>(),
}));
// Exercise the real command facade and hook while keeping the captured local
// bridge deterministic. Native module loading is covered by switching tests.
vi.mock("./transport", async (original) => ({
  ...await original<typeof import("./transport")>(),
  captureTransport: () => ({
    invoke: <T,>(command: string, payload?: Record<string, unknown>) => nativeInvoke(command, payload) as Promise<T>,
    listen: async <T,>(event: string, handler: (event: { payload: T }) => void) => {
      const observer = handler as (event: { payload: unknown }) => void;
      handlers.set(event, observer);
      return () => { if (handlers.get(event) === observer) handlers.delete(event); };
    },
    release: async () => {}, on_close: () => () => {},
  }),
}));

beforeEach(() => {
  selectMachine(); localStorage.clear(); handlers.clear();
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

function baseline(request: SessionUpdatesRequest, running: boolean, revision: number): SessionUpdate {
  const summary: EventSummary = { event_key: "reply", type: "message", provider: "codex", timestamp: null,
    phase: "finished", role: "assistant", delivery: "final", title: "Assistant", summary: "Cached reply",
    summary_truncated: false, is_hidden: false, is_error: false, tool: null, usage: null, reasoning: null };
  return { ...request, generation: "delivery", revision: String(revision), base_revision: null, snapshot: true,
    items: [{ item_id: "reply", kind: "assistant_message", level: "final", summary }], groups: [],
    removed_items: [], item_order: ["reply"], state: { total_events: 1, previous_cursor: null, next_cursor: null,
      history_status: "complete", is_running: running, source_revision: `source:${revision}` } };
}

function host(initial: boolean, session_keys = ["live", "other"]) {
  let running = initial;
  let revision = 1;
  let blocked: ((request: SessionUpdatesRequest) => Promise<SessionUpdate>) | undefined;
  const subscriptions = new Map<string, SessionUpdatesRequest>();
  nativeInvoke.mockImplementation(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "list_sessions") {
      const request = payload?.request as ListSessionsRequest;
      const sessions = session_keys.map(row).filter((session) => !request.query?.providers || request.query.providers.includes(session.provider))
        .map((session) => ({ ...session, is_running: session.session_key === "live" && running }));
      return { sessions, next_cursor: null, source_errors: [], pending_providers: [] };
    }
    if (command === "subscribe_session") {
      const request = payload?.request as SessionUpdatesRequest;
      if (request.unsubscribe) subscriptions.delete(request.subscription_id);
      else subscriptions.set(request.subscription_id, request);
      return { revision: "0" };
    }
    if (command === "load_session_backward") {
      const request = payload?.request as SessionUpdatesRequest;
      if (blocked && request.session_key === "live") return blocked(request);
      return baseline(request, request.session_key === "live" && running, revision);
    }
    if (command === "renew_session_subscriptions") {
      const ids = payload?.ids as string[];
      return { renewed_ids: ids.filter((id) => subscriptions.has(id)), missing_ids: ids.filter((id) => !subscriptions.has(id)) };
    }
    if (command === "acknowledge_session_attention") return { changed: false };
    if (command === "update_session_view") return;
    return new Promise(() => {});
  });
  return {
    subscriptions,
    change(value: boolean) { running = value; revision++; },
    block(load: typeof blocked) { blocked = load; },
    baseline(request: SessionUpdatesRequest) { return baseline(request, running, revision); },
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

it("renews retained background interests without a selected conversation or body reads", async () => {
  const timers = vi.spyOn(window, "setInterval");
  const backend = host(true);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  act(() => result.current.toggleProvider("codex"));
  await waitFor(() => expect(result.current.selectedSessionKey).toBeNull());
  await waitFor(() => expect([...backend.subscriptions.values()].some((request) => request.level === "final")).toBe(true));
  const reads = nativeInvoke.mock.calls.filter(([command]) => command === "load_session_backward").length;
  await heartbeat(timers);
  await waitFor(() => expect(nativeInvoke.mock.calls.some(([command]) => command === "renew_session_subscriptions")).toBe(true));
  expect(nativeInvoke.mock.calls.filter(([command]) => command === "load_session_backward")).toHaveLength(reads);
});

it.each([true, false])("recovers expired background state while displaying the current index (cached=%s)", async (initial) => {
  const timers = vi.spyOn(window, "setInterval");
  const backend = host(initial);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live");
  act(() => result.current.toggleProvider("codex"));
  await waitFor(() => expect(result.current.selectedSessionKey).toBeNull());
  await waitFor(() => expect([...backend.subscriptions.values()].some((request) => request.level === "final")).toBe(true));
  backend.change(initial);
  const queued_revision = 2;
  backend.change(!initial); backend.subscriptions.clear();
  let recovery_request: SessionUpdatesRequest | undefined;
  let complete!: (update: SessionUpdate) => void;
  const pending = new Promise<SessionUpdate>((resolve) => { complete = resolve; });
  backend.block((request) => { recovery_request = request; return pending; });
  await heartbeat(timers);
  await waitFor(() => expect(recovery_request?.level).toBe("final"));
  act(() => result.current.toggleProvider("codex"));
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
  await waitFor(() => expect([...backend.subscriptions.values()].some((request) => request.session_key === "live" && request.level === "final")).toBe(true));
  const selected_events = result.current.events;
  backend.change(false);
  expect(handlers.has("relay-changed")).toBe(true);
  act(() => handlers.get("relay-changed")?.({ payload: { session_key: "live", reset: true } }));
  await waitFor(() => expect(nativeInvoke.mock.calls.filter(([command, payload]) => command === "load_session_backward"
    && (payload?.request as SessionUpdatesRequest).session_key === "live")).toHaveLength(3));
  await waitFor(() => expect(result.current.sessions.find((session) => session.session_key === "live")?.is_running).toBe(false));
  expect(result.current.selectedSessionKey).toBe("other");
  expect(result.current.events).toBe(selected_events);
});

it("keeps healthy selected interests when one background lease reconnects", async () => {
  const backend = host(true);
  const { result } = renderHook(() => useViewerState());
  await open(result, "live"); await open(result, "other");
  await waitFor(() => expect([...backend.subscriptions.values()].some((request) => request.session_key === "live" && request.level === "final")).toBe(true));
  const subscription_ids = [...backend.subscriptions].filter(([, request]) => request.session_key === "live").map(([id]) => id);
  const selected_reads = () => nativeInvoke.mock.calls.filter(([command, payload]) => command === "load_session_backward"
    && (payload?.request as SessionUpdatesRequest).session_key === "other").length;
  const reads = selected_reads();
  const selected_events = result.current.events;
  backend.change(false);
  act(() => {
    handlers.get("session-subscriptions-lost")?.({ payload: { subscription_ids } });
    handlers.get("transport-reconnected")?.({ payload: { subscription_ids } });
  });
  await waitFor(() => expect(result.current.sessions.find((session) => session.session_key === "live")?.is_running).toBe(false));
  expect(selected_reads()).toBe(reads);
  expect(result.current.events).toBe(selected_events);
});

it("unsubscribes evicted background sessions before they can exhaust live capacity", async () => {
  const session_keys = ["live", ...Array.from({ length: 11 }, (_, index) => `other-${index}`)];
  const backend = host(false, session_keys);
  const { result } = renderHook(() => useViewerState());
  for (const key of session_keys) await open(result, key);
  await waitFor(() => expect(backend.subscriptions.size).toBe(8));
  expect([...backend.subscriptions.values()].some((request) => request.session_key === "live")).toBe(false);
  expect(nativeInvoke.mock.calls.some(([command, payload]) => command === "subscribe_session"
    && (payload?.request as SessionUpdatesRequest).session_key === "live"
    && (payload?.request as SessionUpdatesRequest).level === "final"
    && (payload?.request as SessionUpdatesRequest).unsubscribe)).toBe(true);
  expect(result.current.selectedSessionKey).toBe(session_keys[session_keys.length - 1]);
});
