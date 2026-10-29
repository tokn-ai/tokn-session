import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSessionDrafts } from "./useSessionDrafts";
import type { SessionDraftModel } from "./useSessionCreationModels";
import type { CommandInvoker, EventSubscriber } from "./transport";
import type { CreateProjectSessionRequest, ManagedSessionSnapshot, SessionSummary } from "./types";

const summary: SessionSummary = {
  session_key: "pi:real", session_id: "real", parent_session_id: null, is_subagent: false, provider: "pi", title: "New task", preview: "Hello",
  project: "Repo", cwd: "/repo", updated_at_ms: 1, timestamp: null, agent_path: null, agent_nickname: null, agent_role: null,
  child_count: 0, message_count: null, event_count: null, history_status: null, has_unread: false,
};
function state(request_id: string, overrides: Partial<ManagedSessionSnapshot> = {}): ManagedSessionSnapshot {
  return { revision: 1, request_id, project_id: "project", provider: "pi", status: "starting", session_id: "real", session_key: "pi:real", session: null, message: null, pending_dialog: null, ...overrides };
}
function deferred<T>() { let resolve!: (value: T) => void; let reject!: (error: unknown) => void; const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
function connection() {
  let callback: (event: { payload: ManagedSessionSnapshot }) => void = () => {};
  const stop = vi.fn();
  const invoke = vi.fn(async (command: string, payload?: Record<string, unknown>): Promise<unknown> => {
    if (command === "get_managed_session") return null;
    if (command === "release_image_upload") return undefined;
    if (command === "create_project_session") return state((payload!.request as CreateProjectSessionRequest).request_id);
    if (command === "upload_image_chunk") {
      const request = payload!.request as { upload_id: string; total_bytes: number };
      return { upload_id: request.upload_id, received_bytes: request.total_bytes, image_id: "saved-image" };
    }
    throw new Error(`Unexpected command ${command}`);
  });
  const listen = vi.fn(async (_name: string, handler: typeof callback) => { callback = handler; return stop; });
  return { invoke, stop, transport: { invoke: invoke as CommandInvoker, listen: listen as EventSubscriber }, emit: (snapshot: ManagedSessionSnapshot) => callback({ payload: snapshot }) };
}
const options = (on_created = vi.fn()) => ({ enabled: true, machine_key: "local", on_created });
const image = () => new File(["image"], "screen.png", { type: "image/png" });
const creates = (host: ReturnType<typeof connection>) => host.invoke.mock.calls.filter(([command]) => command === "create_project_session");
const request = (host: ReturnType<typeof connection>, index = 0) => creates(host)[index][1]!.request as CreateProjectSessionRequest;

beforeEach(() => {
  vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: vi.fn(() => "blob:draft"), revokeObjectURL: vi.fn() }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals(); });

describe("app-only session drafts", () => {
  const selected_model: SessionDraftModel = { choice: { model: { model_provider: "openai", model_id: "chosen", display_name: "Chosen" }, supports_images: true },
    catalog_revision: "catalog", runtime_profile_id: "rpc", runtime_revision: 7, scope_key: "verified-scope" };

  it("stores independent draft models and clears only model choices when project/provider/runtime scope changes", () => {
    const host = connection(); const { result } = renderHook(() => useSessionDrafts(options(), host.transport));
    let first!: string; let second!: string;
    act(() => { first = result.current.createDraft("project"); second = result.current.createDraft("other"); });
    act(() => { result.current.updateDraft(first, { model: selected_model, prompt: "Keep text" }); result.current.updateDraft(second, { model: selected_model }); });
    act(() => result.current.updateDraft(first, { selection: { provider: "codex", runtime_profile_id: "rpc", runtime_revision: 7 } }));
    expect(result.current.drafts[0]).toMatchObject({ model: null, prompt: "Keep text" });
    expect(result.current.drafts[1].model).toEqual(selected_model);
    act(() => result.current.updateDraft(first, { model: selected_model }));
    act(() => result.current.updateDraft(first, { selection: { provider: "codex", runtime_profile_id: "rpc", runtime_revision: 8 } }));
    expect(result.current.drafts[0].model).toBeNull();
    act(() => result.current.updateDraft(first, { model: selected_model }));
    act(() => result.current.updateDraft(first, { project_id: "elsewhere" }));
    expect(result.current.drafts[0]).toMatchObject({ model: null, prompt: "Keep text", selection: { provider: "codex" } });
    expect(host.invoke).not.toHaveBeenCalled();
  });

  it("freezes model proof at first Send and checks uncertain receipts without replaying or changing it", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" ? Promise.reject(new Error("Acknowledgement lost")) : original(command, payload));
    const { result } = renderHook(() => useSessionDrafts(options(), host.transport)); let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "First", model: selected_model }); });
    await act(() => result.current.sendDraft(id));
    expect(request(host).model).toEqual({ model_provider: "openai", model_id: "chosen", catalog_revision: "catalog", runtime_profile_id: "rpc", runtime_revision: 7 });
    act(() => result.current.updateDraft(id, { model: null }));
    await act(() => result.current.sendDraft(id));
    expect(creates(host)).toHaveLength(1);
    expect(result.current.drafts[0].model).toEqual(selected_model);
    expect(result.current.drafts[0].locked).toBe(true);
  });

  it("creates multiple editable drafts without invoking any native command", () => {
    const host = connection();
    const { result } = renderHook(() => useSessionDrafts(options(), host.transport));
    let first!: string; let second!: string;
    act(() => { first = result.current.createDraft("project"); second = result.current.createDraft("another"); });
    act(() => result.current.updateDraft(first, { prompt: "Kept text", selection: { provider: "codex", runtime_profile_id: "rpc", runtime_revision: 7 } }));
    expect(result.current.drafts.map((draft) => draft.draft_id)).toEqual([first, second]);
    expect(result.current.drafts[0]).toMatchObject({ prompt: "Kept text", locked: false, can_discard: true, snapshot: null });
    expect(host.invoke).not.toHaveBeenCalled();
  });

  it("does not create drafts, invoke or subscribe when disabled", () => {
    const host = connection();
    const { result } = renderHook(() => useSessionDrafts({ ...options(), enabled: false }, host.transport));
    act(() => expect(result.current.createDraft("project")).toBe(""));
    expect(result.current.drafts).toEqual([]); expect(host.invoke).not.toHaveBeenCalled(); expect(host.transport.listen).not.toHaveBeenCalled();
  });

  it.each(["pi", "codex", "open_code", "dsh"] as const)("freezes %s first-message selection and rejects same-frame duplicates", async (provider) => {
    const host = connection(); const start = deferred<ManagedSessionSnapshot>(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" ? start.promise : original(command, payload));
    const { result } = renderHook(() => useSessionDrafts(options(), host.transport));
    let id!: string; let pending!: Promise<void>;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "First", selection: { provider, runtime_profile_id: "rpc", runtime_revision: 7 } }); });
    act(() => { pending = result.current.sendDraft(id); void result.current.sendDraft(id); result.current.updateDraft(id, { prompt: "Changed" }); });
    expect(creates(host)).toHaveLength(1);
    expect(request(host)).toMatchObject({ project_id: "project", prompt: "First", provider, runtime_profile_id: "rpc", runtime_revision: 7 });
    expect(result.current.drafts[0]).toMatchObject({ prompt: "First", sending: true, locked: true });
    await act(async () => { start.resolve(state(request(host).request_id, { provider })); await pending; });
  });

  it("keeps blank text/image-free drafts local even when Send is called", async () => {
    const host = connection(); const { result } = renderHook(() => useSessionDrafts(options(), host.transport));
    let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "  " }); });
    await act(async () => result.current.sendDraft(id));
    expect(creates(host)).toHaveLength(0); expect(result.current.drafts[0].locked).toBe(false);
  });

  it("preserves other drafts and promotes only verified, accepted history once", async () => {
    const host = connection(); const created = vi.fn(); const { result } = renderHook(() => useSessionDrafts(options(created), host.transport));
    let first!: string; let second!: string;
    act(() => { first = result.current.createDraft("project"); second = result.current.createDraft("other"); result.current.updateDraft(first, { prompt: "First" }); });
    await act(async () => result.current.sendDraft(first));
    const id = request(host).request_id;
    act(() => host.emit(state(id, { revision: 2, session: summary, creation_input_state: "uncertain" })));
    expect(created).not.toHaveBeenCalled(); expect(result.current.drafts[0].prompt).toBe("First");
    act(() => host.emit(state(id, { revision: 3, session: summary, creation_input_state: "accepted" })));
    expect(created).toHaveBeenCalledExactlyOnceWith(first, summary);
    expect(result.current.drafts.map((draft) => draft.draft_id)).toEqual([second]);
    act(() => host.emit(state(id, { revision: 4, session: summary, creation_input_state: "accepted" })));
    expect(created).toHaveBeenCalledTimes(1);
  });

  it.each([null, "not_attempted", "uncertain", "rejected"] as const)("retains Pi first message when history exists but acceptance is %s", async (creation_input_state) => {
    const host = connection(); const created = vi.fn(); const { result } = renderHook(() => useSessionDrafts(options(created), host.transport));
    let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "Unconfirmed" }); result.current.addImages(id, [image()]); });
    await act(async () => result.current.sendDraft(id));
    act(() => host.emit(state(request(host).request_id, { revision: 2, session: summary, status: "idle", creation_input_state })));
    expect(created).not.toHaveBeenCalled(); expect(result.current.drafts[0]).toMatchObject({ prompt: "Unconfirmed", locked: true });
    expect(result.current.drafts[0].images).toHaveLength(1); expect(URL.revokeObjectURL).not.toHaveBeenCalled();
  });

  it("tracks simultaneous first messages independently", async () => {
    const host = connection(); const created = vi.fn(); const { result } = renderHook(() => useSessionDrafts(options(created), host.transport));
    let first!: string; let second!: string;
    act(() => {
      first = result.current.createDraft("project"); second = result.current.createDraft("other");
      result.current.updateDraft(first, { prompt: "First" }); result.current.updateDraft(second, { prompt: "Second" });
    });
    await act(async () => { await Promise.all([result.current.sendDraft(first), result.current.sendDraft(second)]); });
    expect(creates(host)).toHaveLength(2); expect(request(host).request_id).not.toBe(request(host, 1).request_id);
    act(() => host.emit(state(request(host, 1).request_id, { revision: 2, project_id: "other", session: summary, creation_input_state: "accepted" })));
    expect(created).toHaveBeenCalledExactlyOnceWith(second, summary);
    expect(result.current.drafts.map((draft) => draft.draft_id)).toEqual([first]);
    act(() => host.emit(state(request(host).request_id, { revision: 2, session: summary, creation_input_state: "accepted" })));
    expect(created).toHaveBeenLastCalledWith(first, summary); expect(result.current.drafts).toEqual([]);
  });

  it("ignores old revisions and preserves same-revision verified metadata", async () => {
    const host = connection(); const created = vi.fn(); const { result } = renderHook(() => useSessionDrafts(options(created), host.transport));
    let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "First" }); });
    await act(async () => result.current.sendDraft(id));
    const native = request(host).request_id;
    act(() => host.emit(state(native, { revision: 4, session: summary, creation_input_state: "uncertain" })));
    act(() => host.emit(state(native, { revision: 3, session: summary, creation_input_state: "accepted" })));
    expect(created).not.toHaveBeenCalled(); expect(result.current.drafts[0].snapshot?.revision).toBe(4);
    act(() => host.emit(state(native, { revision: 4, creation_input_state: "accepted" })));
    expect(created).toHaveBeenCalledExactlyOnceWith(id, summary);
  });

  it("never replays an uncertain create even when the status read returns no receipt", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" ? Promise.reject(new Error("Acknowledgement lost")) : original(command, payload));
    const { result } = renderHook(() => useSessionDrafts(options(), host.transport)); let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "First" }); });
    await act(async () => result.current.sendDraft(id));
    expect(result.current.drafts[0]).toMatchObject({ uncertain: true, locked: true, can_discard: false });
    act(() => { result.current.updateDraft(id, { prompt: "Changed", selection: { provider: "codex" } }); result.current.discardDraft(id); });
    await act(async () => result.current.sendDraft(id));
    await act(async () => result.current.checkDraft(id));
    expect(creates(host)).toHaveLength(1); expect(result.current.drafts[0].prompt).toBe("First");
    expect(result.current.drafts[0].error).toContain("first message will not be sent twice");
    const reads = host.invoke.mock.calls.filter(([command]) => command === "get_managed_session");
    expect(reads.every(([, payload]) => (payload!.request as { request_id: string }).request_id === request(host).request_id)).toBe(true);
  });

  it("unlocks definite pre-admission rejection and uses a fresh request for the corrected message", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!;
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" ? Promise.reject({ message: "Unavailable", may_have_started: false }) : original(command, payload));
    const { result } = renderHook(() => useSessionDrafts(options(), host.transport)); let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "First" }); });
    await act(async () => result.current.sendDraft(id));
    expect(result.current.drafts[0]).toMatchObject({ error: "Unavailable", locked: false, can_discard: true, uncertain: false });
    act(() => result.current.updateDraft(id, { prompt: "Corrected" }));
    await act(async () => result.current.sendDraft(id));
    expect(creates(host)).toHaveLength(2); expect(request(host, 1).request_id).not.toBe(request(host).request_id); expect(request(host, 1).prompt).toBe("Corrected");
  });

  it("does not unlock a newer live receipt while cleaning up a rejected acknowledgement", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!; const released = deferred<unknown>();
    host.invoke.mockImplementation((command, payload) => command === "create_project_session"
      ? Promise.reject({ message: "Rejected acknowledgement", may_have_started: false })
      : command === "release_image_upload" ? released.promise : original(command, payload));
    const { result } = renderHook(() => useSessionDrafts(options(), host.transport)); let id!: string; let pending!: Promise<void>;
    act(() => { id = result.current.createDraft("project"); result.current.addImages(id, [image()]); pending = result.current.sendDraft(id); });
    await waitFor(() => expect(host.invoke.mock.calls.some(([command]) => command === "release_image_upload")).toBe(true));
    act(() => host.emit(state(request(host).request_id, { revision: 2, creation_input_state: "uncertain" })));
    await act(async () => { released.resolve(undefined); await pending; });
    expect(result.current.drafts[0]).toMatchObject({ locked: true, uncertain: true, snapshot: { revision: 2 } });
    await act(async () => result.current.sendDraft(id));
    expect(creates(host)).toHaveLength(1);
  });

  it.each([
    { status: "failed", creation_phase: "failed", creation_input_state: "rejected" },
    { status: "idle", creation_phase: "ready", creation_input_state: "rejected", session: summary },
  ] satisfies Partial<ManagedSessionSnapshot>[])("allows discarding definitive failure without starting a replacement or deleting native history: %j", async (snapshot) => {
    const host = connection(); const { result } = renderHook(() => useSessionDrafts(options(), host.transport)); let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "First" }); });
    await act(async () => result.current.sendDraft(id));
    act(() => host.emit(state(request(host).request_id, { revision: 2, provider: "codex", ...snapshot })));
    expect(result.current.drafts[0]).toMatchObject({ locked: true, can_discard: true });
    act(() => result.current.discardDraft(id));
    expect(result.current.drafts).toEqual([]); expect(creates(host)).toHaveLength(1);
    expect(host.invoke.mock.calls.some(([command]) => /stop|delete/.test(command))).toBe(false);
  });

  it("uploads only on first Send, keeps image previews through uncertainty, and releases on accepted promotion", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!; const created = vi.fn();
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" ? Promise.reject(new Error("Acknowledgement lost")) : original(command, payload));
    const { result } = renderHook(() => useSessionDrafts(options(created), host.transport)); let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.addImages(id, [image()]); });
    expect(host.invoke).not.toHaveBeenCalled();
    await act(async () => result.current.sendDraft(id));
    expect(request(host)).toMatchObject({ prompt: "", image_ids: ["saved-image"] });
    await act(async () => result.current.checkDraft(id));
    expect(host.invoke.mock.calls.filter(([command]) => command === "upload_image_chunk")).toHaveLength(1);
    expect(host.invoke.mock.calls.filter(([command]) => command === "release_image_upload")).toHaveLength(0);
    expect(URL.revokeObjectURL).not.toHaveBeenCalled();
    act(() => host.emit(state(request(host).request_id, { revision: 2, session: summary, creation_input_state: "accepted" })));
    expect(host.invoke.mock.calls.filter(([command]) => command === "release_image_upload")).toHaveLength(1);
    expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:draft");
    expect(created).toHaveBeenCalledExactlyOnceWith(id, summary);
  });

  it("disposes unsent images on removal, discard and owner unmount", () => {
    const host = connection(); const { result, unmount } = renderHook(() => useSessionDrafts(options(), host.transport)); let first!: string; let second!: string;
    act(() => { first = result.current.createDraft("project"); second = result.current.createDraft("other"); result.current.addImages(first, [image(), image()]); result.current.addImages(second, [image()]); });
    act(() => result.current.removeImage(first, result.current.drafts[0].images[0].draft_id));
    act(() => result.current.discardDraft(first));
    expect(URL.revokeObjectURL).toHaveBeenCalledTimes(2);
    unmount(); expect(URL.revokeObjectURL).toHaveBeenCalledTimes(3); expect(host.invoke).not.toHaveBeenCalled();
  });

  it("coalesces slow status reads across live events and manual checks", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!; const read = deferred<ManagedSessionSnapshot | null>();
    const { result } = renderHook(() => useSessionDrafts(options(), host.transport)); let id!: string;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "First" }); });
    await act(async () => result.current.sendDraft(id));
    host.invoke.mockClear();
    host.invoke.mockImplementation((command, payload) => command === "get_managed_session" ? read.promise : original(command, payload));
    let checking!: Promise<void>;
    act(() => {
      host.emit(state(requestFromDraft(result.current.drafts[0]), { revision: 2 }));
      checking = result.current.checkDraft(id);
      host.emit(state(requestFromDraft(result.current.drafts[0]), { revision: 3 }));
    });
    await act(async () => {});
    expect(host.invoke.mock.calls.filter(([command]) => command === "get_managed_session")).toHaveLength(1);
    await act(async () => { read.resolve(null); await checking; });
  });

  it("fences delayed native completion after machine change without stopping the admitted session", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!; const start = deferred<ManagedSessionSnapshot>(); const created = vi.fn();
    host.invoke.mockImplementation((command, payload) => command === "create_project_session" ? start.promise : original(command, payload));
    const { result, rerender } = renderHook(({ machine_key }) => useSessionDrafts({ ...options(created), machine_key }, host.transport), { initialProps: { machine_key: "local" } });
    let id!: string; let pending!: Promise<void>;
    act(() => { id = result.current.createDraft("project"); result.current.updateDraft(id, { prompt: "First" }); pending = result.current.sendDraft(id); });
    rerender({ machine_key: "another" });
    await act(async () => { start.resolve(state(request(host).request_id, { session: summary, creation_input_state: "accepted" })); await pending; });
    expect(result.current.drafts).toEqual([]); expect(created).not.toHaveBeenCalled(); expect(host.invoke.mock.calls.some(([command]) => command === "stop_managed_session")).toBe(false);
  });

  it("cleans up a partial upload after unmount without creating a provider session", async () => {
    const host = connection(); const original = host.invoke.getMockImplementation()!; const chunk = deferred<unknown>();
    host.invoke.mockImplementation((command, payload) => command === "upload_image_chunk" ? chunk.promise : original(command, payload));
    const { result, unmount } = renderHook(() => useSessionDrafts(options(), host.transport)); let id!: string; let pending!: Promise<void>;
    act(() => { id = result.current.createDraft("project"); result.current.addImages(id, [image()]); pending = result.current.sendDraft(id); });
    await waitFor(() => expect(host.invoke.mock.calls.some(([command]) => command === "upload_image_chunk")).toBe(true));
    const upload = host.invoke.mock.calls.find(([command]) => command === "upload_image_chunk")![1]!.request as { upload_id: string; total_bytes: number };
    unmount();
    await act(async () => { chunk.resolve({ upload_id: upload.upload_id, received_bytes: upload.total_bytes, image_id: "unused" }); await pending; });
    expect(creates(host)).toHaveLength(0); expect(host.invoke.mock.calls.filter(([command]) => command === "release_image_upload")).toHaveLength(1);
    expect(URL.revokeObjectURL).toHaveBeenCalledTimes(1);
  });
});

function requestFromDraft(draft: { snapshot: ManagedSessionSnapshot | null }) { return draft.snapshot!.request_id; }
