import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useProjectWorkspace } from "./useProjectWorkspace";
import type { CommandInvoker, EventSubscriber } from "./transport";
import type { ManagedSessionSnapshot, RegisteredProject, SessionCreationOptions, SessionSummary } from "./types";

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
    registryChanged: () => handlers.get("registered-projects-changed")!({ payload: null as never }),
    runtimesChanged: () => handlers.get("rpc-runtimes-changed")!({ payload: null as never }) };
}
const options = () => ({ enabled: true, machine_key: "local", selected_session_key: null as string | null });

describe("local project workspace", () => {
  it("invalidates draft creation options when runtime profiles change or settings closes", async () => {
    const host = connection();
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    const initial = result.current.loadCreationOptions;
    await act(async () => { host.runtimesChanged(); });
    expect(result.current.loadCreationOptions).not.toBe(initial);
    const after_event = result.current.loadCreationOptions;
    act(() => result.current.refreshCreationOptions());
    expect(result.current.loadCreationOptions).not.toBe(after_event);
    await act(() => result.current.loadCreationOptions("project"));
    expect(host.invoke).toHaveBeenCalledWith("get_session_creation_options", { request: { project_id: "project" } });
    expect(host.invoke.mock.calls.some(([command]) => command === "create_project_session")).toBe(false);
  });

  it("refreshes automatic runtime availability on focus without requiring a saved-profile event", async () => {
    const host = connection();
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace?.available).toBe(true));
    const before = result.current.loadCreationOptions;
    await act(async () => { window.dispatchEvent(new Event("focus")); window.dispatchEvent(new Event("focus")); });
    expect(result.current.loadCreationOptions).not.toBe(before);
    await act(() => result.current.loadCreationOptions("project"));
    expect(host.invoke).toHaveBeenCalledWith("get_session_creation_options", { request: { project_id: "project" } });
  });

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
    renderHook(() => useProjectWorkspace({ ...options(), enabled: false }, host.transport));
    await act(async () => {});
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

  it.each(["stop", "dialog"] as const)("reports %s errors for a pending draft without a selected native session", async (action) => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "stop_managed_session" || command === "respond_managed_session_dialog"
      ? Promise.reject(new Error("Connector unavailable")) : original(command, payload));
    const { result } = renderHook(() => useProjectWorkspace(options(), host.transport));
    await waitFor(() => expect(result.current.workspace).not.toBeNull());
    await act(async () => {
      if (action === "stop") await result.current.control("stop", state("pending-draft"));
      else await result.current.respond({ request_id: "pending-draft", dialog_id: "question", cancelled: false, value: "Answer" });
    });
    expect(result.current.controlErrorFor("pending-draft")).toBe("Connector unavailable");
    expect(result.current.controlErrorFor("another-draft")).toBeNull();
    expect(result.current.control_error).toBeNull();
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
