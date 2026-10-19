import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useProjectWorkspace } from "./useProjectWorkspace";
import type { CommandInvoker, EventSubscriber } from "./transport";
import type { ManagedSessionSnapshot, RegisteredProject, SessionSummary } from "./types";

afterEach(cleanup);
const project: RegisteredProject = { project_id: "project", group_key: "/repo", folder_path: "/repo", name: "Repo", created_at_ms: 1, available: true };
const summary: SessionSummary = {
  session_key: "pi:real", session_id: "real", parent_session_id: null, is_subagent: false, provider: "pi", title: "New task", preview: "Hello",
  project: "Repo", cwd: "/repo", updated_at_ms: 1, timestamp: null, agent_path: null, agent_nickname: null, agent_role: null,
  child_count: 0, message_count: null, event_count: null, history_status: null, has_unread: false,
};
function state(request_id: string, overrides: Partial<ManagedSessionSnapshot> = {}): ManagedSessionSnapshot {
  return { revision: 1, request_id, project_id: project.project_id, provider: "pi", status: "starting", session_id: "real", session_key: "pi:real", session: null, message: null, pending_dialog: null, ...overrides };
}
function deferred<T>() { let resolve!: (value: T) => void; let reject!: (error: Error) => void; const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
function connection() {
  const handlers = new Map<string, (event: { payload: ManagedSessionSnapshot }) => void>();
  const stop = vi.fn();
  const invoke = vi.fn(async (command: string, payload?: Record<string, unknown>): Promise<unknown> => {
    if (command === "list_registered_projects") return { available: true, unavailable_reason: null, projects: [project] };
    if (command === "get_managed_session") return null;
    if (command === "create_project_session") return state((payload?.request as { request_id: string }).request_id);
    if (command === "stop_managed_session") return state((payload?.request as { request_id: string }).request_id, { status: "stopped" });
    throw new Error(`Unexpected command ${command}`);
  });
  const listen = vi.fn(async (name: string, callback: (event: { payload: ManagedSessionSnapshot }) => void) => { handlers.set(name, callback); return stop; });
  return { invoke, stop, transport: { invoke: invoke as CommandInvoker, listen: listen as EventSubscriber }, emit: (snapshot: ManagedSessionSnapshot) => handlers.get("managed-session-changed")!({ payload: snapshot }),
    registryChanged: () => handlers.get("registered-projects-changed")!({ payload: null as never }) };
}
const options = (on_created = vi.fn()) => ({ enabled: true, machine_key: "local", selected_session_key: null as string | null, on_created });

describe("local project workspace", () => {
  it("refreshes registrations from another window and on focus", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.projects).toHaveLength(1));
    host.invoke.mockImplementation((command, payload) => command === "list_registered_projects"
      ? Promise.resolve({ available: true, unavailable_reason: null, projects: [project, { ...project, project_id: "second", folder_path: "/trees/repo" }] })
      : original(command, payload));
    act(() => host.registryChanged());
    await waitFor(() => expect(result.current.workspace?.projects).toHaveLength(2));
    const before = host.invoke.mock.calls.length;
    act(() => window.dispatchEvent(new Event("focus")));
    await waitFor(() => expect(host.invoke.mock.calls.length).toBeGreaterThan(before));
  });
  it("does not invoke or subscribe through a remote or browser workspace", async () => {
    const host = connection();
    const { result } = renderHook(() => useProjectWorkspace({ ...options(), enabled: false }, host.transport));
    await act(async () => { await result.current.createSession("project", "Hello"); });
    expect(host.invoke).not.toHaveBeenCalled(); expect(host.transport.listen).not.toHaveBeenCalled();
  });

  it("deduplicates same-frame starts and waits for the indexed real identity", async () => {
    const host = connection(); const created = vi.fn(); const start = deferred<ManagedSessionSnapshot>();
    const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" ? start.promise : original(command, payload));
    const { result } = renderHook(() => useProjectWorkspace(options(created), host.transport));
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    let pending!: Promise<void>;
    act(() => { pending = result.current.createSession("project", "Hello"); void result.current.createSession("project", "Hello"); });
    const calls = host.invoke.mock.calls.filter(([command]) => command === "create_project_session");
    expect(calls).toHaveLength(1);
    const id = (calls[0][1]?.request as { request_id: string }).request_id;
    await act(async () => { start.resolve(state(id)); await pending; });
    expect(created).not.toHaveBeenCalled();
    host.invoke.mockImplementation((command, payload) => command === "get_managed_session" ? Promise.resolve(state(id, { revision: 2, session: summary, status: "running" })) : original(command, payload));
    act(() => host.emit(state(id, { revision: 2, status: "running" })));
    await waitFor(() => expect(created).toHaveBeenCalledExactlyOnceWith(summary));
    act(() => host.emit(state(id, { revision: 1, status: "starting" })));
    expect(result.current.creation?.status).toBe("running");
  });

  it("coalesces slow identity reads across polling ticks and managed status events", async () => {
    const host = connection(); const created = vi.fn(); const read = deferred<ManagedSessionSnapshot>();
    const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "get_managed_session" ? read.promise : original(command, payload));
    const { result, unmount } = renderHook(() => useProjectWorkspace(options(created), host.transport));
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    vi.useFakeTimers();
    try {
      let starting!: Promise<void>;
      await act(async () => { starting = result.current.createSession("project", "Hello"); });
      const id = result.current.creation!.request_id;
      const reads = () => host.invoke.mock.calls.filter(([command]) => command === "get_managed_session");
      expect(reads()).toHaveLength(1);
      act(() => host.emit(state(id, { revision: 2, status: "running" })));
      await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
      expect(reads()).toHaveLength(1);
      expect(created).not.toHaveBeenCalled();
      await act(async () => { read.resolve(state(id, { revision: 3, status: "running", session: summary })); await starting; });
      expect(created).toHaveBeenCalledExactlyOnceWith(summary);
      await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
      expect(reads()).toHaveLength(1);
    } finally { unmount(); vi.useRealTimers(); }
  });

  it("retains the request identity after an unconfirmed start", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    let starts = 0;
    host.invoke.mockImplementation((command, payload) => {
      if (command === "create_project_session" && starts++ === 0) return Promise.reject(new Error("bridge timeout"));
      return original(command, payload);
    });
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    await act(async () => { await result.current.createSession("project", "Original prompt"); });
    expect(result.current.creation_error).toContain("not confirmed");
    await act(async () => { await result.current.createSession("project", "Original prompt"); });
    const requests = host.invoke.mock.calls.filter(([command]) => command === "create_project_session").map(([, payload]) => payload?.request);
    expect(requests).toHaveLength(2); expect(requests[1]).toEqual(requests[0]);
  });

  it("allows correcting a request after explicit confirmation that no session started", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    let starts = 0;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" && starts++ === 0
      ? Promise.reject({ message: "The project folder disappeared", may_have_started: false })
      : original(command, payload));
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    await act(async () => { await result.current.createSession("old-project", "Keep this prompt"); });
    expect(result.current.creation_error).toBe("The project folder disappeared");
    expect(result.current.creation_uncertain).toBe(false);
    await act(async () => { await result.current.createSession("correct-folder", "Keep this prompt"); });
    const requests = host.invoke.mock.calls.filter(([command]) => command === "create_project_session").map(([, payload]) => payload?.request as { request_id: string; project_id: string; prompt: string });
    expect(requests).toHaveLength(2);
    expect(requests[1].request_id).not.toBe(requests[0].request_id);
    expect(requests[1]).toMatchObject({ project_id: "correct-folder", prompt: "Keep this prompt" });
  });

  it("checks an uncertain receipt before resending creation", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session"
      ? Promise.reject(new Error("Lost acknowledgement")) : original(command, payload));
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    await act(async () => { await result.current.createSession("project", "Original"); });
    const id = (host.invoke.mock.calls.find(([command]) => command === "create_project_session")![1]?.request as { request_id: string }).request_id;
    host.invoke.mockImplementation((command, payload) => command === "get_managed_session" ? Promise.resolve(state(id)) : original(command, payload));
    await act(async () => { await result.current.createSession("project", "Original"); });
    expect(host.invoke.mock.calls.filter(([command]) => command === "create_project_session")).toHaveLength(1);
    expect(result.current.creation?.request_id).toBe(id);
  });

  it("stops a delayed start on its original machine and never selects it on the successor", async () => {
    const old = connection(); const next = connection(); const start = deferred<ManagedSessionSnapshot>(); const created = vi.fn();
    const original = old.invoke.getMockImplementation()!;
    old.invoke.mockImplementation((command, payload) => command === "create_project_session" ? start.promise : original(command, payload));
    const { result, rerender } = renderHook(({ transport, machine_key }) => useProjectWorkspace({ ...options(created), machine_key }, transport),
      { initialProps: { transport: old.transport, machine_key: "old" } });
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    let pending!: Promise<void>;
    act(() => { pending = result.current.createSession("project", "Hello"); });
    const id = (old.invoke.mock.calls.find(([command]) => command === "create_project_session")![1]?.request as { request_id: string }).request_id;
    rerender({ transport: next.transport, machine_key: "next" });
    await act(async () => { start.resolve(state(id, { session: summary })); await pending; });
    expect(created).not.toHaveBeenCalled();
    expect(old.invoke).toHaveBeenCalledWith("stop_managed_session", { request: { request_id: id } });
    expect(next.invoke.mock.calls.some(([command]) => command === "stop_managed_session")).toBe(false);
  });

  it("does not steal selection after the user opens another conversation", async () => {
    const host = connection(); const created = vi.fn();
    const { result, rerender, unmount } = renderHook(({ selected }) => useProjectWorkspace({ ...options(created), selected_session_key: selected }, host.transport), { initialProps: { selected: null as string | null } });
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    await act(async () => { await result.current.createSession("project", "Hello"); });
    const id = result.current.creation!.request_id;
    rerender({ selected: "codex:another" });
    act(() => host.emit(state(id, { revision: 2, session: summary })));
    expect(created).not.toHaveBeenCalled();
    act(() => result.current.openCreation());
    expect(created).toHaveBeenCalledExactlyOnceWith(summary);
    act(() => result.current.beginCreation());
    expect(result.current.creation).toBeNull();
    unmount();
    expect(host.invoke.mock.calls.some(([command]) => command === "stop_managed_session")).toBe(false);
  });

  it("never sends an automatic dialog answer", async () => {
    const host = connection();
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    await act(async () => { await result.current.createSession("project", "Hello"); });
    act(() => host.emit(state(result.current.creation!.request_id, { revision: 2, pending_dialog: { dialog_id: "permission", kind: "confirm", title: "Allow tool?", message: null, options: [], initial_value: null } })));
    expect(host.invoke.mock.calls.some(([command]) => command === "respond_managed_session_dialog")).toBe(false);
  });

  it("surfaces an owned Pi dialog even when its conversation is not selected", async () => {
    const host = connection();
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    const dialog = { dialog_id: "background", kind: "input", title: "Pi needs a value", message: null, options: [], initial_value: null };
    act(() => host.emit(state("another-owned-session", { revision: 2, pending_dialog: dialog })));
    expect(result.current.pending_dialog_session?.request_id).toBe("another-owned-session");
    expect(result.current.pending_dialog_session?.pending_dialog).toEqual(dialog);
    act(() => host.emit(state("another-owned-session", { revision: 3, pending_dialog: null })));
    expect(result.current.pending_dialog_session).toBeNull();
  });
});
