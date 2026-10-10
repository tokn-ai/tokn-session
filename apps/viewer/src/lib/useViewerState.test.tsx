import { act, cleanup, fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";
import { useLayoutEffect } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readingEventKey, saveReadingPosition } from "./readingPosition";
import { eventButtonId } from "./state";
import {
  acknowledgeSessionAttention,
  getSessionIndexProgress,
  getSessionInputStatus,
  listSessionChildren,
  listSessions,
  listenForSessionIndexChanges,
  listenForSessionIndexProgress,
  listenForRelayChanges,
  listenForTransportReconnect,
  listenForSessionUpdates,
  listenForSessionNotifications,
  loadSessionUpdates,
  loadEventDetail,
  loadEventPage,
  loadTrajectoryEventPage,
  retrySessionIndex,
  submitSessionInput,
  updateSessionView,
} from "./tauri";
import type {
  EventDetail,
  EventPageResponse,
  ListSessionsResponse,
  SessionIndexChangedEvent,
  SessionIndexProgress,
  SessionSummary,
  TrajectoryEventPageResponse,
  RelayChange,
} from "./types";
import { useViewerState } from "./useViewerState";
import { ViewerPage } from "../pages/ViewerPage";

vi.mock("../components/RelayConnection", () => ({ RelayConnection: () => null }));

vi.mock("./tauri", () => ({
  updateSessionView: vi.fn(() => Promise.resolve()),
  loadSessionUpdates: vi.fn(() => Promise.reject(new Error("Unknown viewer command"))),
  listenForSessionUpdates: vi.fn(() => Promise.resolve(vi.fn())),
  listenForSessionNotifications: vi.fn(() => Promise.resolve(vi.fn())),
  listenForRelayChanges: vi.fn(() => Promise.resolve(vi.fn())),
  acknowledgeSessionAttention: vi.fn(() => Promise.resolve({ changed: false })),
  getSessionIndexProgress: vi.fn(() => new Promise(() => undefined)),
  getSessionInputStatus: vi.fn(),
  submitSessionInput: vi.fn(),
  listSessionChildren: vi.fn(() => new Promise(() => undefined)),
  listSessions: vi.fn(() => new Promise(() => undefined)),
  listenForSessionIndexChanges: vi.fn(() => Promise.resolve(vi.fn())),
  listenForSessionIndexProgress: vi.fn(() => Promise.resolve(vi.fn())),
  listenForTransportReconnect: vi.fn(() => Promise.resolve(vi.fn())),
  loadEventDetail: vi.fn(() => new Promise(() => undefined)),
  loadEventPage: vi.fn(() => new Promise(() => undefined)),
  loadTrajectoryEventPage: vi.fn(() => new Promise(() => undefined)),
  retrySessionIndex: vi.fn(() => new Promise(() => undefined)),
}));

beforeEach(() => {
  localStorage.clear();
  vi.mocked(loadSessionUpdates).mockReset().mockRejectedValue(new Error("Unknown viewer command"));
  vi.mocked(listenForSessionUpdates).mockReset().mockResolvedValue(vi.fn());
  vi.mocked(listenForSessionNotifications).mockReset().mockResolvedValue(vi.fn());
  vi.mocked(updateSessionView).mockReset().mockResolvedValue(undefined);
  vi.mocked(listenForRelayChanges).mockReset().mockResolvedValue(vi.fn());
  vi.mocked(acknowledgeSessionAttention).mockReset().mockResolvedValue({ changed: false });
  vi.mocked(getSessionIndexProgress).mockReset().mockImplementation(() => new Promise(() => undefined));
  vi.mocked(getSessionInputStatus).mockReset().mockResolvedValue({ available: true, message: "", max_length: 16_384 });
  vi.mocked(submitSessionInput).mockReset().mockImplementation(async (request) => ({
    request_id: request.request_id, status: "accepted", message: "Codex App accepted the message.",
  }));
  vi.mocked(listSessionChildren).mockReset().mockImplementation(() => new Promise(() => undefined));
  vi.mocked(listSessions).mockReset().mockImplementation(() => new Promise(() => undefined));
  vi.mocked(listenForSessionIndexChanges).mockReset().mockResolvedValue(vi.fn());
  vi.mocked(listenForSessionIndexProgress).mockReset().mockResolvedValue(vi.fn());
  vi.mocked(listenForTransportReconnect).mockReset().mockResolvedValue(vi.fn());
  vi.mocked(loadEventPage).mockReset().mockImplementation(() => new Promise(() => undefined));
  vi.mocked(loadEventDetail).mockReset().mockImplementation(() => new Promise(() => undefined));
  vi.mocked(loadTrajectoryEventPage).mockReset().mockImplementation(
    () => new Promise(() => undefined),
  );
  vi.mocked(retrySessionIndex).mockReset().mockImplementation(() => new Promise(() => undefined));
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

async function selectListedSession(
  result: { current: { sessions: SessionSummary[]; selectSession: (sessionKey: string) => void } },
  sessionKey: string,
) {
  await waitFor(() => {
    expect(result.current.sessions.some((session) => session.session_key === sessionKey)).toBe(true);
  });
  act(() => result.current.selectSession(sessionKey));
}

function session(sessionKey: string): SessionSummary {
  return {
    session_key: sessionKey,
    session_id: sessionKey,
    parent_session_id: null,
    is_subagent: false,
    provider: "codex",
    title: sessionKey,
    preview: null,
    project: "viewer",
    cwd: "/work/repo",
    updated_at_ms: 1,
    timestamp: "2026-08-31T00:00:00Z",
    agent_path: null,
    agent_nickname: null,
    agent_role: null,
    child_count: 0,
    message_count: null,
    event_count: 1,
    history_status: "complete",
    has_unread: false,
  };
}

function indexProgress(overrides: Partial<SessionIndexProgress> = {}): SessionIndexProgress {
  return {
    revision: "1",
    is_refreshing: false,
    activity: "idle",
    catalog: {
      scope: "full",
      active_provider: null,
      processed_providers: 6,
      total_providers: 6,
      pending_providers: [],
      error_providers: [],
    },
    body: {
      active_provider: null,
      pending_jobs: 0,
      failed_jobs: 0,
      completed_in_run: 0,
      stale_in_run: 0,
      batch_size: 1,
      providers: [],
    },
    worker_error: null,
    retry_at_ms: null,
    ...overrides,
  };
}

function toolEventPage(): EventPageResponse {
  return {
    events: [{
      event_key: "event.v1.1",
      type: "tool_call",
      provider: "codex",
      timestamp: "2026-08-31T00:00:00Z",
      phase: "finished",
      role: null,
      title: "exec_command",
      summary: "shell exit 0 cargo test",
      summary_truncated: false,
      is_hidden: false,
      is_error: false,
      tool: {
        kind: "shell",
        tool_name: "exec_command",
        tool_call_id: "call-1",
        command: "cargo test",
        cwd: "/work/repo",
        path: null,
        query: null,
        url: null,
        task_title: null,
        exit_code: 0,
        bytes: null,
        added: null,
        removed: null,
      },
      usage: null,
      reasoning: null,
    }],
    next_cursor: null,
    previous_cursor: null,
    total_events: 1,
    history_status: "complete",
  };
}

function pendingToolEventPage(): EventPageResponse {
  const page = toolEventPage();
  const event = page.events[0]!;
  return {
    ...page,
    events: [{
      ...event,
      phase: "started",
      summary: "shell running cargo test",
      tool: {
        ...event.tool!,
        exit_code: null,
      },
    }],
  };
}

function toolDetail(text: string): EventDetail {
  return {
    event_key: "event.v1.1",
    event: { type: "tool_call" },
    native: null,
    is_hidden: false,
    tool_output: {
      sections: [{ label: "stdout", text, format: "text" }],
      truncated: false,
      original_size_bytes: text.length,
      source_event_key: "event.v1.1",
    },
  };
}

function reasoningEventPage(overrides: Partial<EventPageResponse["events"][number]> = {}): EventPageResponse {
  return {
    events: [{
      event_key: "event.v1.reasoning",
      type: "reasoning",
      provider: "codex",
      timestamp: "2026-08-31T00:00:00Z",
      phase: "finished",
      role: null,
      title: "Reasoning",
      summary: "Inspect the source",
      summary_truncated: false,
      is_hidden: false,
      is_error: false,
      tool: null,
      usage: null,
      reasoning: {
        preview: "Inspect the source",
        has_summary: true,
        has_text: false,
        has_encrypted_content: false,
        is_redacted: false,
      },
      ...overrides,
    }],
    next_cursor: null,
    previous_cursor: null,
    total_events: 1,
    history_status: "complete",
  };
}

function trajectoryEventPage(): EventPageResponse {
  return {
    events: [{
      event_key: "trajectory.v1.turn-1",
      type: "trajectory",
      provider: "codex",
      timestamp: "2026-08-31T01:00:00Z",
      phase: null,
      role: null,
      title: "Turn trajectory",
      summary: "Whole turn",
      summary_truncated: false,
      is_hidden: false,
      is_error: false,
      trajectory: {
        event_count: 2,
        tool_count: 1,
        reasoning_count: 0,
        agent_activity_count: 0,
        error_count: 0,
        unknown_count: 0,
        started_at: "2026-08-31T00:00:00Z",
        ended_at: "2026-08-31T01:00:00Z",
        duration_ms: "3600000",
      },
      tool: null,
      usage: null,
      reasoning: null,
    }],
    next_cursor: null,
    previous_cursor: null,
    total_events: 1,
    history_status: "complete",
  };
}

function trajectoryChildPage(): TrajectoryEventPageResponse {
  const child = toolEventPage().events[0]!;
  return {
    events: [{
      ...child,
      event_key: "event.v1.trajectory-tool",
      timestamp: "2026-08-31T00:10:00Z",
    }],
    next_cursor: null,
    previous_cursor: null,
    total_events: 1,
  };
}

function workingTrajectoryPage(event_key = "trajectory.v1.turn-1"): EventPageResponse {
  const page = trajectoryEventPage();
  page.events[0].event_key = event_key;
  page.events[0].trajectory!.status = "working";
  return page;
}

function trajectoryChildren(
  prefix: string,
  start: number,
  count: number,
  previous_cursor: string | null = null,
  total_events = start + count,
): TrajectoryEventPageResponse {
  const child = trajectoryChildPage().events[0];
  return {
    events: Array.from({ length: count }, (_, index) => ({
      ...child,
      event_key: `event.v1.${prefix}-${start + index}`,
    })),
    next_cursor: null,
    previous_cursor,
    total_events,
  };
}

function ViewerPageCommitProbe() {
  const state = useViewerState();
  return (
    <>
      <button
        disabled={state.sessions.length === 0}
        onClick={() => state.selectSession(state.sessions[0]!.session_key)}
        type="button"
      >
        Select indexed session
      </button>
      <output
        data-owner={state.eventsOwnerKey ?? ""}
        data-session={state.selectedSessionKey ?? ""}
        data-testid="viewer-page-commit-state"
      >
        {state.initialPageLoaded ? "ready" : "loading"}
      </output>
    </>
  );
}

describe("sidebar view preference", () => {
  it("requests project order and preserves a selection outside the replacement page", async () => {
    vi.mocked(listSessions)
      .mockResolvedValueOnce({ sessions: [session("a")], next_cursor: null, source_errors: [], pending_providers: [] })
      .mockResolvedValueOnce({ sessions: [session("b")], next_cursor: null, source_errors: [], pending_providers: [] });
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "a");
    act(() => result.current.setSessionOrder("project"));
    await waitFor(() => expect(result.current.sessions[0]?.session_key).toBe("b"));
    expect(result.current.selectedSessionKey).toBe("a");
    expect(result.current.selectedSession?.session_key).toBe("a");
    const calls = vi.mocked(listSessions).mock.calls;
    expect(calls[calls.length - 1][0].query.order).toBe("project");
    expect(localStorage.getItem("tokn.viewer.sidebar-order")).toBe("project");
  });
});

describe("retained session turns", () => {
  it("loads earlier turns as one retained window and restores them on returning to a cached session", async () => {
    const sessions = [session("a"), session("b")];
    vi.mocked(listSessions).mockResolvedValue({ sessions, next_cursor: null, source_errors: [], pending_providers: [] });
    const row = toolEventPage().events[0];
    const recent = { ...toolEventPage(), events: [{ ...row, event_key: "recent", summary: "old output" }], previous_cursor: "earlier" };
    const expanded = { ...recent, events: [{ ...row, event_key: "older" }, { ...row, event_key: "recent", summary: "updated output" }], previous_cursor: null, total_events: 2 };
    let retained: EventPageResponse = recent;
    vi.mocked(loadEventPage).mockImplementation(async (request) => {
      if (request.session_key === "b") return toolEventPage();
      if (request.window_mode === "earlier") retained = expanded;
      return retained;
    });
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "a");
    await waitFor(() => expect(result.current.events).toEqual(recent.events));
    act(() => result.current.loadOlderEvents());
    await waitFor(() => expect(result.current.events).toEqual(expanded.events));
    expect(loadEventPage).toHaveBeenLastCalledWith({ session_key: "a", cursor: "earlier", window_mode: "earlier", direction: "backward" });
    await selectListedSession(result, "b");
    await waitFor(() => expect(result.current.events).toEqual(toolEventPage().events));
    await selectListedSession(result, "a");
    await waitFor(() => expect(result.current.events).toEqual(expanded.events));
    expect(loadEventPage).toHaveBeenLastCalledWith({ session_key: "a", window_mode: "retained", direction: "backward" });
    expect(result.current.olderCursor).toBeNull();
  });

  it("waits for an earlier-window load before refreshing new live activity", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("a")], next_cursor: null, source_errors: [], pending_providers: [] });
    const initial = { ...toolEventPage(), previous_cursor: "earlier" };
    const expanded = { ...initial, events: [{ ...initial.events[0], event_key: "older" }, ...initial.events], previous_cursor: null };
    const refreshed = { ...expanded, events: [...expanded.events, { ...initial.events[0], event_key: "latest" }] };
    const earlier = deferred<EventPageResponse>();
    vi.mocked(loadEventPage).mockResolvedValueOnce(initial).mockReturnValueOnce(earlier.promise).mockResolvedValueOnce(refreshed);
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "a");
    await waitFor(() => expect(result.current.olderCursor).toBe("earlier"));
    act(() => result.current.loadOlderEvents());
    act(() => emit?.({ session_key: "a", reset: false }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    await act(async () => { earlier.resolve(expanded); });
    await waitFor(() => expect(result.current.events).toEqual(refreshed.events));
    expect(loadEventPage).toHaveBeenLastCalledWith({ session_key: "a", window_mode: "retained", direction: "backward" });
    expect(result.current.olderLoading).toBe(false);
  });

  it("ignores an earlier-window result after the selected session changes", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("a"), session("b")], next_cursor: null, source_errors: [], pending_providers: [] });
    const initial = { ...toolEventPage(), previous_cursor: "earlier" };
    const earlier = deferred<EventPageResponse>();
    vi.mocked(loadEventPage).mockResolvedValueOnce(initial).mockReturnValueOnce(earlier.promise).mockResolvedValueOnce(reasoningEventPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "a");
    await waitFor(() => expect(result.current.olderCursor).toBe("earlier"));
    act(() => result.current.loadOlderEvents());
    await selectListedSession(result, "b");
    await waitFor(() => expect(result.current.events).toEqual(reasoningEventPage().events));
    await act(async () => { earlier.resolve(initial); });
    expect(result.current.events).toEqual(reasoningEventPage().events);
    expect(result.current.eventsOwnerKey).toBe("b");
    expect(result.current.olderLoading).toBe(false);
  });
});

describe("useViewerState Relay updates", () => {
  it("keeps the last good page visible while a followed session fails and clears its warning on recovery", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    let followError: string | null = null;
    vi.mocked(loadEventPage).mockImplementation(async () => ({ ...toolEventPage(), follow_error: followError }));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toEqual(toolEventPage().events));
    const lastGood = result.current.events;

    followError = "invalid UTF-8 in session source";
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(result.current.followError).toBe(followError));
    expect(result.current.events).toEqual(lastGood);
    expect(result.current.eventsError).toBeNull();

    followError = null;
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(result.current.followError).toBeNull());
    expect(result.current.events).toEqual(lastGood);
  });

  it("restores disclosure by stable slot while fetching details with the new generation key", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = trajectoryEventPage();
    original.events[0] = { ...original.events[0], event_key: "window.v1.old.trajectory.v1.1", slot_key: "trajectory.v1.1" };
    vi.mocked(loadEventPage).mockResolvedValueOnce(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toEqual(original.events));
    act(() => result.current.toggleEventExpanded(original.events[0].event_key));
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(original.events[0].event_key)?.has_loaded).toBe(true));
    const replacement = { ...original, events: [{ ...original.events[0], event_key: "window.v1.new.trajectory.v1.1" }] };
    vi.mocked(loadEventPage).mockResolvedValueOnce(replacement);
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(result.current.expandedEventKey).toBe(replacement.events[0].event_key));
    expect(loadTrajectoryEventPage).toHaveBeenLastCalledWith(expect.objectContaining({ trajectory_key: replacement.events[0].event_key }));
    expect(result.current.trajectoryPages.get("live")?.has(original.events[0].event_key)).toBe(false);
  });

  it("replaces a reset timeline and its loaded child window together without collapsing to the latest 40", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const oldKey = original.events[0].event_key;
    const oldLatest = trajectoryChildren("old", 40, 40, "old-earlier");
    const oldEarlier = trajectoryChildren("old", 0, 40, null, 80);
    const oldRows = [...oldEarlier.events, ...oldLatest.events];
    const oldDetail = { ...toolDetail("last good output"), event_key: oldRows[79].event_key };
    vi.mocked(loadEventPage).mockResolvedValue(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValueOnce(oldLatest).mockResolvedValueOnce(oldEarlier);
    vi.mocked(loadEventDetail).mockResolvedValue(oldDetail);
    const commits: ReturnType<typeof useViewerState>[] = [];
    const { result } = renderHook(() => {
      const state = useViewerState();
      useLayoutEffect(() => { commits.push(state); });
      return state;
    });
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(oldKey)?.events).toHaveLength(80));
    act(() => result.current.toggleTrajectoryEventExpanded(oldKey, oldDetail.event_key));
    await waitFor(() => expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail));
    commits.length = 0;

    const parent = deferred<EventPageResponse>();
    const latest = deferred<TrajectoryEventPageResponse>();
    const earlier = deferred<TrajectoryEventPageResponse>();
    vi.mocked(loadEventPage).mockReturnValueOnce(parent.promise);
    vi.mocked(loadTrajectoryEventPage).mockReturnValueOnce(latest.promise).mockReturnValueOnce(earlier.promise);
    const expectOriginalVisible = () => {
      expect(result.current.events).toEqual(original.events);
      expect(result.current.expandedEventKey).toBe(oldKey);
      expect(result.current.trajectoryPages.get("live")?.get(oldKey)?.events).toEqual(oldRows);
      expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail);
    };
    act(() => emit?.({ session_key: "live", reset: true }));
    expectOriginalVisible();

    const replacement = workingTrajectoryPage("trajectory.v1.replacement");
    const newKey = replacement.events[0].event_key;
    await act(async () => parent.resolve(replacement));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(3));
    expectOriginalVisible();
    expect(loadTrajectoryEventPage).toHaveBeenNthCalledWith(3, {
      session_key: "live", trajectory_key: newKey, direction: "backward", limit: 40,
    });
    const newLatest = trajectoryChildren("fresh", 50, 40, "fresh-earlier");
    await act(async () => latest.resolve(newLatest));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(4));
    expectOriginalVisible();
    expect(loadTrajectoryEventPage).toHaveBeenNthCalledWith(4, {
      session_key: "live", trajectory_key: newKey, direction: "backward", limit: 40, cursor: "fresh-earlier",
    });
    const newEarlier = trajectoryChildren("fresh", 0, 50, null, 90);
    await act(async () => earlier.resolve(newEarlier));
    await waitFor(() => expect(result.current.events).toEqual(replacement.events));
    expect(result.current.expandedEventKey).toBe(newKey);
    expect(result.current.trajectoryPages.get("live")?.get(newKey)?.events).toEqual([...newEarlier.events, ...newLatest.events]);
    expect(result.current.trajectoryPages.get("live")?.has(oldKey)).toBe(false);
    expect(result.current.expandedTrajectoryEventKey).toBeNull();
    expect(result.current.expandedTrajectoryDetail).toBeNull();
    expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(4);
    expect(commits.length).toBeGreaterThan(0);
    for (const state of commits) {
      const committedKey = state.events[0].event_key;
      expect(state.expandedEventKey).toBe(committedKey);
      expect(state.trajectoryPages.get("live")?.get(committedKey)?.events).toHaveLength(committedKey === oldKey ? 80 : 90);
      expect(state.expandedTrajectoryDetail).toEqual(committedKey === oldKey ? oldDetail : null);
    }
  });

  it.each(["parent", "children"] as const)("retains the last good reset presentation after a %s failure and retries the replacement", async (failure) => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const key = original.events[0].event_key;
    const oldChildren = trajectoryChildren("old", 0, 40);
    const oldDetail = { ...toolDetail("last good output"), event_key: oldChildren.events[0].event_key };
    vi.mocked(loadEventPage).mockResolvedValue(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(oldChildren);
    vi.mocked(loadEventDetail).mockResolvedValue(oldDetail);
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));
    act(() => result.current.toggleTrajectoryEventExpanded(key, oldDetail.event_key));
    await waitFor(() => expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail));
    const replacement = workingTrajectoryPage();
    replacement.events[0].summary = "replacement snapshot";
    if (failure === "parent") vi.mocked(loadEventPage).mockRejectedValueOnce(new Error("refresh unavailable"));
    else {
      vi.mocked(loadEventPage).mockResolvedValueOnce(replacement);
      vi.mocked(loadTrajectoryEventPage).mockRejectedValueOnce(new Error("refresh unavailable"));
    }
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(result.current.eventsError).toContain("refresh unavailable"));
    expect(result.current.events).toEqual(original.events);
    expect(result.current.expandedEventKey).toBe(key);
    expect(result.current.trajectoryPages.get("live")?.get(key)?.events).toEqual(oldChildren.events);
    expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail);

    const retryChildren = deferred<TrajectoryEventPageResponse>();
    vi.mocked(loadEventPage).mockResolvedValueOnce(replacement);
    vi.mocked(loadTrajectoryEventPage).mockReturnValueOnce(retryChildren.promise);
    act(() => result.current.retryEvents());
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(3));
    expect(result.current.events).toEqual(original.events);
    expect(result.current.expandedEventKey).toBe(key);
    expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail);
    const freshChildren = trajectoryChildren("fresh", 0, 40);
    await act(async () => retryChildren.resolve(freshChildren));
    await waitFor(() => expect(result.current.events).toEqual(replacement.events));
    expect(result.current.trajectoryPages.get("live")?.get(key)?.events).toEqual(freshChildren.events);
    expect(result.current.expandedTrajectoryEventKey).toBeNull();
    expect(result.current.expandedTrajectoryDetail).toBeNull();
    expect(result.current.eventsError).toBeNull();
  });

  it("keeps reset semantics when jumping to latest supersedes the pending replacement", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const key = original.events[0].event_key;
    const oldLatest = trajectoryChildren("old", 40, 40, "old-earlier");
    const oldEarlier = trajectoryChildren("old", 0, 40, null, 80);
    const oldDetail = { ...toolDetail("old output"), event_key: oldEarlier.events[0].event_key };
    vi.mocked(loadEventPage).mockResolvedValue(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValueOnce(oldLatest).mockResolvedValueOnce(oldEarlier);
    vi.mocked(loadEventDetail).mockResolvedValue(oldDetail);
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.events).toHaveLength(80));
    act(() => {
      result.current.toggleTrajectoryEventExpanded(key, oldDetail.event_key);
      result.current.selectEvent(oldDetail.event_key);
      result.current.setFollowingLive(false);
    });
    await waitFor(() => expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail));
    expect(result.current.detail).toEqual(oldDetail);

    const supersededParent = deferred<EventPageResponse>();
    const replacement = workingTrajectoryPage();
    replacement.events[0].summary = "replacement snapshot";
    const latest = deferred<TrajectoryEventPageResponse>();
    const earlier = deferred<TrajectoryEventPageResponse>();
    vi.mocked(loadEventPage).mockReturnValueOnce(supersededParent.promise).mockResolvedValueOnce(replacement);
    vi.mocked(loadTrajectoryEventPage).mockReturnValueOnce(latest.promise).mockReturnValueOnce(earlier.promise);
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    expect(result.current.pendingLiveActivity).toBe(true);
    act(() => result.current.showLiveActivity());
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(3));
    expect(result.current.events).toEqual(original.events);
    expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail);

    const newLatest = trajectoryChildren("fresh", 40, 40, "fresh-earlier");
    newLatest.events[0].event_key = oldDetail.event_key;
    await act(async () => latest.resolve(newLatest));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(4));
    expect(result.current.events).toEqual(original.events);
    const newEarlier = trajectoryChildren("fresh", 0, 40, null, 80);
    await act(async () => earlier.resolve(newEarlier));
    await waitFor(() => expect(result.current.events).toEqual(replacement.events));
    expect(result.current.trajectoryPages.get("live")?.get(key)?.events).toEqual([...newEarlier.events, ...newLatest.events]);
    expect(result.current.expandedTrajectoryEventKey).toBeNull();
    expect(result.current.expandedTrajectoryDetail).toBeNull();
    expect(result.current.selectedEventKey).toBeNull();
    expect(result.current.detail).toBeNull();
    await act(async () => supersededParent.resolve(original));
    expect(result.current.events).toEqual(replacement.events);
    expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(4);
  });

  it("ignores a staged reset child response after switching sessions", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live"), session("next")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const key = original.events[0].event_key;
    vi.mocked(loadEventPage).mockResolvedValueOnce(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValueOnce(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));
    const replacement = workingTrajectoryPage("trajectory.v1.replacement");
    const children = deferred<TrajectoryEventPageResponse>();
    vi.mocked(loadEventPage).mockResolvedValueOnce(replacement);
    vi.mocked(loadTrajectoryEventPage).mockReturnValueOnce(children.promise);
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(2));
    const next = toolEventPage();
    next.events[0].summary = "next session";
    vi.mocked(loadEventPage).mockResolvedValueOnce(next);
    act(() => result.current.selectSession("next"));
    await waitFor(() => expect(result.current.events).toEqual(next.events));
    await act(async () => children.resolve(trajectoryChildren("fresh", 0, 40)));
    expect(result.current.selectedSessionKey).toBe("next");
    expect(result.current.events).toEqual(next.events);
    expect(result.current.expandedEventKey).toBeNull();
    expect(result.current.trajectoryPages.size).toBe(0);
  });

  it.each([true, false])("preserves an expanded completed turn through a reset while following is %s", async (following) => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = trajectoryEventPage();
    original.events[0].trajectory!.status = "complete";
    original.events.push(workingTrajectoryPage("trajectory.v1.current").events[0]);
    original.total_events = 2;
    const key = original.events[0].event_key;
    const oldChildren = trajectoryChildren("old", 0, 40);
    const oldDetail = { ...toolDetail("old output"), event_key: oldChildren.events[0].event_key };
    vi.mocked(loadEventPage).mockResolvedValueOnce(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValueOnce(trajectoryChildPage()).mockResolvedValueOnce(oldChildren);
    vi.mocked(loadEventDetail).mockResolvedValue(oldDetail);
    const commits: ReturnType<typeof useViewerState>[] = [];
    const { result } = renderHook(() => {
      const state = useViewerState();
      useLayoutEffect(() => { commits.push(state); });
      return state;
    });
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get("trajectory.v1.current")?.has_loaded).toBe(true));
    act(() => result.current.toggleEventExpanded(key));
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));
    act(() => {
      result.current.toggleTrajectoryEventExpanded(key, oldDetail.event_key);
      result.current.selectEvent(oldDetail.event_key);
      result.current.setFollowingLive(following);
    });
    await waitFor(() => expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail));
    await waitFor(() => expect(result.current.detail).toEqual(oldDetail));
    commits.length = 0;

    const replacement = {
      ...original,
      events: [{ ...original.events[0], summary: "refreshed completed turn" }, original.events[1]],
    };
    const children = deferred<TrajectoryEventPageResponse>();
    vi.mocked(loadEventPage).mockResolvedValueOnce(replacement);
    vi.mocked(loadTrajectoryEventPage).mockReturnValueOnce(children.promise);
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(3));
    expect(result.current.events).toEqual(original.events);
    expect(result.current.expandedEventKey).toBe(key);
    expect(result.current.expandedTrajectoryDetail).toEqual(oldDetail);

    const freshChildren = trajectoryChildren("fresh", 0, 40);
    // An ordinal can be reused after reset. Disclosure survives, but the old
    // nested expansion and detail must not transfer to its new occupant.
    freshChildren.events[0].event_key = oldDetail.event_key;
    await act(async () => children.resolve(freshChildren));
    await waitFor(() => expect(result.current.events).toEqual(replacement.events));
    expect(result.current.expandedEventKey).toBe(key);
    expect(result.current.trajectoryPages.get("live")?.get(key)?.events).toEqual(freshChildren.events);
    expect(result.current.expandedTrajectoryEventKey).toBeNull();
    expect(result.current.expandedTrajectoryDetail).toBeNull();
    expect(result.current.selectedEventKey).toBeNull();
    expect(result.current.detail).toBeNull();
    expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(3);
    for (const state of commits) {
      expect(state.expandedEventKey).toBe(key);
      const refreshed = state.events[0].summary === "refreshed completed turn";
      expect(state.trajectoryPages.get("live")?.get(key)?.events).toEqual(refreshed ? freshChildren.events : oldChildren.events);
    }
  });

  it("keeps a manually collapsed turn closed through live resets", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const key = original.events[0].event_key;
    vi.mocked(loadEventPage).mockResolvedValueOnce(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));
    act(() => result.current.toggleEventExpanded(key));

    const replacement = workingTrajectoryPage();
    replacement.events[0].summary = "new progress";
    vi.mocked(loadEventPage).mockResolvedValueOnce(replacement);
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(result.current.events).toEqual(replacement.events));
    expect(result.current.expandedEventKey).toBeNull();
    expect(loadTrajectoryEventPage).toHaveBeenCalledOnce();

    act(() => result.current.showLiveActivity());
    expect(result.current.expandedEventKey).toBe(key);
  });

  it.each([
    ["resolve", "same"], ["reject", "same"],
    ["resolve", "new"], ["reject", "new"],
  ] as const)("honors a manual collapse when staged children %s for a %s turn key", async (settlement, replacementKind) => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const key = original.events[0].event_key;
    vi.mocked(loadEventPage).mockResolvedValueOnce(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValueOnce(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));
    const replacement = workingTrajectoryPage(replacementKind === "same" ? key : "trajectory.v1.replacement");
    replacement.events[0].summary = "new progress";
    const children = deferred<TrajectoryEventPageResponse>();
    vi.mocked(loadEventPage).mockResolvedValueOnce(replacement);
    vi.mocked(loadTrajectoryEventPage).mockReturnValueOnce(children.promise);
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(2));
    expect(result.current.events).toEqual(original.events);
    act(() => result.current.toggleEventExpanded(key));
    expect(result.current.expandedEventKey).toBeNull();
    await act(async () => {
      if (settlement === "resolve") children.resolve(trajectoryChildren("fresh", 0, 40));
      else children.reject(new Error("no longer visible"));
    });
    await waitFor(() => expect(result.current.events).toEqual(replacement.events));
    expect(result.current.expandedEventKey).toBeNull();
    expect(result.current.eventsError).toBeNull();
    expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(2);
  });

  it("does not auto-open replacement work after following stops during its staged child load", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const key = original.events[0].event_key;
    vi.mocked(loadEventPage).mockResolvedValueOnce(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValueOnce(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));

    const replacement = workingTrajectoryPage("trajectory.v1.replacement");
    const children = deferred<TrajectoryEventPageResponse>();
    vi.mocked(loadEventPage).mockResolvedValueOnce(replacement);
    vi.mocked(loadTrajectoryEventPage).mockReturnValueOnce(children.promise);
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(2));
    expect(result.current.events).toEqual(original.events);
    act(() => result.current.setFollowingLive(false));
    await act(async () => children.resolve(trajectoryChildren("fresh", 0, 40)));
    await waitFor(() => expect(result.current.events).toEqual(replacement.events));
    expect(result.current.expandedEventKey).toBeNull();
    expect(result.current.eventsError).toBeNull();
    expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(2);
  });

  it.each(["complete", "absent"] as const)("commits a reset with %s working trajectory without reopening stale work", async (status) => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const key = original.events[0].event_key;
    vi.mocked(loadEventPage).mockResolvedValueOnce(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValueOnce(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));
    const parent = deferred<EventPageResponse>();
    vi.mocked(loadEventPage).mockReturnValueOnce(parent.promise);
    act(() => emit?.({ session_key: "live", reset: true }));
    expect(result.current.expandedEventKey).toBe(key);
    const replacement = status === "complete" ? trajectoryEventPage() : toolEventPage();
    if (status === "complete") replacement.events[0].trajectory!.status = "complete";
    await act(async () => parent.resolve(replacement));
    await waitFor(() => expect(result.current.events).toEqual(replacement.events));
    expect(result.current.expandedEventKey).toBeNull();
    expect(result.current.trajectoryPages.size).toBe(0);
    expect(loadTrajectoryEventPage).toHaveBeenCalledOnce();
  });

  it("opens child outputs independently and preserves both through concurrent loading", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const page = trajectoryEventPage();
    page.events[0].trajectory!.status = "working";
    const key = page.events[0].event_key;
    const children = trajectoryChildPage();
    const first = children.events[0].event_key;
    const second = "event.second";
    children.events.push({...children.events[0], event_key: second});
    children.total_events = 2;
    vi.mocked(loadEventPage).mockResolvedValue(page);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(children);
    const firstResponse = deferred<EventDetail>();
    const secondResponse = deferred<EventDetail>();
    vi.mocked(loadEventDetail).mockImplementation((request) => request.event_key === first ? firstResponse.promise : secondResponse.promise);
    const {result} = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));
    act(() => result.current.toggleTrajectoryEventExpanded(key, first));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledOnce());
    act(() => result.current.toggleTrajectoryEventExpanded(key, second));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledTimes(2));
    await act(async () => firstResponse.resolve({...toolDetail("first output"), event_key: first}));
    await act(async () => secondResponse.resolve({...toolDetail("second output"), event_key: second}));
    await waitFor(() => expect(result.current.expandedActivities.get(first)?.detail?.event_key).toBe(first));
    expect(result.current.expandedActivities.get(second)?.detail?.event_key).toBe(second);
    expect(result.current.expandedActivityKeys).toEqual(new Set([first, second]));
    act(() => result.current.toggleTrajectoryEventExpanded(key, first));
    expect(result.current.expandedActivityKeys).toEqual(new Set([second]));
  });

  it.each(["timeline", "trajectory"])("retains visible %s detail through live refreshes and refresh errors", async (location) => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const isTrajectory = location === "trajectory";
    const timelinePage = isTrajectory ? trajectoryEventPage() : toolEventPage();
    if (isTrajectory) timelinePage.events[0].trajectory!.status = "working";
    const trajectoryKey = timelinePage.events[0].event_key;
    const eventKey = isTrajectory ? trajectoryChildPage().events[0].event_key : trajectoryKey;
    const existing = { ...toolDetail("visible output"), event_key: eventKey };
    vi.mocked(loadEventPage).mockResolvedValue(timelinePage);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    vi.mocked(loadEventDetail).mockResolvedValue(existing);
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    if (isTrajectory) {
      await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(trajectoryKey)?.has_loaded).toBe(true));
      act(() => result.current.toggleTrajectoryEventExpanded(trajectoryKey, eventKey));
    } else {
      act(() => {
        result.current.toggleEventExpanded(eventKey);
        result.current.selectEvent(eventKey);
      });
    }
    const currentDetail = () => isTrajectory ? result.current.expandedTrajectoryDetail : result.current.expandedDetail;
    const currentLoading = () => isTrajectory ? result.current.expandedTrajectoryDetailLoading : result.current.expandedDetailLoading;
    const currentError = () => isTrajectory ? result.current.expandedTrajectoryDetailError : result.current.expandedDetailError;
    await waitFor(() => expect(currentDetail()).toEqual(existing));

    const failed = deferred<EventDetail>();
    vi.mocked(loadEventDetail).mockReturnValue(failed.promise);
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledTimes(2));
    expect(currentDetail()).toEqual(existing);
    expect(currentLoading()).toBe(true);
    if (!isTrajectory) expect(result.current.detail).toEqual(existing);
    await act(async () => failed.reject(new Error("temporarily unavailable")));
    await waitFor(() => expect(currentError()).toBe("temporarily unavailable"));
    expect(currentDetail()).toEqual(existing);
    if (!isTrajectory) expect(result.current.detail).toEqual(existing);

    const fresh = deferred<EventDetail>();
    vi.mocked(loadEventDetail).mockReturnValue(fresh.promise);
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledTimes(3));
    expect(currentDetail()).toEqual(existing);
    const replacement = { ...toolDetail("latest output"), event_key: eventKey };
    await act(async () => fresh.resolve(replacement));
    await waitFor(() => expect(currentDetail()).toEqual(replacement));
    expect(currentLoading()).toBe(false);
    expect(currentError()).toBeNull();
    if (!isTrajectory) expect(result.current.detail).toEqual(replacement);
  });

  it("drops retained detail when the owner or snapshot generation changes", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const page = toolEventPage();
    page.events.push({ ...page.events[0], event_key: "event.v1.2" });
    vi.mocked(loadEventPage).mockResolvedValue(page);
    vi.mocked(loadEventDetail).mockResolvedValue(toolDetail("old owner output"));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toHaveLength(2));
    act(() => {
      result.current.toggleEventExpanded("event.v1.1");
      result.current.selectEvent("event.v1.1");
    });
    await waitFor(() => expect(result.current.expandedDetail).toEqual(toolDetail("old owner output")));
    vi.mocked(loadEventDetail).mockReturnValue(deferred<EventDetail>().promise);
    act(() => {
      result.current.toggleEventExpanded("event.v1.2");
      result.current.selectEvent("event.v1.2");
    });
    expect(result.current.expandedDetail).toBeNull();
    expect(result.current.detail).toBeNull();
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => expect(result.current.expandedDetail).toEqual(toolDetail("old owner output")));
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.expandedEventKey).toBeNull());
    expect(result.current.expandedDetail).toBeNull();
    expect(result.current.detail).toBeNull();
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    expect(result.current.expandedDetail).toBeNull();
    expect(result.current.expandedDetailLoading).toBe(true);
  });

  it("updates visible work while scrolled up and keeps it expanded on completion", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const active = trajectoryEventPage();
    active.events[0].trajectory!.status = "working";
    vi.mocked(loadEventPage).mockResolvedValue(active);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    const { container } = await act(async () => render(<ViewerPage />));
    fireEvent.click(await screen.findByRole("button", { name: /session live/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /^Working for/ })).toHaveAttribute("aria-expanded", "true"));
    fireEvent.click(await screen.findByRole("button", { name: "Ran 1 command" }));
    expect(await screen.findByText("cargo test")).toBeInTheDocument();
    const timeline = container.querySelector<HTMLElement>(".conversation__timeline")!;
    Object.defineProperties(timeline, { scrollHeight: { configurable: true, value: 1000 }, clientHeight: { configurable: true, value: 300 } });
    timeline.scrollTop = 120;
    fireEvent.wheel(timeline, { deltaY: -1 });
    fireEvent.scroll(timeline);
    const updated = trajectoryChildPage();
    updated.events[0].tool!.command = "cargo check";
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(updated);
    act(() => emit?.({ session_key: "live", reset: false }));
    expect(await screen.findByText("cargo check")).toBeInTheDocument();
    expect(timeline.scrollTop).toBe(120);
    const finished = trajectoryEventPage();
    finished.events[0].trajectory!.status = "complete";
    vi.mocked(loadEventPage).mockResolvedValue(finished);
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Worked for 1h" })).toHaveAttribute("aria-expanded", "true"));
    expect(screen.getByText("cargo check")).toBeInTheDocument();
  });

  it.each([true, false])("keeps historical work expanded when newer work starts while following is %s", async (following) => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const history = trajectoryEventPage().events[0];
    history.trajectory!.status = "complete";
    const working = { ...history, event_key: "trajectory.v1.current", trajectory: { ...history.trajectory!, status: "working" as const } };
    const page = { ...trajectoryEventPage(), events: [history, working], total_events: 2 };
    vi.mocked(loadEventPage).mockResolvedValue(page);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.expandedEventKey).toBe(working.event_key));
    act(() => {
      result.current.setFollowingLive(following);
      result.current.toggleEventExpanded(history.event_key);
    });
    const nextWorking = { ...working, event_key: "trajectory.v1.next" };
    vi.mocked(loadEventPage).mockResolvedValue({
      ...page,
      events: [history, { ...working, trajectory: { ...working.trajectory, status: "complete" } }, nextWorking],
      total_events: 3,
    });
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(result.current.events).toHaveLength(3));
    expect(result.current.expandedEventKey).toBe(history.event_key);
    act(() => result.current.showLiveActivity());
    expect(result.current.expandedEventKey).toBe(nextWorking.event_key);
  });

  it("keeps a completed turn reopened by the reader expanded when the next turn begins", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = workingTrajectoryPage();
    const key = original.events[0].event_key;
    vi.mocked(loadEventPage).mockResolvedValueOnce(original);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.has_loaded).toBe(true));

    const completed = trajectoryEventPage();
    completed.events[0].trajectory!.status = "complete";
    vi.mocked(loadEventPage).mockResolvedValueOnce(completed);
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(result.current.events[0].trajectory?.status).toBe("complete"));
    expect(result.current.expandedEventKey).toBeNull();
    act(() => result.current.toggleEventExpanded(key));
    expect(result.current.expandedEventKey).toBe(key);

    const nextWorking = workingTrajectoryPage("trajectory.v1.next").events[0];
    const nextPage = { ...completed, events: [...completed.events, nextWorking], total_events: 2 };
    vi.mocked(loadEventPage).mockResolvedValueOnce(nextPage);
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(result.current.events).toEqual(nextPage.events));
    expect(result.current.expandedEventKey).toBe(key);
    act(() => result.current.showLiveActivity());
    expect(result.current.expandedEventKey).toBe(nextWorking.event_key);
  });

  it("auto-expands active work, refreshes its children, respects manual collapse and closes on completion", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const active = trajectoryEventPage();
    active.events[0].trajectory!.status = "working";
    vi.mocked(loadEventPage).mockResolvedValue(active);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    const key = active.events[0].event_key;
    await waitFor(() => expect(result.current.expandedEventKey).toBe(key));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledOnce());
    expect(vi.mocked(loadTrajectoryEventPage).mock.calls[0][0].direction).toBe("backward");
    const children = trajectoryChildPage();
    children.events[0].summary = "new progress";
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(children);
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(key)?.events[0].summary).toBe("new progress"));
    act(() => result.current.toggleEventExpanded(key));
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(3));
    expect(result.current.expandedEventKey).toBeNull();
    act(() => result.current.toggleEventExpanded(key));
    const finished = trajectoryEventPage();
    finished.events[0].trajectory!.status = "complete";
    vi.mocked(loadEventPage).mockResolvedValue(finished);
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(result.current.events[0].trajectory?.status).toBe("complete"));
    expect(result.current.expandedEventKey).toBeNull();
    act(() => result.current.toggleEventExpanded(key));
    expect(result.current.expandedEventKey).toBe(key);
  });

  it("coalesces events during a pending refresh and still applies the newest batch", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    const pending = deferred<EventPageResponse>();
    vi.mocked(loadEventPage).mockReturnValueOnce(pending.promise);
    act(() => emit?.({ session_key: "live", reset: false }));
    act(() => { emit?.({ session_key: "live", reset: false }); emit?.({ session_key: "live", reset: false }); });
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    const latest = toolEventPage();
    latest.events[0].summary = "latest";
    vi.mocked(loadEventPage).mockResolvedValue(latest);
    await act(async () => pending.resolve(toolEventPage()));
    await waitFor(() => expect(result.current.events[0].summary).toBe("latest"));
    expect(loadEventPage).toHaveBeenCalledTimes(3);
  });

  it("refreshes local progress without requiring an unread attention event", async () => {
    let emit: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("local")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "local");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    const next = toolEventPage();
    next.events[0].summary = "running progress";
    vi.mocked(loadEventPage).mockResolvedValue(next);
    act(() => emit?.({ changed: true, attention_session_keys: [], updated_session_keys: ["local"] }));
    await waitFor(() => expect(result.current.events[0].summary).toBe("running progress"));
  });

  it("preserves expansion and updates items even when reading older events", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => {
      emit = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    vi.mocked(loadEventDetail).mockResolvedValue(toolDetail("done"));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    expect(result.current.expandedEventKey).toBe("event.v1.1");
    act(() => result.current.setFollowingLive(false));
    act(() => emit?.({ session_key: "live", reset: false }));
    expect(result.current.pendingLiveActivity).toBe(true);
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(3));
    act(() => result.current.showLiveActivity());
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(4));
    expect(result.current.pendingLiveActivity).toBe(false);
    expect(result.current.expandedEventKey).toBe("event.v1.1");
    act(() => emit?.({ session_key: "live", reset: true }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(5));
    await waitFor(() => expect(result.current.expandedEventKey).toBeNull());
  });

  it("ignores unrelated session bodies and unregisters its listener", async () => {
    const stop = vi.fn();
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(stop); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result, unmount } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => emit?.({ session_key: "another", reset: true }));
    act(() => emit?.({ session_key: null, reset: false }));
    expect(loadEventPage).toHaveBeenCalledOnce();
    unmount();
    expect(stop).toHaveBeenCalledOnce();
  });
});

describe("useViewerState refresh after message input", () => {
  it("refreshes after acceptance without a Relay notification and picks up delayed persistence without resending", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const original = toolEventPage();
    vi.mocked(loadEventPage).mockResolvedValue(original);
    const { container } = render(<ViewerPage />);
    fireEvent.click(await screen.findByRole("button", { name: /session live/ }));
    const input = await screen.findByRole("textbox", { name: "Message this session" });
    await waitFor(() => expect(input).toBeEnabled());
    await screen.findByText("cargo test");
    const timeline = container.querySelector<HTMLElement>(".conversation__timeline")!;
    Object.defineProperties(timeline, { scrollHeight: { configurable: true, value: 1000 }, clientHeight: { configurable: true, value: 300 } });
    timeline.scrollTop = 120;
    fireEvent.wheel(timeline, { deltaY: -1 });
    fireEvent.scroll(timeline);
    vi.useFakeTimers();
    fireEvent.change(input, { target: { value: "hello" } });
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Send" })));
    expect(loadEventPage).toHaveBeenCalledTimes(2);
    expect(input).toHaveValue("");
    expect(screen.queryByText("Response after sending")).not.toBeInTheDocument();
    await act(async () => vi.advanceTimersByTimeAsync(1_000));
    expect(loadEventPage).toHaveBeenCalledTimes(3);
    const response = { ...reasoningEventPage().events[0], type: "message" as const,
      role: "assistant" as const, summary: "Response after sending", reasoning: null };
    vi.mocked(loadEventPage).mockResolvedValue({ ...original, events: [...original.events, response], total_events: 2 });
    await act(async () => vi.advanceTimersByTimeAsync(2_000));
    expect(screen.getByText("Response after sending")).toBeInTheDocument();
    expect(timeline.scrollTop).toBe(120);
    await act(async () => vi.advanceTimersByTimeAsync(5_000));
    await act(async () => vi.advanceTimersByTimeAsync(12_000));
    expect(loadEventPage).toHaveBeenCalledTimes(6);
    await act(async () => vi.advanceTimersByTimeAsync(60_000));
    expect(loadEventPage).toHaveBeenCalledTimes(6);
    expect(submitSessionInput).toHaveBeenCalledOnce();
  });

  it("retains loaded history and open turns while refreshing a reader above the latest event", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const turn = trajectoryEventPage().events[0];
    const older = { ...toolEventPage().events[0], event_key: "old-event" };
    const initial = { ...trajectoryEventPage(), events: [older, turn], total_events: 2 };
    vi.mocked(loadEventPage).mockResolvedValue(initial);
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toHaveLength(2));
    act(() => result.current.toggleEventExpanded(turn.event_key));
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(turn.event_key)?.has_loaded).toBe(true));
    const next = { ...reasoningEventPage().events[0], event_key: "new-event" };
    vi.mocked(loadEventPage).mockResolvedValue({ ...initial, events: [older, turn, next], total_events: 3 });
    act(() => { result.current.setFollowingLive(false); result.current.refreshSessionAfterInput("live"); });
    await waitFor(() => expect(result.current.events).toEqual([older, turn, next]));
    expect(result.current.expandedEventKey).toBe(turn.event_key);
    expect(result.current.pendingLiveActivity).toBe(true);
    expect(result.current.trajectoryPages.get("live")?.get(turn.event_key)?.events).toEqual(trajectoryChildPage().events);
    expect(loadEventPage).toHaveBeenNthCalledWith(2, { session_key: "live", window_mode: "retained", direction: "backward" });
    expect(loadEventPage).toHaveBeenCalledTimes(2);
  });

  it("coalesces scheduled reads during an in-flight refresh", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.eventsLoading).toBe(false));
    const pending = deferred<EventPageResponse>();
    vi.mocked(loadEventPage).mockReturnValueOnce(pending.promise);
    vi.useFakeTimers();
    act(() => result.current.refreshSessionAfterInput("live"));
    await act(async () => vi.advanceTimersByTimeAsync(3_000));
    expect(loadEventPage).toHaveBeenCalledTimes(2);
    await act(async () => pending.resolve(toolEventPage()));
    expect(loadEventPage).toHaveBeenCalledTimes(3);
  });

  it("cancels delayed reads on session switches and ignores late acceptance for the previous target", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live"), session("other")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.eventsLoading).toBe(false));
    vi.useFakeTimers();
    await act(async () => result.current.refreshSessionAfterInput("live"));
    expect(loadEventPage).toHaveBeenCalledTimes(2);
    await act(async () => result.current.selectSession("other"));
    expect(loadEventPage).toHaveBeenCalledTimes(3);
    act(() => result.current.refreshSessionAfterInput("live"));
    await act(async () => vi.advanceTimersByTimeAsync(30_000));
    expect(loadEventPage).toHaveBeenCalledTimes(3);
    await act(async () => result.current.selectSession("live"));
    expect(loadEventPage).toHaveBeenCalledTimes(4);
    await act(async () => vi.advanceTimersByTimeAsync(30_000));
    expect(loadEventPage).toHaveBeenCalledTimes(4);
  });

  it("cancels delayed reads when the viewer unmounts to change machines", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result, unmount } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.eventsLoading).toBe(false));
    vi.useFakeTimers();
    await act(async () => result.current.refreshSessionAfterInput("live"));
    unmount();
    await act(async () => vi.advanceTimersByTimeAsync(30_000));
    expect(loadEventPage).toHaveBeenCalledTimes(2);
  });
});

describe("useViewerState session-index signalling", () => {
  it("keeps the initial catalog metadata-only until the user chooses a session", async () => {
    const indexedSession = session("codex:index-only");
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [indexedSession],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result } = renderHook(() => useViewerState());

    await waitFor(() => expect(result.current.sessions).toEqual([indexedSession]));
    expect(result.current.selectedSessionKey).toBeNull();
    expect(result.current.selectedSession).toBeNull();
    expect(loadEventPage).not.toHaveBeenCalled();

    act(() => result.current.selectSession(indexedSession.session_key));
    await waitFor(() => {
      expect(loadEventPage).toHaveBeenCalledWith({
        session_key: indexedSession.session_key,
        window_mode: "retained", direction: "backward",
      });
    });
  });

  it("subscribes to index changes before the initial catalog query", async () => {
    const subscription = deferred<() => void>();
    vi.mocked(listenForSessionIndexChanges).mockImplementation(() => subscription.promise);
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [],
      next_cursor: null,
      source_errors: [],
      pending_providers: ["codex"],
    });

    renderHook(() => useViewerState());

    expect(listSessions).not.toHaveBeenCalled();
    await act(async () => {
      subscription.resolve(vi.fn());
      await subscription.promise;
    });
    await waitFor(() => expect(listSessions).toHaveBeenCalledOnce());
  });

  it("subscribes to index progress before its snapshot and ignores an older snapshot", async () => {
    const snapshot = deferred<SessionIndexProgress>();
    let progressHandler: ((progress: SessionIndexProgress) => void) | undefined;
    vi.mocked(listenForSessionIndexProgress).mockImplementation((handler) => {
      progressHandler = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(getSessionIndexProgress).mockReturnValue(snapshot.promise);

    const { result } = renderHook(() => useViewerState());

    await waitFor(() => expect(listenForSessionIndexProgress).toHaveBeenCalledOnce());
    await waitFor(() => expect(getSessionIndexProgress).toHaveBeenCalledOnce());
    expect(
      vi.mocked(listenForSessionIndexProgress).mock.invocationCallOrder[0],
    ).toBeLessThan(vi.mocked(getSessionIndexProgress).mock.invocationCallOrder[0]!);

    act(() => progressHandler?.(indexProgress({ revision: "12", activity: "body", is_refreshing: true })));
    await waitFor(() => expect(result.current.sessionIndexProgress?.revision).toBe("12"));

    await act(async () => {
      snapshot.resolve(indexProgress({ revision: "11" }));
      await snapshot.promise;
    });

    expect(result.current.sessionIndexProgress?.revision).toBe("12");
    expect(result.current.sessionIndexProgress?.activity).toBe("body");
  });

  it("refreshes progress after reconnect and accepts a restarted server revision", async () => {
    const initial = deferred<SessionIndexProgress>();
    const resumed = deferred<SessionIndexProgress>();
    let progressHandler: ((progress: SessionIndexProgress) => void) | undefined;
    let reconnectHandler: (() => void) | undefined;
    vi.mocked(listenForSessionIndexProgress).mockImplementation((handler) => {
      progressHandler = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listenForTransportReconnect).mockImplementation((handler) => {
      reconnectHandler = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(getSessionIndexProgress)
      .mockReturnValueOnce(initial.promise)
      .mockReturnValueOnce(resumed.promise);

    const { result } = renderHook(() => useViewerState());
    await waitFor(() => expect(getSessionIndexProgress).toHaveBeenCalledOnce());
    act(() => progressHandler?.(indexProgress({ revision: "12", activity: "body" })));
    await waitFor(() => expect(result.current.sessionIndexProgress?.revision).toBe("12"));

    act(() => reconnectHandler?.());
    await waitFor(() => expect(getSessionIndexProgress).toHaveBeenCalledTimes(2));
    act(() => progressHandler?.(indexProgress({ revision: "2", activity: "catalog" })));
    await waitFor(() => expect(result.current.sessionIndexProgress?.revision).toBe("2"));
    await act(async () => {
      resumed.resolve(indexProgress({ revision: "1" }));
      initial.resolve(indexProgress({ revision: "13" }));
      await Promise.all([resumed.promise, initial.promise]);
    });
    expect(result.current.sessionIndexProgress?.revision).toBe("2");
    expect(result.current.sessionIndexProgress?.activity).toBe("catalog");
  });

  it("uses the retry command for both the status action and sidebar retry", async () => {
    const waiting = indexProgress({
      revision: "2",
      activity: "waiting_to_retry",
      body: {
        ...indexProgress().body,
        pending_jobs: 4,
      },
    });
    vi.mocked(getSessionIndexProgress).mockResolvedValue(indexProgress());
    vi.mocked(retrySessionIndex).mockResolvedValue(waiting);
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    const { result } = renderHook(() => useViewerState());

    await waitFor(() => expect(result.current.sessionIndexProgress?.revision).toBe("1"));
    await waitFor(() => expect(listSessions).toHaveBeenCalledOnce());
    await act(async () => {
      await result.current.retrySessionIndex();
    });
    expect(retrySessionIndex).toHaveBeenCalledOnce();
    expect(result.current.sessionIndexProgress?.revision).toBe("2");

    act(() => result.current.retrySessions());
    await waitFor(() => expect(retrySessionIndex).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(listSessions).toHaveBeenCalledTimes(2));
  });

  it("keeps a retry command failure alongside the last-known index snapshot", async () => {
    vi.mocked(getSessionIndexProgress).mockResolvedValue(indexProgress());
    vi.mocked(retrySessionIndex).mockRejectedValue(new Error("Session index scheduler is unavailable."));
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    const { result } = renderHook(() => useViewerState());

    await waitFor(() => expect(result.current.sessionIndexProgress?.revision).toBe("1"));
    await act(async () => {
      await result.current.retrySessionIndex();
    });

    expect(result.current.sessionIndexProgress?.revision).toBe("1");
    expect(result.current.sessionIndexProgressError).toBe("Session index scheduler is unavailable.");
  });

  it("acknowledges only after React commits an accepted initial event page", async () => {
    const indexedSession = session("codex:indexed");
    const page = deferred<EventPageResponse>();
    const pageStateAtAcknowledgement: Array<{
      eventsOwnerKey: string | null;
      initialPageLoaded: boolean;
      sessionKey: string | null;
    }> = [];
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [indexedSession],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockImplementationOnce(() => page.promise);

    render(<ViewerPageCommitProbe />);

    await waitFor(() => expect(listSessions).toHaveBeenCalledOnce());
    expect(loadEventPage).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Select indexed session" }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledOnce());
    vi.mocked(acknowledgeSessionAttention).mockImplementation(() => {
      const pageState = screen.getByTestId("viewer-page-commit-state");
      pageStateAtAcknowledgement.push({
        eventsOwnerKey: pageState.getAttribute("data-owner"),
        initialPageLoaded: pageState.textContent === "ready",
        sessionKey: pageState.getAttribute("data-session"),
      });
      return Promise.resolve({ changed: true });
    });
    await act(async () => {
      page.resolve({
        ...toolEventPage(),
        attention_revision: "7",
      });
      await Promise.resolve();
    });

    await waitFor(() => {
      expect(acknowledgeSessionAttention).toHaveBeenCalledWith({
        session_key: indexedSession.session_key,
        attention_revision: "7",
      });
    });
    expect(pageStateAtAcknowledgement).toEqual([{
      eventsOwnerKey: indexedSession.session_key,
      initialPageLoaded: true,
      sessionKey: indexedSession.session_key,
    }]);
    await waitFor(() => expect(listSessions).toHaveBeenCalledTimes(2));
  });

  it("keeps final replies unread while scrolled up and acknowledges when following resumes", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue({ ...toolEventPage(), attention_revision: "1" });
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(acknowledgeSessionAttention).toHaveBeenCalledWith({ session_key: "live", attention_revision: "1" }));
    vi.mocked(acknowledgeSessionAttention).mockClear();
    act(() => result.current.setFollowingLive(false));
    vi.mocked(loadEventPage).mockResolvedValue({ ...toolEventPage(), attention_revision: "4" });
    act(() => emit?.({ session_key: "live", reset: false }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.eventsLoading).toBe(false));
    expect(acknowledgeSessionAttention).not.toHaveBeenCalled();
    act(() => result.current.setFollowingLive(true));
    expect(result.current.pendingLiveActivity).toBe(false);
    await waitFor(() => expect(acknowledgeSessionAttention).toHaveBeenCalledWith({ session_key: "live", attention_revision: "4" }));
  });

  it("hides the jump button and acknowledges replies after manually scrolling to the end", async () => {
    let emit: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue({ ...toolEventPage(), attention_revision: "1" });
    const { container } = render(<ViewerPage />);
    fireEvent.click(await screen.findByRole("button", { name: /session live/ }));
    await waitFor(() => expect(acknowledgeSessionAttention).toHaveBeenCalledWith({ session_key: "live", attention_revision: "1" }));
    vi.mocked(acknowledgeSessionAttention).mockClear();
    const timeline = container.querySelector<HTMLElement>(".conversation__timeline")!;
    Object.defineProperties(timeline, {
      scrollHeight: { configurable: true, value: 1000 },
      clientHeight: { configurable: true, value: 300 },
    });
    timeline.scrollTop = 300;
    fireEvent.wheel(timeline, { deltaY: -80 });
    fireEvent.scroll(timeline);
    vi.mocked(loadEventPage).mockResolvedValue({ ...toolEventPage(), attention_revision: "4" });
    act(() => emit?.({ session_key: "live", reset: false }));
    await screen.findByRole("button", { name: "New activity · Jump to latest" });
    timeline.scrollTop = 700;
    fireEvent.wheel(timeline, { deltaY: 80 });
    fireEvent.scroll(timeline);
    await waitFor(() => expect(screen.queryByRole("button", { name: /Jump to latest/ })).not.toBeInTheDocument());
    await waitFor(() => expect(acknowledgeSessionAttention).toHaveBeenCalledWith({ session_key: "live", attention_revision: "4" }));
  });

  it("reopens at last read without acknowledgement, then marks the committed end read on jump", async () => {
    const page = { ...toolEventPage(), attention_revision: "9" };
    const event = page.events[0];
    saveReadingPosition("saved", {
      anchors: [{ slot_key: event.event_key, type: event.type, timestamp: event.timestamp, top: -40 }],
      last_event: readingEventKey(event), at_end: false,
    });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("saved")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(page);
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "saved");
    await waitFor(() => expect(result.current.initialPageLoaded).toBe(true));
    expect(acknowledgeSessionAttention).not.toHaveBeenCalled();
    act(() => result.current.showLiveActivity());
    await waitFor(() => expect(acknowledgeSessionAttention).toHaveBeenCalledWith({ session_key: "saved", attention_revision: "9" }));
  });

  it("does not acknowledge a page invalidated by a selection change before commit", async () => {
    const first = session("codex:first");
    const second = session("codex:second");
    const firstPage = deferred<EventPageResponse>();
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [first, second],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage)
      .mockImplementationOnce(() => firstPage.promise)
      .mockImplementationOnce(() => new Promise(() => undefined));
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, first.session_key);
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledOnce());
    await act(async () => {
      firstPage.resolve({
        ...toolEventPage(),
        attention_revision: "10",
      });
      // The response handler runs in this microtask, but React has not
      // committed its state updates yet. Selecting another session here is
      // the race that must suppress the acknowledgement.
      await Promise.resolve();
      result.current.selectSession(second.session_key);
    });

    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    expect(acknowledgeSessionAttention).not.toHaveBeenCalled();
  });

  it("does not acknowledge an event page React discarded as stale", async () => {
    const first = session("codex:first");
    const second = session("codex:second");
    const stalePage = deferred<EventPageResponse>();
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [first, second],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage)
      .mockImplementationOnce(() => stalePage.promise)
      .mockResolvedValueOnce(toolEventPage());
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, first.session_key);
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(1));
    act(() => result.current.selectSession(second.session_key));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    await act(async () => {
      stalePage.resolve({
        ...toolEventPage(),
        attention_revision: "9",
      });
      await stalePage.promise;
    });

    expect(acknowledgeSessionAttention).not.toHaveBeenCalled();
  });

  it("reloads the sidebar after an index-change event and unregisters on cleanup", async () => {
    const unlisten = vi.fn();
    let emitIndexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => {
      emitIndexChange = handler;
      return Promise.resolve(unlisten);
    });
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [session("codex:listener")],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { unmount } = renderHook(() => useViewerState());

    await waitFor(() => expect(emitIndexChange).toBeDefined());
    await waitFor(() => expect(listSessions).toHaveBeenCalledTimes(1));
    expect(loadEventPage).not.toHaveBeenCalled();
    act(() => emitIndexChange?.({ changed: true, attention_session_keys: [] }));
    await waitFor(() => expect(listSessions).toHaveBeenCalledTimes(2));
    expect(loadEventPage).not.toHaveBeenCalled();
    unmount();
    expect(unlisten).toHaveBeenCalledOnce();
  });

  it("coalesces index and Relay changes into one trailing catalog read", async () => {
    const first = deferred<ListSessionsResponse>();
    const second = deferred<ListSessionsResponse>();
    let emitIndexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    let emitRelayChange: ((change: RelayChange) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => {
      emitIndexChange = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => {
      emitRelayChange = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listSessions)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    const { result } = renderHook(() => useViewerState());
    await waitFor(() => expect(listSessions).toHaveBeenCalledOnce());

    for (let index = 0; index < 5; index += 1) {
      act(() => emitIndexChange?.({ changed: true, attention_session_keys: [] }));
      act(() => emitRelayChange?.({ session_key: null, reset: false }));
    }
    expect(listSessions).toHaveBeenCalledOnce();
    const initial = session("codex:first-result");
    await act(async () => first.resolve({
      sessions: [initial], next_cursor: null, source_errors: [], pending_providers: [],
    }));
    expect(result.current.sessions).toEqual([initial]);
    expect(listSessions).toHaveBeenCalledTimes(2);

    const latest = session("codex:latest-result");
    await act(async () => second.resolve({
      sessions: [latest], next_cursor: null, source_errors: [], pending_providers: [],
    }));
    expect(result.current.sessions).toEqual([latest]);
    expect(result.current.sessionsLoading).toBe(false);
    expect(listSessions).toHaveBeenCalledTimes(2);
  });

  it("starts a changed provider query immediately and ignores the superseded catalog", async () => {
    const oldQuery = deferred<ListSessionsResponse>();
    const newQuery = deferred<ListSessionsResponse>();
    let emitIndexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => {
      emitIndexChange = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listSessions)
      .mockReturnValueOnce(oldQuery.promise)
      .mockReturnValueOnce(newQuery.promise);
    const { result } = renderHook(() => useViewerState());
    await waitFor(() => expect(listSessions).toHaveBeenCalledOnce());
    act(() => emitIndexChange?.({ changed: true, attention_session_keys: [] }));
    act(() => result.current.toggleProvider("pi"));
    expect(listSessions).toHaveBeenCalledTimes(2);
    expect(vi.mocked(listSessions).mock.calls[1]![0].query.providers).not.toContain("pi");

    const current = session("codex:current-query");
    await act(async () => newQuery.resolve({
      sessions: [current], next_cursor: null, source_errors: [], pending_providers: [],
    }));
    await act(async () => oldQuery.resolve({
      sessions: [session("codex:stale-query")], next_cursor: null, source_errors: [], pending_providers: [],
    }));
    expect(result.current.sessions).toEqual([current]);
    expect(listSessions).toHaveBeenCalledTimes(2);
  });

  it("waits for pagination before a queued catalog refresh, including after a failure", async () => {
    const more = deferred<ListSessionsResponse>();
    let emitIndexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => {
      emitIndexChange = handler;
      return Promise.resolve(vi.fn());
    });
    const initial = session("codex:page-one");
    vi.mocked(listSessions)
      .mockResolvedValueOnce({
        sessions: [initial], next_cursor: "sessions.v1.1", source_errors: [], pending_providers: [],
      })
      .mockReturnValueOnce(more.promise)
      .mockResolvedValueOnce({
        sessions: [initial], next_cursor: null, source_errors: [], pending_providers: [],
      });
    const { result } = renderHook(() => useViewerState());
    await waitFor(() => expect(result.current.sessionsLoading).toBe(false));
    act(() => result.current.loadMoreSessions());
    expect(listSessions).toHaveBeenCalledTimes(2);
    expect(vi.mocked(listSessions).mock.calls[1]![0].cursor).toBe("sessions.v1.1");
    for (let index = 0; index < 3; index += 1) {
      act(() => emitIndexChange?.({ changed: true, attention_session_keys: [] }));
    }
    expect(listSessions).toHaveBeenCalledTimes(2);
    await act(async () => more.reject(new Error("index temporarily unavailable")));
    expect(listSessions).toHaveBeenCalledTimes(3);
    expect(result.current.sessions).toEqual([initial]);
    expect(result.current.sessionsLoadingMore).toBe(false);
    expect(result.current.sessionsError).toBeNull();
  });

  it("drops queued catalog work when unmounting to change machines", async () => {
    const pending = deferred<ListSessionsResponse>();
    let emitIndexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => {
      emitIndexChange = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listSessions).mockReturnValueOnce(pending.promise);
    const { unmount } = renderHook(() => useViewerState());
    await waitFor(() => expect(listSessions).toHaveBeenCalledOnce());
    act(() => emitIndexChange?.({ changed: true, attention_session_keys: [] }));
    unmount();
    await act(async () => pending.resolve({
      sessions: [], next_cursor: null, source_errors: [], pending_providers: [],
    }));
    expect(listSessions).toHaveBeenCalledOnce();
  });

  it("retains an explicitly selected child across a root catalog refresh", async () => {
    const root = session("codex:parent");
    root.child_count = 1;
    const child = session("codex:child");
    child.parent_session_id = root.session_id;
    child.is_subagent = true;
    let emitIndexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => {
      emitIndexChange = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [root],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, root.session_key);
    await waitFor(() => expect(result.current.selectedSessionKey).toBe(root.session_key));
    act(() => result.current.openRelatedSession(root.session_key, child));
    await waitFor(() => expect(result.current.selectedSessionKey).toBe(child.session_key));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));

    act(() => emitIndexChange?.({ changed: true, attention_session_keys: [] }));
    await waitFor(() => expect(listSessions).toHaveBeenCalledTimes(2));
    expect(result.current.selectedSessionKey).toBe(child.session_key);
    expect(result.current.selectedSession?.session_key).toBe(child.session_key);
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
  });

  it("keeps the visible session when a background catalog refresh fails", async () => {
    const selected = session("codex:refresh-failure");
    let emitIndexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => {
      emitIndexChange = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listSessions)
      .mockResolvedValueOnce({
        sessions: [selected],
        next_cursor: null,
        source_errors: [],
        pending_providers: [],
      })
      .mockRejectedValueOnce(new Error("local index is temporarily unavailable"));
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, selected.session_key);
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledOnce());
    act(() => emitIndexChange?.({ changed: true, attention_session_keys: [] }));

    await waitFor(() => {
      expect(result.current.sessionsError).toBe("local index is temporarily unavailable");
    });
    expect(result.current.sessions).toEqual([selected]);
    expect(result.current.selectedSessionKey).toBe(selected.session_key);
    expect(result.current.selectedSession?.session_key).toBe(selected.session_key);
    expect(loadEventPage).toHaveBeenCalledOnce();
  });

  it("reloads and acknowledges only a selected session named by index attention", async () => {
    const selected = session("codex:selected");
    let emitIndexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => {
      emitIndexChange = handler;
      return Promise.resolve(vi.fn());
    });
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [selected],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage)
      .mockResolvedValueOnce(toolEventPage())
      .mockResolvedValueOnce({ ...toolEventPage(), attention_revision: "12" });

    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, selected.session_key);
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(1));
    act(() => emitIndexChange?.({
      changed: true,
      attention_session_keys: [selected.session_key],
    }));
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    await waitFor(() => {
      expect(acknowledgeSessionAttention).toHaveBeenCalledWith({
        session_key: selected.session_key,
        attention_revision: "12",
      });
    });
  });
});

describe("useViewerState inspector focus", () => {
  it("restores focus to the selected event button after a pointer-style selection", () => {
    vi.spyOn(window, "requestAnimationFrame").mockImplementation((callback) => {
      callback(0);
      return 1;
    });
    const eventKey = "event.v1.2";
    const trigger = document.createElement("button");
    trigger.id = eventButtonId(eventKey);
    document.body.append(trigger);
    const { result, unmount } = renderHook(() => useViewerState());

    expect(document.activeElement).toBe(document.body);
    act(() => result.current.selectEvent(eventKey));
    expect(result.current.inspectorOpen).toBe(true);
    act(() => result.current.closeInspector());

    expect(trigger).toHaveFocus();
    unmount();
    trigger.remove();
  });
});

describe("useViewerState expanded tool detail", () => {
  it("loads only after expansion and reuses the shared detail cache", async () => {
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [{
        session_key: "codex:session-1",
        session_id: "session-1",
        parent_session_id: null,
        is_subagent: false,
        provider: "codex",
        title: "Tool session",
        preview: "Run the checks",
        project: "viewer",
        cwd: "/work/repo",
        updated_at_ms: 1,
        timestamp: "2026-08-31T00:00:00Z",
        agent_path: null,
        agent_nickname: null,
        agent_role: null,
        child_count: 0,
        message_count: null,
        event_count: 1,
        history_status: "complete",
        has_unread: false,
      }],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue({
      events: [{
        event_key: "event.v1.1",
        type: "tool_call",
        provider: "codex",
        timestamp: "2026-08-31T00:00:00Z",
        phase: "finished",
        role: null,
        title: "exec_command",
        summary: "shell exit 0 cargo test",
        summary_truncated: false,
        is_hidden: false,
        is_error: false,
        tool: {
          kind: "shell",
          tool_name: "exec_command",
          tool_call_id: "call-1",
          command: "cargo test",
          cwd: "/work/repo",
          path: null,
          query: null,
          url: null,
          task_title: null,
          exit_code: 0,
          bytes: null,
          added: null,
          removed: null,
        },
        usage: null,
        reasoning: null,
      }],
      next_cursor: null,
      previous_cursor: null,
      total_events: 1,
      history_status: "complete",
    });
    vi.mocked(loadEventDetail).mockResolvedValue({
      event_key: "event.v1.1",
      event: { type: "tool_call" },
      native: null,
      is_hidden: false,
      tool_output: {
        sections: [{ label: "stdout", text: "ok", format: "text" }],
        truncated: false,
        original_size_bytes: 2,
        source_event_key: "event.v1.1",
      },
    });
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, "codex:session-1");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    expect(loadEventDetail).not.toHaveBeenCalled();

    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => expect(result.current.expandedDetail?.tool_output).not.toBeNull());
    expect(loadEventDetail).toHaveBeenCalledOnce();

    act(() => result.current.toggleEventExpanded("event.v1.1"));
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => expect(result.current.expandedDetail?.tool_output).not.toBeNull());
    expect(loadEventDetail).toHaveBeenCalledOnce();
  });

  it("ignores stale cache writes without deleting a fresh request for the same session key", async () => {
    const oldA = deferred<EventDetail>();
    const freshA = deferred<EventDetail>();
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [session("codex:a"), session("codex:b")],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    vi.mocked(loadEventDetail)
      .mockImplementationOnce(() => oldA.promise)
      .mockImplementationOnce(() => freshA.promise);
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, "codex:a");
    await waitFor(() => expect(result.current.eventsOwnerKey).toBe("codex:a"));
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledTimes(1));

    act(() => result.current.selectSession("codex:b"));
    await waitFor(() => expect(result.current.eventsOwnerKey).toBe("codex:b"));
    act(() => result.current.selectSession("codex:a"));
    await waitFor(() => expect(result.current.eventsOwnerKey).toBe("codex:a"));
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledTimes(2));

    await act(async () => {
      oldA.resolve(toolDetail("stale A"));
      await oldA.promise;
    });
    expect(result.current.expandedDetail).toBeNull();
    expect(result.current.expandedDetailLoading).toBe(true);

    act(() => result.current.toggleEventExpanded("event.v1.1"));
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => expect(result.current.expandedDetailLoading).toBe(true));
    expect(loadEventDetail).toHaveBeenCalledTimes(2);

    await act(async () => {
      freshA.resolve(toolDetail("fresh A"));
      await freshA.promise;
    });
    await waitFor(() => {
      expect(result.current.expandedDetail?.tool_output?.sections[0]?.text).toBe("fresh A");
    });

    act(() => result.current.toggleEventExpanded("event.v1.1"));
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => {
      expect(result.current.expandedDetail?.tool_output?.sections[0]?.text).toBe("fresh A");
    });
    expect(loadEventDetail).toHaveBeenCalledTimes(2);
  });

  it("refreshes expanded and Inspector detail after a same-session event update", async () => {
    const stale = deferred<EventDetail>();
    const fresh = deferred<EventDetail>();
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [session("codex:session-1")],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage)
      .mockResolvedValueOnce(pendingToolEventPage())
      .mockResolvedValueOnce(toolEventPage());
    vi.mocked(loadEventDetail)
      .mockImplementationOnce(() => stale.promise)
      .mockImplementationOnce(() => fresh.promise);
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, "codex:session-1");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => result.current.selectEvent("event.v1.1"));
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledTimes(1));

    act(() => result.current.retryEvents());
    await waitFor(() => expect(loadEventPage).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledTimes(2));

    await act(async () => {
      fresh.resolve(toolDetail("fresh output"));
      await fresh.promise;
    });
    await waitFor(() => {
      expect(result.current.detail?.tool_output?.sections[0]?.text).toBe("fresh output");
      expect(result.current.expandedDetail?.tool_output?.sections[0]?.text).toBe("fresh output");
    });

    await act(async () => {
      stale.resolve(toolDetail("stale output"));
      await stale.promise;
    });
    expect(result.current.detail?.tool_output?.sections[0]?.text).toBe("fresh output");
    expect(result.current.expandedDetail?.tool_output?.sections[0]?.text).toBe("fresh output");

    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => expect(result.current.expandedEventKey).toBeNull());
    act(() => result.current.toggleEventExpanded("event.v1.1"));
    await waitFor(() => {
      expect(result.current.expandedDetail?.tool_output?.sections[0]?.text).toBe("fresh output");
    });

    act(() => result.current.closeInspector());
    await waitFor(() => expect(result.current.inspectorOpen).toBe(false));
    act(() => result.current.toggleInspector());
    await waitFor(() => {
      expect(result.current.detail?.tool_output?.sections[0]?.text).toBe("fresh output");
    });
    expect(loadEventDetail).toHaveBeenCalledTimes(2);
  });
});

describe("useViewerState expanded reasoning detail", () => {
  it("loads a compaction summary that arrives after its card was expanded", async () => {
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [session("codex:compaction")], next_cursor: null, source_errors: [], pending_providers: [],
    });
    const compaction = { state: "started", trigger: null, reason: null, has_summary: false, summary_opaque: false, measurements: [] };
    vi.mocked(loadEventPage)
      .mockResolvedValueOnce(reasoningEventPage({ type: "compaction", title: "Compacting…", reasoning: null, compaction }))
      .mockResolvedValue(reasoningEventPage({ type: "compaction", title: "Context compacted", reasoning: null,
        compaction: { ...compaction, state: "completed", has_summary: true } }));
    vi.mocked(loadEventDetail).mockResolvedValue({ event_key: "event.v1.reasoning",
      event: { type: "compaction", summary: "Retained decisions" }, native: null, is_hidden: false, tool_output: null });
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "codex:compaction");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => result.current.toggleEventExpanded("event.v1.reasoning"));
    expect(loadEventDetail).not.toHaveBeenCalled();
    act(() => result.current.retryEvents());
    await waitFor(() => expect(result.current.events[0].title).toBe("Context compacted"));
    await waitFor(() => expect(result.current.expandedDetail?.event).toMatchObject({ summary: "Retained decisions" }));
    expect(loadEventDetail).toHaveBeenCalledOnce();
    act(() => result.current.selectEvent("event.v1.reasoning"));
    await waitFor(() => expect(result.current.detail?.event).toMatchObject({ type: "compaction" }));
    expect(loadEventDetail).toHaveBeenCalledOnce();
  });

  it("loads readable reasoning only after expansion and shares the detail cache", async () => {
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [session("codex:reasoning")],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(reasoningEventPage());
    vi.mocked(loadEventDetail).mockResolvedValue({
      event_key: "event.v1.reasoning",
      event: { type: "reasoning", summary: "Inspect the source" },
      native: null,
      is_hidden: false,
      tool_output: null,
    });
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, "codex:reasoning");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    expect(loadEventDetail).not.toHaveBeenCalled();

    act(() => result.current.toggleEventExpanded("event.v1.reasoning"));
    await waitFor(() => expect(result.current.expandedDetail?.event).toMatchObject({ type: "reasoning" }));
    expect(loadEventDetail).toHaveBeenCalledOnce();

    act(() => result.current.selectEvent("event.v1.reasoning"));
    await waitFor(() => expect(result.current.detail?.event).toMatchObject({ type: "reasoning" }));
    expect(loadEventDetail).toHaveBeenCalledOnce();
  });

  it("does not request opaque, redacted, or hidden reasoning detail", async () => {
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [session("codex:opaque")],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(reasoningEventPage({
      event_key: "event.v1.opaque",
      summary: "Reasoning redacted by provider",
      is_hidden: false,
      reasoning: {
        preview: null,
        has_summary: false,
        has_text: false,
        has_encrypted_content: true,
        is_redacted: true,
      },
    }));
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, "codex:opaque");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => result.current.toggleEventExpanded("event.v1.opaque"));
    await waitFor(() => expect(result.current.expandedEventKey).toBe("event.v1.opaque"));
    expect(result.current.expandedDetailLoading).toBe(false);
    expect(loadEventDetail).not.toHaveBeenCalled();
  });
});

describe("useViewerState related-session navigation", () => {
  const root = { ...session("codex:root"), child_count: 1 };
  const source = { ...session("codex:source"), parent_session_id: root.session_id, is_subagent: true };

  beforeEach(() => {
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [root], next_cursor: null, source_errors: [], pending_providers: [],
    });
    vi.mocked(listSessionChildren).mockResolvedValue({ sessions: [source], next_cursor: null });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
  });

  it.each(["parent", "sibling", "grandchild"])("opens a verified %s sender without changing the sidebar ancestry", async (kind) => {
    const target = kind === "parent" ? root : {
      ...session(`codex:${kind}`), is_subagent: true,
      parent_session_id: kind === "sibling" ? root.session_id : "codex:unloaded-middle-task",
    };
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, root.session_key);
    act(() => result.current.loadSessionChildren(root.session_key));
    await waitFor(() => expect(result.current.sessionChildren.get(root.session_key)?.sessions).toEqual([source]));
    act(() => result.current.selectSession(source.session_key));
    await waitFor(() => expect(result.current.selectedSession).toEqual(source));
    const initialTree = result.current.sessionChildren;
    act(() => result.current.openRelatedSession(source.session_key, target));
    await waitFor(() => expect(result.current.selectedSession).toEqual(target));
    expect(result.current.selectedSessionKey).toBe(target.session_key);
    expect(result.current.sessionChildren).toBe(initialTree);
    expect(result.current.sessionChildren.has(source.session_key)).toBe(false);
    expect(listSessionChildren).toHaveBeenCalledOnce();
    await waitFor(() => expect(loadEventPage).toHaveBeenLastCalledWith(expect.objectContaining({ session_key: target.session_key })));

    // Background root refreshes must not erase a selected sender whose
    // metadata never belonged to the currently loaded child page.
    act(() => result.current.retrySessions());
    await waitFor(() => expect(listSessions).toHaveBeenCalledTimes(2));
    expect(result.current.selectedSession).toEqual(target);
    expect(result.current.sessionChildren).toBe(initialTree);
  });

  it("ignores a sender button from a previously selected task", async () => {
    const sibling = { ...session("codex:sibling"), is_subagent: true, parent_session_id: root.session_id };
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, root.session_key);
    act(() => result.current.loadSessionChildren(root.session_key));
    await waitFor(() => expect(result.current.sessionChildren.get(root.session_key)?.sessions).toEqual([source]));
    act(() => result.current.selectSession(source.session_key));
    await waitFor(() => expect(result.current.selectedSession).toEqual(source));
    const staleOpen = result.current.openRelatedSession;
    act(() => result.current.selectSession(root.session_key));
    const initialTree = result.current.sessionChildren;
    act(() => staleOpen(source.session_key, sibling));
    expect(result.current.selectedSession).toEqual(root);
    expect(result.current.sessionChildren).toBe(initialTree);
    expect(listSessionChildren).toHaveBeenCalledOnce();
  });

  it("keeps a sender outside the loaded tree available as a navigation source", async () => {
    const sender = { ...session("codex:deep-sender"), is_subagent: true, parent_session_id: "codex:missing-middle" };
    const replySender = { ...session("codex:reply-sender"), is_subagent: true, parent_session_id: "codex:other-middle" };
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, root.session_key);
    act(() => result.current.openRelatedSession(root.session_key, sender));
    await waitFor(() => expect(result.current.selectedSession).toEqual(sender));
    act(() => result.current.openRelatedSession(sender.session_key, replySender));
    await waitFor(() => expect(result.current.selectedSession).toEqual(replySender));
    expect(result.current.sessionChildren.size).toBe(0);
    expect(listSessionChildren).not.toHaveBeenCalled();
  });
});

describe("useViewerState agent communication detail", () => {
  function communicationPage(hasText = true, hidden = false): EventPageResponse {
    return reasoningEventPage({
      event_key: "event.v1.communication", type: "agent_activity", title: "Agent message",
      reasoning: null, is_hidden: hidden,
      agent_activity: {
        kind: "message", event_id: "delivery-1", target_session_id: null,
        target_agent_path: "/root", target: null, actor_agent_path: "/root/reviewer",
        communication: { has_text: hasText, has_encrypted_content: !hasText, trigger_turn: false },
      },
    });
  }

  function communicationDetail(): EventDetail {
    return { event_key: "event.v1.communication", native: null, is_hidden: false, tool_output: null,
      event: { type: "agent_activity", communication: { text: "Review result" } } };
  }

  beforeEach(() => {
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [session("codex:communication"), session("codex:next")],
      next_cursor: null, source_errors: [], pending_providers: [],
    });
  });

  it.each(["question_request", "question_reply"])("loads historical %s detail after expansion and shares it with Inspector", async (type) => {
    const page = communicationPage();
    page.events = [{ ...page.events[0], type, agent_activity: null, title: "Questions" }];
    const recorded: EventDetail = { ...communicationDetail(), event: {
      ...(type === "question_reply"
        ? { type, request_id: "call-1", replies: [{ question_id: "storage", question: "Choose storage", answers: ["SQLite"] }] }
        : { type, is_blocking: false, questions: [{ question: "Choose storage", options: null }] }),
    } };
    vi.mocked(loadEventPage).mockResolvedValue(page);
    vi.mocked(loadEventDetail).mockResolvedValue(recorded);
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "codex:communication");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => result.current.toggleEventExpanded("event.v1.communication"));
    await waitFor(() => expect(result.current.expandedDetail).toEqual(recorded));
    act(() => result.current.selectEvent("event.v1.communication"));
    await waitFor(() => expect(result.current.detail).toEqual(recorded));
    expect(loadEventDetail).toHaveBeenCalledOnce();
  });

  it.each(["question_reply", "message"])("clears question badges from resolved %s pages despite a stale catalog", async (type) => {
    let emit: ((change: RelayChange) => void) | undefined;
    let indexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForRelayChanges).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => { indexChange = handler; return Promise.resolve(vi.fn()); });
    const stale = { ...session("codex:communication"), question_attention: { required_count: 0, available_count: 2 } };
    vi.mocked(listSessions).mockResolvedValue({ sessions: [stale, session("codex:next")], next_cursor: null, source_errors: [], pending_providers: [] });
    const pending = { ...communicationPage(), outstanding_questions: [
      { event_key: "event.v1.communication", requires_input: false, unanswered_count: 2 },
    ] };
    vi.mocked(loadEventPage).mockResolvedValueOnce(pending)
      .mockResolvedValue({ ...communicationPage(), events: [{ ...communicationPage().events[0], type }], outstanding_questions: [] });
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "codex:communication");
    await waitFor(() => expect(result.current.outstandingQuestions).toHaveLength(1));
    act(() => emit?.({ session_key: "codex:communication", reset: false }));
    await waitFor(() => expect(result.current.outstandingQuestions).toEqual([]));
    expect(result.current.sessions[0].question_attention).toEqual({ required_count: 0, available_count: 0 });
    expect(result.current.selectedSession?.question_attention?.available_count).toBe(0);
    act(() => indexChange?.({ changed: true, attention_session_keys: [] }));
    await waitFor(() => expect(listSessions).toHaveBeenCalledTimes(2));
    expect(result.current.sessions[0].question_attention?.available_count).toBe(0);
  });

  it("clears cached child question badges and keeps them cleared after switching back to the parent", async () => {
    const root = session("codex:communication");
    const child = { ...session("codex:child"), parent_session_id: root.session_id, is_subagent: true,
      question_attention: { required_count: 1, available_count: 0 } };
    root.child_count = 1;
    vi.mocked(listSessions).mockResolvedValue({ sessions: [root], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue({ ...communicationPage(), outstanding_questions: [] });
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, root.session_key);
    await waitFor(() => expect(result.current.initialPageLoaded).toBe(true));
    act(() => result.current.openRelatedSession(root.session_key, child));
    await waitFor(() => expect(result.current.selectedSession?.question_attention?.required_count).toBe(0));
    act(() => result.current.selectSession(root.session_key));
    expect(result.current.sessionChildren.get(root.session_key)?.sessions.find((row) => row.session_key === child.session_key)?.question_attention)
      .toEqual({ required_count: 0, available_count: 0 });
  });

  it("opens an outstanding question from the sidebar without consuming it and clears navigation on session switches", async () => {
    const page = communicationPage();
    page.events = [{ ...page.events[0], type: "question_request", agent_activity: null, title: "Questions" }];
    page.outstanding_questions = [{ event_key: "event.v1.communication", requires_input: false, unanswered_count: 2 }];
    vi.mocked(loadEventPage).mockResolvedValue(page);
    vi.mocked(loadEventDetail).mockResolvedValue(communicationDetail());
    const { result } = renderHook(() => useViewerState());
    await waitFor(() => expect(result.current.sessions).toHaveLength(2));
    act(() => result.current.selectQuestionSession("codex:communication"));
    await waitFor(() => expect(result.current.expandedEventKey).toBe("event.v1.communication"));
    expect(result.current.outstandingQuestions).toEqual(page.outstanding_questions);
    expect(result.current.questionNavigation?.event_key).toBe("event.v1.communication");
    await waitFor(() => expect(result.current.expandedDetail).not.toBeNull());
    const previous = result.current.questionNavigation?.revision ?? 0;
    act(() => result.current.selectQuestionSession("codex:communication"));
    expect(result.current.questionNavigation?.revision).toBeGreaterThan(previous);
    expect(result.current.outstandingQuestions).toHaveLength(1);
    act(() => result.current.selectSession("codex:next"));
    expect(result.current.outstandingQuestions).toEqual([]);
    expect(result.current.questionNavigation).toBeNull();
  });

  it("loads after expansion, retries failures, and shares detail with Inspector", async () => {
    vi.mocked(loadEventPage).mockResolvedValue(communicationPage());
    vi.mocked(loadEventDetail).mockRejectedValueOnce(new Error("Snapshot changed"))
      .mockResolvedValue(communicationDetail());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "codex:communication");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    expect(loadEventDetail).not.toHaveBeenCalled();
    act(() => result.current.toggleEventExpanded("event.v1.communication"));
    await waitFor(() => expect(result.current.expandedDetailError).toBe("Snapshot changed"));
    act(() => result.current.retryExpandedDetail());
    await waitFor(() => expect(result.current.expandedDetail).toEqual(communicationDetail()));
    expect(loadEventDetail).toHaveBeenCalledTimes(2);
    act(() => result.current.selectEvent("event.v1.communication"));
    await waitFor(() => expect(result.current.detail).toEqual(communicationDetail()));
    expect(loadEventDetail).toHaveBeenCalledTimes(2);
  });

  it.each(["encrypted", "hidden"])("does not request %s message content on expansion", async (kind) => {
    vi.mocked(loadEventPage).mockResolvedValue(communicationPage(kind !== "encrypted", kind === "hidden"));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "codex:communication");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => result.current.toggleEventExpanded("event.v1.communication"));
    expect(result.current.expandedEventKey).toBe("event.v1.communication");
    expect(result.current.expandedDetailLoading).toBe(false);
    expect(loadEventDetail).not.toHaveBeenCalled();
  });

  it("loads communication detail through a nested trajectory expansion", async () => {
    vi.mocked(loadEventPage).mockResolvedValue(trajectoryEventPage());
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(communicationPage());
    vi.mocked(loadEventDetail).mockResolvedValue(communicationDetail());
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "codex:communication");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => result.current.toggleEventExpanded("trajectory.v1.turn-1"));
    await waitFor(() => expect(result.current.trajectoryPages.get("codex:communication")
      ?.get("trajectory.v1.turn-1")?.events).toHaveLength(1));
    expect(loadEventDetail).not.toHaveBeenCalled();
    act(() => result.current.toggleTrajectoryEventExpanded("trajectory.v1.turn-1", "event.v1.communication"));
    await waitFor(() => expect(result.current.expandedTrajectoryDetail).toEqual(communicationDetail()));
    expect(loadEventDetail).toHaveBeenCalledWith({
      session_key: "codex:communication", event_key: "event.v1.communication",
    });
  });

  it("discards communication detail after switching sessions", async () => {
    const pending = deferred<EventDetail>();
    vi.mocked(loadEventPage).mockResolvedValue(communicationPage());
    vi.mocked(loadEventDetail).mockImplementation(() => pending.promise);
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "codex:communication");
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    act(() => result.current.toggleEventExpanded("event.v1.communication"));
    await waitFor(() => expect(loadEventDetail).toHaveBeenCalledOnce());
    act(() => result.current.selectSession("codex:next"));
    await act(async () => { pending.resolve(communicationDetail()); await pending.promise; });
    expect(result.current.expandedDetail).toBeNull();
    expect(result.current.expandedEventKey).toBeNull();
  });
});

describe("useViewerState whole-turn trajectories", () => {
  it("loads a bounded child page only after opening and gives child tools their normal detail path", async () => {
    const root = session("codex:trajectory-root");
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [root],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(trajectoryEventPage());
    vi.mocked(loadTrajectoryEventPage).mockResolvedValue(trajectoryChildPage());
    vi.mocked(loadEventDetail).mockResolvedValue({
      event_key: "event.v1.trajectory-tool",
      event: { type: "tool_call" },
      native: null,
      is_hidden: false,
      tool_output: {
        sections: [{ label: "stdout", text: "nested output", format: "text" }],
        truncated: false,
        original_size_bytes: 13,
        source_event_key: "event.v1.trajectory-tool",
      },
    });
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, root.session_key);
    await waitFor(() => expect(result.current.events).toHaveLength(1));
    expect(loadTrajectoryEventPage).not.toHaveBeenCalled();
    expect(loadEventDetail).not.toHaveBeenCalled();

    act(() => result.current.toggleEventExpanded("trajectory.v1.turn-1"));
    await waitFor(() => {
      expect(result.current.trajectoryPages.get(root.session_key)?.get("trajectory.v1.turn-1")?.events)
        .toHaveLength(1);
    });
    expect(loadTrajectoryEventPage).toHaveBeenCalledWith({
      session_key: root.session_key,
      trajectory_key: "trajectory.v1.turn-1",
      cursor: undefined,
      direction: "forward",
      limit: 40,
    });
    expect(loadEventDetail).not.toHaveBeenCalled();

    act(() => result.current.toggleTrajectoryEventExpanded(
      "trajectory.v1.turn-1",
      "event.v1.trajectory-tool",
    ));
    await waitFor(() => {
      expect(result.current.expandedTrajectoryDetail?.tool_output?.sections[0]?.text)
        .toBe("nested output");
    });
    expect(result.current.expandedEventKey).toBe("trajectory.v1.turn-1");
    expect(result.current.expandedTrajectoryEventKey).toBe("event.v1.trajectory-tool");
    expect(loadEventDetail).toHaveBeenCalledWith({
      session_key: root.session_key,
      event_key: "event.v1.trajectory-tool",
    });

    act(() => result.current.selectEvent("event.v1.trajectory-tool"));
    await waitFor(() => {
      expect(result.current.selectedEvent?.event_key).toBe("event.v1.trajectory-tool");
    });
    expect(loadEventDetail).toHaveBeenCalledOnce();
  });

  it("ignores a stale child page after the selected session changes", async () => {
    const root = session("codex:trajectory-root");
    const nextSession = session("codex:next-session");
    const childPage = deferred<TrajectoryEventPageResponse>();
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [root, nextSession],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(trajectoryEventPage());
    vi.mocked(loadTrajectoryEventPage).mockImplementation(() => childPage.promise);
    const { result } = renderHook(() => useViewerState());

    await selectListedSession(result, root.session_key);
    await waitFor(() => expect(result.current.selectedSessionKey).toBe(root.session_key));
    act(() => result.current.toggleEventExpanded("trajectory.v1.turn-1"));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledOnce());

    act(() => result.current.selectSession(nextSession.session_key));
    await waitFor(() => expect(result.current.selectedSessionKey).toBe(nextSession.session_key));

    await act(async () => {
      childPage.resolve(trajectoryChildPage());
      await childPage.promise;
    });

    expect(result.current.trajectoryPages.has(root.session_key)).toBe(false);
  });

  it("assembles legacy pages atomically without exposing partial inner groups", async () => {
    const root = session("codex:trajectory-cursors");
    const child = trajectoryChildPage().events[0];
    const remaining = deferred<TrajectoryEventPageResponse>();
    vi.mocked(listSessions).mockResolvedValue({ sessions: [root], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadEventPage).mockResolvedValue(trajectoryEventPage());
    vi.mocked(loadTrajectoryEventPage).mockResolvedValueOnce({ events: [child], previous_cursor: null, next_cursor: "more", total_events: 2 })
      .mockReturnValueOnce(remaining.promise);
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, root.session_key);
    act(() => result.current.toggleEventExpanded("trajectory.v1.turn-1"));
    await waitFor(() => expect(loadTrajectoryEventPage).toHaveBeenCalledTimes(2));
    expect(result.current.trajectoryPages.get(root.session_key)?.get("trajectory.v1.turn-1")).toMatchObject({ events: [], has_loaded: false });
    const last = { ...child, event_key: "last" };
    await act(async () => remaining.resolve({ events: [last], previous_cursor: "before", next_cursor: null, total_events: 2 }));
    await waitFor(() => expect(result.current.trajectoryPages.get(root.session_key)?.get("trajectory.v1.turn-1")).toMatchObject({
      events: [child, last], has_loaded: true,
    }));
  });

});

describe("useViewerState subagent discovery", () => {
  it("loads direct child metadata lazily and lets a child own the event timeline", async () => {
    const root = session("codex:root");
    root.child_count = 1;
    const child = session("codex:child");
    child.parent_session_id = root.session_id;
    child.is_subagent = true;
    child.agent_nickname = "Hubble";
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [root],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(listSessionChildren).mockResolvedValue({
      sessions: [child],
      next_cursor: null,
    });

    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, root.session_key);
    await waitFor(() => expect(result.current.selectedSession?.session_key).toBe(root.session_key));
    expect(listSessionChildren).not.toHaveBeenCalled();

    act(() => result.current.loadSessionChildren(root.session_key));
    await waitFor(() => {
      expect(result.current.sessionChildren.get(root.session_key)?.sessions).toEqual([child]);
    });
    expect(listSessionChildren).toHaveBeenCalledWith({
      parent_session_key: root.session_key,
      cursor: undefined,
      limit: 60,
    });

    act(() => result.current.selectSession(child.session_key));
    await waitFor(() => expect(result.current.selectedSession?.session_key).toBe(child.session_key));
  });

  it("opens a delegation child before its lazy sidebar page arrives", async () => {
    const root = session("codex:delegating-root");
    root.child_count = 1;
    const child = session("codex:delegated-child");
    child.parent_session_id = root.session_id;
    child.is_subagent = true;
    child.agent_nickname = "Hubble";
    const childPage = deferred<{ sessions: SessionSummary[]; next_cursor: string | null }>();
    vi.mocked(listSessions).mockResolvedValue({
      sessions: [root],
      next_cursor: null,
      source_errors: [],
      pending_providers: [],
    });
    vi.mocked(loadEventPage).mockResolvedValue(toolEventPage());
    vi.mocked(listSessionChildren).mockImplementation(() => childPage.promise);

    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, root.session_key);
    await waitFor(() => expect(result.current.selectedSession?.session_key).toBe(root.session_key));

    act(() => result.current.openRelatedSession(root.session_key, child));

    await waitFor(() => {
      expect(result.current.selectedSession?.session_key).toBe(child.session_key);
      expect(result.current.sessionChildren.get(root.session_key)?.sessions).toEqual([child]);
    });
    expect(listSessionChildren).toHaveBeenCalledWith({
      parent_session_key: root.session_key,
      cursor: undefined,
      limit: 60,
    });

    await act(async () => {
      childPage.resolve({ sessions: [], next_cursor: null });
      await childPage.promise;
    });

    await waitFor(() => {
      expect(result.current.sessionChildren.get(root.session_key)?.sessions).toEqual([child]);
    });
  });
});

function semanticSnapshot(request: import("./types").SessionUpdatesRequest, page: EventPageResponse): import("./types").SessionUpdate {
  const { events, ...state } = page;
  return { subscription_id: request.subscription_id, session_key: request.session_key, level: request.level,
    generation: "fixture", base_revision: null, revision: "1", snapshot: true,
    items: events.map((summary) => ({ item_id: summary.event_key, kind: "tool_summary", level: "steps", summary })),
    groups: [], removed_items: [], item_order: events.map((event) => event.event_key), state };
}

describe("semantic session delivery", () => {
  it("accepts pushed items without reloading the retained window", async () => {
    let emit: ((update: import("./types").SessionUpdate) => void) | undefined;
    vi.mocked(listenForSessionUpdates).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const page = toolEventPage();
    vi.mocked(loadSessionUpdates).mockImplementation(async (request) => semanticSnapshot(request, request.level === "all" ? page : { ...page, events: [], total_events: 0 }));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toEqual(page.events));
    const request = vi.mocked(loadSessionUpdates).mock.calls.find(([request]) => request.level === "all")![0];
    const update = semanticSnapshot(request, page);
    act(() => emit?.({ ...update, snapshot: false, base_revision: "1", revision: "2", item_order: null,
      items: [{ ...update.items[0], summary: { ...page.events[0], summary: "pushed progress" } }] }));
    await waitFor(() => expect(result.current.events[0].summary).toBe("pushed progress"));
    expect(loadEventPage).not.toHaveBeenCalled();
    expect(vi.mocked(loadSessionUpdates).mock.calls.filter(([request]) => request.level === "all")).toHaveLength(1);
  });

  it("expands and refreshes work children from all delivery without child RPCs", async () => {
    let emit: ((update: import("./types").SessionUpdate) => void) | undefined;
    vi.mocked(listenForSessionUpdates).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const page = trajectoryEventPage();
    const child = trajectoryChildPage().events[0];
    page.events[0] = { ...page.events[0], child_keys: [child.event_key] };
    const makeUpdate = (request: import("./types").SessionUpdatesRequest): import("./types").SessionUpdate => ({ ...semanticSnapshot(request, page),
      items: [{ item_id: child.event_key, kind: "tool_summary", level: "steps", summary: child }],
      groups: [{ item_id: page.events[0].event_key, kind: "work_summary", level: "steps", summary: page.events[0] }] });
    vi.mocked(loadSessionUpdates).mockImplementation(async (request) => makeUpdate(request));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toEqual(page.events));
    act(() => result.current.toggleEventExpanded(page.events[0].event_key));
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(page.events[0].event_key)?.events).toEqual([child]));
    const request = vi.mocked(loadSessionUpdates).mock.calls.find(([request]) => request.level === "all")![0];
    act(() => emit?.({ ...makeUpdate(request), snapshot: false, base_revision: "1", revision: "2", item_order: null, groups: [],
      items: [{ item_id: child.event_key, kind: "tool_summary", level: "steps", summary: { ...child, summary: "cached progress" } }] }));
    await waitFor(() => expect(result.current.trajectoryPages.get("live")?.get(page.events[0].event_key)?.events[0].summary).toBe("cached progress"));
    expect(loadTrajectoryEventPage).not.toHaveBeenCalled();
    expect(loadEventPage).not.toHaveBeenCalled();
  });

  it.each([true, false])("uses compact attention only when notification delivery is available (%s)", async (available) => {
    if (!available) vi.mocked(listenForSessionNotifications).mockRejectedValue(new Error("unavailable"));
    let indexChange: ((change: SessionIndexChangedEvent) => void) | undefined;
    vi.mocked(listenForSessionIndexChanges).mockImplementation((handler) => { indexChange = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const page = toolEventPage();
    vi.mocked(loadSessionUpdates).mockImplementation(async (request) => semanticSnapshot(request, page));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toEqual(page.events));
    const catalogCalls = vi.mocked(listSessions).mock.calls.length;
    const updateCalls = vi.mocked(loadSessionUpdates).mock.calls.length;
    act(() => indexChange?.({ changed: true, catalog_refresh_required: false,
      attention_session_keys: ["live"], updated_session_keys: ["live"] }));
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 30)); });
    expect(listSessions).toHaveBeenCalledTimes(catalogCalls + (available ? 0 : 1));
    expect(loadSessionUpdates).toHaveBeenCalledTimes(updateCalls);
    expect(loadEventPage).not.toHaveBeenCalled();
  });

  it("renders cached content immediately while reopening catches up", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("one"), session("two")], next_cursor: null, source_errors: [], pending_providers: [] });
    const page = toolEventPage();
    const catchup = deferred<import("./types").SessionUpdate>();
    let firstRequests = 0;
    vi.mocked(loadSessionUpdates).mockImplementation(async (request) => {
      if (request.level === "all" && !request.unsubscribe && request.session_key === "one" && ++firstRequests > 1) return catchup.promise;
      return semanticSnapshot(request, request.level === "all" && !request.unsubscribe
        ? { ...page, events: [{ ...page.events[0], summary: request.session_key }] } : { ...page, events: [], total_events: 0 });
    });
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "one"); await waitFor(() => expect(result.current.events[0].summary).toBe("one"));
    act(() => result.current.selectSession("two")); await waitFor(() => expect(result.current.events[0].summary).toBe("two"));
    act(() => result.current.selectSession("one"));
    expect(result.current.events[0].summary).toBe("one");
    expect(loadEventPage).not.toHaveBeenCalled();
  });
  it("uses delivered all-level details and refreshes Inspector on detail-only pushes", async () => {
    let emit: ((update: import("./types").SessionUpdate) => void) | undefined;
    vi.mocked(listenForSessionUpdates).mockImplementation((handler) => { emit = handler; return Promise.resolve(vi.fn()); });
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("live")], next_cursor: null, source_errors: [], pending_providers: [] });
    const page = toolEventPage();
    vi.mocked(loadSessionUpdates).mockImplementation(async (request) => ({ ...semanticSnapshot(request, page),
      items: [...semanticSnapshot(request, page).items, { item_id: `detail:${page.events[0].event_key}`, kind: "detail", level: "details", event_key: page.events[0].event_key, detail: toolDetail("delivered") }] }));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "live");
    await waitFor(() => expect(result.current.events).toEqual(page.events));
    act(() => result.current.selectEvent(page.events[0].event_key));
    await waitFor(() => expect(result.current.detail?.tool_output?.sections[0].text).toBe("delivered"));
    const request = vi.mocked(loadSessionUpdates).mock.calls.find(([request]) => request.level === "all")![0];
    act(() => emit?.({ ...semanticSnapshot(request, page), snapshot: false, base_revision: "1", revision: "2", item_order: null,
      items: [{ item_id: `detail:${page.events[0].event_key}`, kind: "detail", level: "details", event_key: page.events[0].event_key, detail: toolDetail("pushed") }] }));
    await waitFor(() => expect(result.current.detail?.tool_output?.sections[0].text).toBe("pushed"));
    expect(loadEventDetail).not.toHaveBeenCalled();
    expect(vi.mocked(loadSessionUpdates).mock.calls.some(([request]) => request.level === "details")).toBe(false);
  });

  it("downgrades the previous session to final and opens the next at all", async () => {
    vi.mocked(listSessions).mockResolvedValue({ sessions: [session("one"), session("two")], next_cursor: null, source_errors: [], pending_providers: [] });
    vi.mocked(loadSessionUpdates).mockImplementation(async (request) => semanticSnapshot(request, request.level === "all" && !request.unsubscribe
      ? toolEventPage() : { ...toolEventPage(), events: [], total_events: 0 }));
    const { result } = renderHook(() => useViewerState());
    await selectListedSession(result, "one");
    await waitFor(() => expect(result.current.events).toEqual(toolEventPage().events));
    act(() => result.current.selectSession("two"));
    await waitFor(() => expect(vi.mocked(loadSessionUpdates).mock.calls.some(([request]) => request.session_key === "two" && request.level === "all")).toBe(true));
    const requests = vi.mocked(loadSessionUpdates).mock.calls.map(([request]) => request);
    expect(requests.some((request) => request.session_key === "one" && request.level === "all" && request.unsubscribe)).toBe(true);
    expect(requests.some((request) => request.session_key === "one" && request.level === "final" && !request.unsubscribe)).toBe(true);
  });

});
