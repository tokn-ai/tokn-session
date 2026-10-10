import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { listen as nativeListen } from "@tauri-apps/api/event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { saveReadingPosition } from "./readingPosition";
import { selectMachine, type ViewerClient } from "./transport";
import type { EventPageResponse, ListSessionsResponse, SessionSummary } from "./types";
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

it("stops delayed local pagination when a remote viewer replaces the local reader", async () => {
  const pending = deferred<EventPageResponse>();
  nativeInvoke.mockImplementation(async (command: string) => {
    if (command === "list_sessions") return catalog("local-session");
    if (command === "load_session_updates") throw new Error("Unknown viewer command");
    if (command === "load_event_page") return pending.promise;
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
    expect(nativeInvoke).toHaveBeenCalledWith("load_event_page", {
      request: { session_key: "local-session", window_mode: "retained", direction: "backward" },
    }, undefined);
  });
  local.unmount();
  const remote = remoteMachine(); selectMachine(remote.client);
  const next = renderHook(() => useViewerState());
  await waitFor(() => expect(next.result.current.sessions[0]?.session_key).toBe("remote-session"));
  await act(async () => pending.resolve({
    events: [], previous_cursor: "local-earlier", next_cursor: null,
    total_events: 1, history_status: "complete",
  }));
  expect(nativeInvoke.mock.calls.filter(([command]) => command === "load_event_page")).toHaveLength(1);
  expect(remote.invoke.mock.calls.map(([command]) => command)).not.toContain("load_event_page");
  expect(next.result.current.selectedSessionKey).toBeNull();
});

it("does not issue a compatibility fallback after a retired local request rejects", async () => {
  const pending = deferred<never>();
  nativeInvoke.mockImplementation(async (command: string) => {
    if (command === "list_sessions") return catalog("local-session");
    if (command === "load_session_updates") return pending.promise;
    if (command === "update_session_view") return;
    return new Promise(() => {});
  });
  const local = renderHook(() => useViewerState());
  await waitFor(() => expect(local.result.current.sessions).toHaveLength(1));
  act(() => local.result.current.selectSession("local-session"));
  await waitFor(() => {
    expect(local.result.current.eventsError).toBeNull();
    expect(nativeInvoke).toHaveBeenCalledWith("load_session_updates", expect.any(Object), undefined);
  });
  local.unmount();
  const remote = remoteMachine(); selectMachine(remote.client);
  renderHook(() => useViewerState());
  await act(async () => pending.reject(new Error("Unknown viewer command")));
  expect(nativeInvoke.mock.calls.map(([command]) => command)).not.toContain("load_event_page");
  expect(remote.invoke.mock.calls.map(([command]) => command)).not.toContain("load_event_page");
});
