import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { updateSessionView } from "./tauri";
import { captureTransport } from "./transport";
import { sessionViewCandidates, useSessionView } from "./useSessionView";
import type { SessionChildrenState, SessionSummary, SessionViewRequest } from "./types";

vi.mock("./tauri", () => ({ updateSessionView: vi.fn(() => Promise.resolve()) }));
vi.mock("./transport", () => ({ captureTransport: vi.fn() }));

const invoke = vi.fn();
const release = vi.fn(() => Promise.resolve());
let close: (() => void) | undefined;
beforeEach(() => {
  vi.useFakeTimers();
  vi.mocked(updateSessionView).mockReset().mockResolvedValue(undefined);
  release.mockReset().mockResolvedValue(undefined);
  close = undefined;
  vi.mocked(captureTransport).mockReturnValue({
    invoke, listen: vi.fn(), release, on_close: (handler) => { close = handler; return () => { close = undefined; }; },
  });
});
afterEach(() => { cleanup(); vi.useRealTimers(); });

function session(session_key: string, cwd = "/work/repo") {
  return { session_key, cwd, project: "repo" } as SessionSummary;
}
const children = new Map<string, SessionChildrenState>();

function sent(): SessionViewRequest[] {
  return vi.mocked(updateSessionView).mock.calls.map(([request]) => request);
}

describe("session view leases", () => {
  it("pins explicit selection, scopes candidates by full path, and heartbeats without rereading history", async () => {
    const first = session("a");
    const second = session("b");
    const other = session("other", "/elsewhere/repo");
    const { rerender } = renderHook(({ selected }) => useSessionView(selected.session_key, selected, [first, second, other], children), {
      initialProps: { selected: first },
    });
    await act(async () => {});
    expect(sent()[0]).toMatchObject({ session_key: "a", candidate_session_keys: ["a", "b"], revision: 1 });
    await act(async () => { rerender({ selected: second }); });
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(sent().map(({ session_key, revision }) => [session_key, revision])).toEqual([["a", 1], ["b", 2], ["b", 3]]);
    expect(new Set(sent().map(({ view_id }) => view_id)).size).toBe(1);
    expect(captureTransport).toHaveBeenCalledOnce();
    expect(updateSessionView).toHaveBeenLastCalledWith(expect.anything(), invoke);
  });

  it("creates a view identity on private-network HTTP without randomUUID", async () => {
    vi.stubGlobal("crypto", { getRandomValues: crypto.getRandomValues.bind(crypto) });
    try {
      renderHook(() => useSessionView(null, null, [], children));
      await act(async () => {});
      expect(sent()[0].view_id).toMatch(/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/);
    } finally { vi.unstubAllGlobals(); }
  });

  it("coalesces changing selection while a lease update is pending", async () => {
    let resolve!: () => void;
    vi.mocked(updateSessionView).mockReturnValueOnce(new Promise<void>((done) => { resolve = done; }));
    const sessions = [session("a"), session("b"), session("c")];
    const { rerender } = renderHook(({ selected }) => useSessionView(selected.session_key, selected, sessions, children), {
      initialProps: { selected: sessions[0] },
    });
    act(() => rerender({ selected: sessions[1] }));
    act(() => rerender({ selected: sessions[2] }));
    expect(updateSessionView).toHaveBeenCalledOnce();
    await act(async () => { resolve(); });
    expect(sent().map(({ session_key }) => session_key)).toEqual(["a", "c"]);
  });

  it("releases on disconnect with a later revision and cannot resurrect a stale update", async () => {
    let resolve!: () => void;
    vi.mocked(updateSessionView).mockReturnValueOnce(new Promise<void>((done) => { resolve = done; }));
    const selected = session("a");
    const { unmount, rerender } = renderHook(({ key }) => useSessionView(key, selected, [selected], children), { initialProps: { key: "a" } });
    act(() => rerender({ key: "b" }));
    act(() => close?.());
    expect(release).toHaveBeenCalledExactlyOnceWith("update_session_view", { request: {
      view_id: sent()[0].view_id, revision: 2, session_key: null, candidate_session_keys: [],
    } });
    await act(async () => { resolve(); await vi.advanceTimersByTimeAsync(60_000); });
    expect(updateSessionView).toHaveBeenCalledOnce();
    unmount();
    expect(release).toHaveBeenCalledOnce();
  });

  it("releases an unmounted view and uses a fresh identity for a new viewer", async () => {
    const first = renderHook(() => useSessionView("a", session("a"), [], children));
    await act(async () => {});
    first.unmount();
    renderHook(() => useSessionView("a", session("a"), [], children));
    await act(async () => {});
    expect(sent()[0].view_id).not.toBe(sent()[1].view_id);
    expect(release).toHaveBeenCalledWith("update_session_view", { request: expect.objectContaining({
      view_id: sent()[0].view_id, session_key: null, candidate_session_keys: [],
    }) });
  });

  it("bounds candidates, includes known children, and ignores unrelated child caches", () => {
    const root = session("root");
    const tree = new Map([
      ["root", { sessions: [session("child")] } as SessionChildrenState],
      ["unrelated", { sessions: [session("hidden-child")] } as SessionChildrenState],
    ]);
    expect(sessionViewCandidates(root, [root], tree)).toEqual(["root", "child"]);
    const many = Array.from({ length: 200 }, (_, i) => session(`session-${i}`));
    expect(sessionViewCandidates(null, many, tree)).toHaveLength(128);
  });
});
