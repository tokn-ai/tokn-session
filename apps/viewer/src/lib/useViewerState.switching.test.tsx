import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { listen as nativeListen } from "@tauri-apps/api/event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { saveReadingPosition } from "./readingPosition";
import { selectMachine, type ViewerClient } from "./transport";
import type { EventSummary, ListSessionsResponse, SessionSummary, SessionUpdate, SessionUpdatesRequest } from "./types";
import { useViewerState } from "./useViewerState";

// Keep the real reader API and transport: this exercises the local IPC / remote
// boundary rather than replacing the very routing behavior under test.
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
const nativeInvoke = vi.fn();

beforeEach(() => {
  selectMachine(); localStorage.clear();
  vi.stubGlobal("__TAURI_INTERNALS__", { invoke: nativeInvoke });
  nativeInvoke.mockReset();
  vi.mocked(nativeListen).mockReset().mockResolvedValue(() => {});
});
afterEach(() => { cleanup(); selectMachine(); vi.unstubAllGlobals(); });

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, failed) => { resolve = done; reject = failed; });
  return { promise, resolve, reject };
}

function catalog(session_key: string): ListSessionsResponse {
  const session: SessionSummary = {
    session_key, session_id: session_key, parent_session_id: null, is_subagent: false,
    provider: "codex", title: session_key, preview: null, project: "repo", cwd: "/repo",
    updated_at_ms: 1, timestamp: null, agent_path: null, agent_nickname: null, agent_role: null,
    child_count: 0, message_count: null, event_count: 1, history_status: "complete", has_unread: false,
  };
  return { sessions: [session], next_cursor: null, source_errors: [], pending_providers: [] };
}

function remoteMachine() {
  const invoke = vi.fn(async (command: string) => {
    if (command === "list_sessions") return catalog("remote-session");
    if (command === "update_session_view") return;
    return new Promise(() => {});
  });
  const client: ViewerClient = {
    endpoint: "https://hub.example/machines/550e8400-e29b-41d4-a716-446655440000",
    invoke: <T,>(command: string) => invoke(command) as Promise<T>,
    listen: async () => () => {}, setStateListener: () => {}, onClose: () => () => {},
    release: async () => {}, close: vi.fn(),
  };
  return { client, invoke };
}

function snapshot(request: SessionUpdatesRequest, revision = "1"): SessionUpdate {
  const event: EventSummary = {
    event_key: "latest-row", type: "message", provider: "codex", timestamp: null,
    phase: "final", role: "assistant", title: "Assistant", summary: "Latest turn",
    summary_truncated: false, is_hidden: false, is_error: false,
    tool: null, usage: null, reasoning: null,
  };
  return {
    subscription_id: request.subscription_id, session_key: request.session_key, level: request.level,
    generation: "local", base_revision: null, revision, snapshot: true,
    items: [{ item_id: event.event_key, kind: "assistant_message", level: "steps", summary: event }],
    groups: [], removed_items: [], item_order: [event.event_key],
    state: { previous_cursor: "local-earlier", next_cursor: null,
      total_events: 2, history_status: "complete", scope: request.scope },
  };
}

it("stops delayed local pagination when a remote viewer replaces the local reader", async () => {
  const subscription = deferred<{ revision: string }>();
  const pending = deferred<SessionUpdate>();
  let backward_request: SessionUpdatesRequest | undefined;
  nativeInvoke.mockImplementation(async (command: string, payload?: { request: SessionUpdatesRequest }) => {
    if (command === "list_sessions") return catalog("local-session");
    if (command === "subscribe_session") return subscription.promise;
    if (command === "load_session_backward") {
      backward_request = payload!.request;
      return backward_request.history_cursor ? pending.promise : snapshot(backward_request);
    }
    if (command === "update_session_view") return;
    return new Promise(() => {});
  });
  saveReadingPosition("local-session", {
    anchors: [{ slot_key: "older-row", type: "tool_call", timestamp: null, top: 0 }],
    last_event: "older-row", at_end: false,
  });
  const local = renderHook(() => useViewerState());
  await waitFor(() => expect(local.result.current.sessions).toHaveLength(1));
  act(() => local.result.current.selectSession("local-session"));
  await waitFor(() => {
    expect(local.result.current.eventsError).toBeNull();
    expect(nativeInvoke).toHaveBeenCalledWith("subscribe_session", {
      request: expect.objectContaining({ session_key: "local-session", level: "steps",
        cursor: null, scope: { history: "recent_turns", group_keys: [] } }),
    }, undefined);
  });
  expect(nativeInvoke.mock.calls.map(([command]) => command)).not.toContain("load_session_backward");
  await act(async () => subscription.resolve({ revision: "0" }));
  await waitFor(() => expect(local.result.current.eventsLoading).toBe(false));
  expect(local.result.current.events[0]?.event_key).toBe("latest-row");
  // Saved anchors do not backfill history before the first screen. Pagination
  // starts only after this explicit request for older content.
  expect(nativeInvoke.mock.calls.filter(([command]) => command === "load_session_backward")).toHaveLength(1);
  expect(backward_request?.history_cursor).toBeUndefined();
  act(() => local.result.current.loadOlderEvents());
  await waitFor(() => expect(nativeInvoke).toHaveBeenCalledWith("load_session_backward", {
    request: expect.objectContaining({ session_key: "local-session", history_cursor: "local-earlier",
      scope: { history: "retained", group_keys: [] } }),
  }, undefined));
  local.unmount();
  const remote = remoteMachine(); selectMachine(remote.client);
  const next = renderHook(() => useViewerState());
  await waitFor(() => expect(next.result.current.sessions[0]?.session_key).toBe("remote-session"));
  await act(async () => pending.resolve(snapshot(backward_request!, "2")));
  expect(nativeInvoke.mock.calls.filter(([command]) => command === "load_session_backward")).toHaveLength(2);
  expect(nativeInvoke.mock.calls.map(([command]) => command)).not.toContain("load_event_page");
  expect(remote.invoke.mock.calls.map(([command]) => command)).not.toContain("load_session_backward");
  expect(next.result.current.selectedSessionKey).toBeNull();
});

it.each(["subscribe_session", "load_session_backward"])("does not issue a compatibility fallback after a retired local %s rejects", async (stage) => {
  const pending = deferred<never>();
  nativeInvoke.mockImplementation(async (command: string) => {
    if (command === "list_sessions") return catalog("local-session");
    if (command === stage) return pending.promise;
    if (command === "subscribe_session") return { revision: "0" };
    if (command === "update_session_view") return;
    return new Promise(() => {});
  });
  const local = renderHook(() => useViewerState());
  await waitFor(() => expect(local.result.current.sessions).toHaveLength(1));
  act(() => local.result.current.selectSession("local-session"));
  await waitFor(() => {
    expect(local.result.current.eventsError).toBeNull();
    expect(nativeInvoke).toHaveBeenCalledWith(stage, expect.any(Object), undefined);
  });
  local.unmount();
  const remote = remoteMachine(); selectMachine(remote.client);
  renderHook(() => useViewerState());
  await act(async () => pending.reject(new Error("Unknown viewer command")));
  const local_commands = nativeInvoke.mock.calls.map(([command]) => command);
  const remote_commands = remote.invoke.mock.calls.map(([command]) => command);
  expect(local_commands).not.toContain("load_session_updates");
  expect(local_commands).not.toContain("load_event_page");
  expect(remote_commands).not.toContain("load_session_updates");
  expect(remote_commands).not.toContain("load_event_page");
  expect(remote_commands).not.toContain("load_session_backward");
});
