import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useProjectWorkspace } from "./useProjectWorkspace";
import type { CommandInvoker, EventSubscriber } from "./transport";
import type { CreateProjectSessionRequest, ManagedSessionSnapshot, RegisteredProject, SessionCreationOptions, SessionCreationSelection, SessionSummary } from "./types";

afterEach(cleanup);
const project: RegisteredProject = { project_id: "project", group_key: "/repo", folder_path: "/repo", name: "Repo", created_at_ms: 1, available: true };
const creation_options: SessionCreationOptions = { project_id: "project", providers: [
  { provider: "pi", available: true, reason: null, supports_images: true, default_profile_id: null, runtimes: [] },
  { provider: "codex", available: true, reason: null, supports_images: true, default_profile_id: "rpc", runtimes: [
    { profile_id: "rpc", revision: 7, name: "Configured runtime", available: true, reason: null },
  ] },
] };
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
    if (command === "get_session_creation_options") return creation_options;
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
  it("loads creation choices through the selected desktop transport", async () => {
    const host = connection();
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    let loaded!: SessionCreationOptions;
    await act(async () => { loaded = await result.current.loadCreationOptions("project"); });
    expect(host.invoke).toHaveBeenCalledWith("get_session_creation_options", { request: { project_id: "project" } });
    expect(loaded).toEqual(creation_options);
    expect(host.invoke.mock.calls.some(([command]) => command === "create_project_session")).toBe(false);
  });

  it.each(["codex", "open_code", "dsh"] as const)("submits %s creation with its exact host runtime selection", async (provider) => {
    const host = connection();
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    await act(async () => { await result.current.createSession("project", "Native defaults", [], { provider, runtime_profile_id: "rpc", runtime_revision: 7 }); });
    expect(host.invoke).toHaveBeenCalledWith("create_project_session", { request: { request_id: expect.any(String), project_id: "project", prompt: "Native defaults", provider, runtime_profile_id: "rpc", runtime_revision: 7 } });
  });

  it.each([
    { provider: "open_code", runtime_profile_id: "rpc", runtime_revision: 7 },
    { provider: "codex", runtime_profile_id: "another-rpc", runtime_revision: 7 },
    { provider: "codex", runtime_profile_id: "rpc", runtime_revision: 8 },
    { provider: "codex" },
  ] satisfies SessionCreationSelection[])("rejects a changed provider or runtime fingerprint while preserving uncertain delivery: %j", async (changed) => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" ? Promise.reject(new Error("Acknowledgement lost")) : original(command, payload));
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    const initial = { provider: "codex", runtime_profile_id: "rpc", runtime_revision: 7 } as const;
    await act(async () => { await result.current.createSession("project", "Original", [], initial); });
    expect(result.current.creation_uncertain).toBe(true);
    await act(async () => { await result.current.createSession("project", "Original", [], changed); });
    expect(result.current.creation_error).toContain("Check the pending session");
    expect(result.current.creation_uncertain).toBe(true);
    const calls = host.invoke.mock.calls.filter(([command]) => command === "create_project_session");
    expect(calls).toHaveLength(1);
    const request = calls[0][1]!.request as CreateProjectSessionRequest;
    expect(request).toMatchObject(initial);
    host.invoke.mockImplementation((command, payload) => command === "get_managed_session" ? Promise.resolve(state(request.request_id, { provider: "codex", creation_phase: "native_create_uncertain" })) : original(command, payload));
    await act(async () => { await result.current.createSession("project", "Original", [], initial); });
    expect(result.current.creation?.request_id).toBe(request.request_id);
    expect(host.invoke.mock.calls.filter(([command]) => command === "create_project_session")).toHaveLength(1);
  });

  it("checks a failed RPC receipt on its original UUID and starts a new one only through explicit New session", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    let request_id: string | null = null;
    host.invoke.mockImplementation((command, payload) => {
      if (command === "create_project_session") {
        request_id = (payload!.request as CreateProjectSessionRequest).request_id;
        return Promise.resolve(state(request_id, { provider: "codex", status: "failed", creation_phase: "failed", message: "Native session exists; first input failed" }));
      }
      if (command === "get_managed_session" && request_id) return Promise.resolve(state(request_id, { provider: "codex", status: "failed", creation_phase: "failed" }));
      return original(command, payload);
    });
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    await act(async () => { await result.current.createSession("project", "Original", [], { provider: "codex" }); });
    const original_id = result.current.creation!.request_id;
    await act(async () => { await result.current.createSession("project", "Original", [], { provider: "codex" }); });
    expect(result.current.creation?.request_id).toBe(original_id);
    expect(host.invoke.mock.calls.filter(([command]) => command === "create_project_session")).toHaveLength(1);
    act(() => result.current.beginCreation());
    expect(result.current.creation).toBeNull();
    await act(async () => { await result.current.createSession("project", "New explicit task", [], { provider: "codex" }); });
    const calls = host.invoke.mock.calls.filter(([command]) => command === "create_project_session");
    expect(calls).toHaveLength(2);
    const next_request = calls[1][1]!.request as CreateProjectSessionRequest;
    expect(next_request.request_id).not.toBe(original_id);
    expect(next_request.prompt).toBe("New explicit task");
  });

  it.each(["native_create_uncertain", "first_input_uncertain", "history_pending"] as const)("keeps %s receipts pinned when New session is requested", async (creation_phase) => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    let request_id: string | null = null;
    host.invoke.mockImplementation((command, payload) => {
      if (command === "create_project_session") {
        request_id = (payload!.request as CreateProjectSessionRequest).request_id;
        return Promise.resolve(state(request_id, { provider: "codex", creation_phase }));
      }
      if (command === "get_managed_session" && request_id) return Promise.resolve(state(request_id, { provider: "codex", creation_phase }));
      return original(command, payload);
    });
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    await act(async () => { await result.current.createSession("project", "Original", [], { provider: "codex" }); });
    const pending = result.current.creation;
    act(() => result.current.beginCreation());
    expect(result.current.creation).toEqual(pending);
    await act(async () => { await result.current.createSession("project", "Original", [], { provider: "codex" }); });
    expect(result.current.creation?.request_id).toBe(pending!.request_id);
    expect(host.invoke.mock.calls.filter(([command]) => command === "create_project_session")).toHaveLength(1);
  });

  it("allows a fresh Pi retry after its owned process failed", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session"
      ? Promise.resolve(state((payload!.request as CreateProjectSessionRequest).request_id, { status: "failed" })) : original(command, payload));
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    await act(async () => { await result.current.createSession("project", "Original"); });
    const first_id = result.current.creation!.request_id;
    await act(async () => { await result.current.createSession("project", "Corrected"); });
    const calls = host.invoke.mock.calls.filter(([command]) => command === "create_project_session");
    expect(calls).toHaveLength(2);
    expect((calls[1][1]!.request as CreateProjectSessionRequest).request_id).not.toBe(first_id);
  });

  it("retains an accepted RPC receipt and original UUID when its status check fails", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    let request_id: string | null = null;
    let fail_read = false;
    host.invoke.mockImplementation((command, payload) => {
      if (command === "create_project_session") {
        request_id = (payload!.request as CreateProjectSessionRequest).request_id;
        return Promise.resolve(state(request_id, { provider: "codex", status: "failed", creation_phase: "failed", message: "First input failed after native creation" }));
      }
      if (command === "get_managed_session" && request_id) {
        if (fail_read) return Promise.reject(new Error("Runtime status unavailable"));
        return Promise.resolve(state(request_id, { provider: "codex", status: "failed", creation_phase: "failed", message: "First input failed after native creation" }));
      }
      return original(command, payload);
    });
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    await act(async () => { await result.current.createSession("project", "Original", [], { provider: "codex" }); });
    const original_receipt = result.current.creation;
    fail_read = true;
    await act(async () => { await result.current.createSession("project", "Original", [], { provider: "codex" }); });
    expect(result.current.creation).toEqual(original_receipt);
    expect(result.current.creation_uncertain).toBe(true);
    expect(result.current.creation_error).toContain("Runtime status unavailable");
    fail_read = false;
    await act(async () => { await result.current.createSession("project", "Original", [], { provider: "codex" }); });
    expect(result.current.creation?.request_id).toBe(original_receipt!.request_id);
    expect(host.invoke.mock.calls.filter(([command]) => command === "create_project_session")).toHaveLength(1);
  });
  it("refreshes discovered folder availability from registry events and on focus", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.projects).toHaveLength(1));
    const unavailable = { ...project, available: false, origin: "discovered", discovery_state: "absent", unavailable_reason: "No saved sessions reference this folder." };
    host.invoke.mockImplementation((command, payload) => command === "list_registered_projects"
      ? Promise.resolve({ available: true, unavailable_reason: null, projects: [unavailable, { ...project, project_id: "second", folder_path: "/trees/repo", origin: "discovered", discovery_state: "present" }] })
      : original(command, payload));
    act(() => host.registryChanged());
    await waitFor(() => expect(result.current.workspace?.projects).toHaveLength(2));
    expect(result.current.workspace?.projects[0]).toEqual(unavailable);
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

  it("refreshes selected owned input after idle cleanup or another window resumes", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "get_managed_session"
      ? Promise.resolve(state("owned", { status: "idle", session: summary })) : original(command, payload));
    const { result } = renderHook(() => useProjectWorkspace({ ...options(), selected_session_key: summary.session_key }, host.transport));
    await waitFor(() => expect(result.current.managed?.status).toBe("idle"));
    const initial = result.current.input_refresh_key;
    act(() => host.emit(state("owned", { revision: 2, status: "running", session: summary })));
    act(() => host.emit(state("owned", { revision: 3, status: "idle", session: summary })));
    expect(result.current.input_refresh_key).toBe(initial);
    act(() => host.emit(state("owned", { revision: 4, status: "stopped", session: summary })));
    expect(result.current.input_refresh_key).toBe(initial + 1);
    act(() => host.emit(state("owned", { revision: 3, status: "running", session: summary })));
    expect(result.current.managed?.status).toBe("stopped");
    expect(result.current.input_refresh_key).toBe(initial + 1);
    act(() => host.emit(state("owned", { revision: 5, status: "failed", session: summary })));
    expect(result.current.input_refresh_key).toBe(initial + 1);
    act(() => host.emit(state("owned", { revision: 6, status: "starting", session: summary })));
    expect(result.current.input_refresh_key).toBe(initial + 2);
    act(() => host.emit(state("owned", { revision: 7, status: "running", session: summary })));
    expect(result.current.input_refresh_key).toBe(initial + 2);
    act(() => host.emit(state("background", { revision: 2, session_key: "pi:other", session: { ...summary, session_key: "pi:other" }, status: "stopped" })));
    expect(result.current.input_refresh_key).toBe(initial + 2);
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
    act(() => result.current.beginCreation());
    expect(result.current.creation_uncertain).toBe(true);
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

it("uploads an image-only Pi prompt once and retains its IDs across an uncertain start", async () => {
  const host = connection(); const original = host.invoke.getMockImplementation()!;
  let creates = 0;
  host.invoke.mockImplementation(async (command, payload) => {
    if (command === "upload_image_chunk") {
      const request = payload?.request as { upload_id: string; total_bytes: number };
      return { upload_id: request.upload_id, received_bytes: request.total_bytes, image_id: "pi-image" };
    }
    if (command === "create_project_session") {
      creates += 1;
      if (creates === 1) throw new Error("Acknowledgement lost");
    }
    return original(command, payload);
  });
  const images = [{ draft_id: "draft", url: "blob:draft", file: new File(["image"], "screen.png", { type: "image/png" }) }];
  const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
  await waitFor(() => expect(result.current.workspace?.available).toBe(true));
  await act(async () => { await result.current.createSession("project", "", images); });
  expect(result.current.creation_uncertain).toBe(true);
  expect(host.invoke.mock.calls.filter(([command]) => command === "release_image_upload")).toHaveLength(0);
  await act(async () => { await result.current.createSession("project", "", images); });
  const requests = host.invoke.mock.calls.filter(([command]) => command === "create_project_session");
  expect(requests).toHaveLength(2);
  expect(requests[0][1]?.request).toEqual(requests[1][1]?.request);
  expect(requests[0][1]?.request).toMatchObject({ project_id: "project", prompt: "", image_ids: ["pi-image"] });
  expect(host.invoke.mock.calls.filter(([command]) => command === "upload_image_chunk")).toHaveLength(1);
  expect(host.invoke.mock.calls.filter(([command]) => command === "release_image_upload")).toHaveLength(1);
});

it("releases project staging tokens after a definite creation rejection", async () => {
  const host = connection(); const original = host.invoke.getMockImplementation()!;
  host.invoke.mockImplementation(async (command, payload) => {
    if (command === "upload_image_chunk") {
      const request = payload?.request as { upload_id: string; total_bytes: number };
      return { upload_id: request.upload_id, received_bytes: request.total_bytes, image_id: "pi-image" };
    }
    if (command === "release_image_upload") return undefined;
    if (command === "create_project_session") throw { message: "Model unavailable", may_have_started: false };
    return original(command, payload);
  });
  const images = [{ draft_id: "draft", url: "blob:draft", file: new File(["image"], "a.png", { type: "image/png" }) }];
  const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
  await waitFor(() => expect(result.current.workspace?.available).toBe(true));
  await act(async () => { await result.current.createSession("project", "", images); });
  expect(result.current.creation_uncertain).toBe(false);
  expect(result.current.creation_error).toBe("Model unavailable");
  const upload = host.invoke.mock.calls.find(([command]) => command === "upload_image_chunk")![1]!.request as { upload_id: string };
  expect(host.invoke).toHaveBeenCalledWith("release_image_upload", { request: { target: { type: "project", project_id: "project" }, upload_id: upload.upload_id } });
});

it("releases an abandoned project upload without starting a provider session", async () => {
  const host = connection(); const original = host.invoke.getMockImplementation()!;
  const chunk = deferred<{ upload_id: string; received_bytes: number; image_id: string }>();
  host.invoke.mockImplementation(async (command, payload) => {
    if (command === "upload_image_chunk") return chunk.promise;
    if (command === "release_image_upload") return undefined;
    return original(command, payload);
  });
  const images = [{ draft_id: "draft", url: "blob:draft", file: new File(["image"], "a.png", { type: "image/png" }) }];
  const { result, unmount } = renderHook(() => useProjectWorkspace(options(), host.transport));
  await waitFor(() => expect(result.current.workspace?.available).toBe(true));
  let pending!: Promise<void>;
  act(() => { pending = result.current.createSession("project", "", images); });
  await waitFor(() => expect(host.invoke.mock.calls.some(([command]) => command === "upload_image_chunk")).toBe(true));
  const request = host.invoke.mock.calls.find(([command]) => command === "upload_image_chunk")![1]!.request as { upload_id: string; total_bytes: number };
  unmount();
  expect(host.invoke).toHaveBeenCalledWith("release_image_upload", { request: { target: { type: "project", project_id: "project" }, upload_id: request.upload_id } });
  await act(async () => { chunk.resolve({ upload_id: request.upload_id, received_bytes: request.total_bytes, image_id: "unused" }); await pending; });
  expect(host.invoke.mock.calls.filter(([command]) => command === "create_project_session")).toHaveLength(0);
});
