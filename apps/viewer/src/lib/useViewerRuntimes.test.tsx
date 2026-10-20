import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { listViewerRuntimes, listenForViewerRuntimeChanges, restartViewerRuntime } from "./tauri";
import { useViewerRuntimes } from "./useViewerRuntimes";
import type { RuntimeInfo, RuntimeInventory, RuntimeRestartResponse } from "./types";

const { desktop, notify } = vi.hoisted(() => ({ desktop: { value: true }, notify: vi.fn() }));
vi.mock("./transport", () => ({ isDesktop: () => desktop.value }));
vi.mock("./tauri", () => ({ listViewerRuntimes: vi.fn(), listenForViewerRuntimeChanges: vi.fn(), restartViewerRuntime: vi.fn() }));
vi.mock("../components/ActionNotifications", () => ({ useActionNotification: () => notify }));
let changed: () => void;
const stop = vi.fn();
const row: RuntimeInfo = { runtime_id: "pi:owned", instance_id: "owned:1", name: "Pi · Hello", kind: "pi",
  execution_kind: "subprocess", ownership: "viewer", status: "idle", pid: 4321, started_at_ms: 1,
  session_key: "saved", session_id: "session", project_id: "project", working_directory: "/project/aa", endpoint: null,
  detail: null, last_error: null, can_restart: true, restart_disabled_reason: null };
function inventory(runtime = row): RuntimeInventory { return { version: 1, runtimes: [runtime] }; }
function deferred<T>() { let resolve!: (value: T) => void; let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
beforeEach(() => {
  desktop.value = true; vi.resetAllMocks();
  vi.mocked(listViewerRuntimes).mockResolvedValue(inventory());
  vi.mocked(listenForViewerRuntimeChanges).mockImplementation(async (handler) => { changed = handler; return stop; });
  vi.mocked(restartViewerRuntime).mockImplementation(async (request) => ({ ...request, message: "Restarted" }));
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); });

it("observes only while the section is active and subscribes before loading", async () => {
  const { result, rerender } = renderHook(({ active }) => useViewerRuntimes(active), { initialProps: { active: false } });
  expect(listViewerRuntimes).not.toHaveBeenCalled();
  expect(listenForViewerRuntimeChanges).not.toHaveBeenCalled();
  rerender({ active: true });
  await waitFor(() => expect(result.current.inventory).toEqual(inventory()));
  expect(vi.mocked(listenForViewerRuntimeChanges).mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(listViewerRuntimes).mock.invocationCallOrder[0]);
  rerender({ active: false });
  expect(stop).toHaveBeenCalledOnce();
  act(() => { changed(); window.dispatchEvent(new Event("focus")); });
  expect(listViewerRuntimes).toHaveBeenCalledOnce();
});

it("never requests inventory or restarts on a browser", async () => {
  desktop.value = false;
  const { result } = renderHook(() => useViewerRuntimes(true));
  await act(async () => { await result.current.restart(row); });
  expect(listenForViewerRuntimeChanges).not.toHaveBeenCalled();
  expect(listViewerRuntimes).not.toHaveBeenCalled();
  expect(restartViewerRuntime).not.toHaveBeenCalled();
});

it("coalesces invalidations and does not publish a snapshot invalidated during loading", async () => {
  const old = deferred<RuntimeInventory>(); const next = deferred<RuntimeInventory>();
  vi.mocked(listViewerRuntimes).mockReturnValueOnce(old.promise).mockReturnValueOnce(next.promise);
  const { result } = renderHook(() => useViewerRuntimes(true));
  await waitFor(() => expect(listViewerRuntimes).toHaveBeenCalledOnce());
  act(() => { changed(); changed(); changed(); });
  expect(listViewerRuntimes).toHaveBeenCalledOnce();
  await act(async () => old.resolve(inventory()));
  expect(listViewerRuntimes).toHaveBeenCalledTimes(2);
  expect(result.current.inventory).toBeNull();
  const latest = inventory({ ...row, instance_id: "owned:2", pid: 4322 });
  await act(async () => next.resolve(latest));
  expect(result.current.inventory).toEqual(latest);
  expect(result.current.loading).toBe(false);
});

it("rejects delayed snapshots from a closed section lifetime", async () => {
  const old = deferred<RuntimeInventory>();
  vi.mocked(listViewerRuntimes).mockReturnValueOnce(old.promise).mockResolvedValue(inventory({ ...row, instance_id: "fresh" }));
  const { result, rerender } = renderHook(({ active }) => useViewerRuntimes(active), { initialProps: { active: true } });
  await waitFor(() => expect(listViewerRuntimes).toHaveBeenCalledOnce());
  rerender({ active: false }); rerender({ active: true });
  await waitFor(() => expect(result.current.inventory?.runtimes[0].instance_id).toBe("fresh"));
  await act(async () => old.resolve(inventory()));
  expect(result.current.inventory?.runtimes[0].instance_id).toBe("fresh");
});

it("retains last known rows after refresh errors and recovers on explicit retry", async () => {
  const { result } = renderHook(() => useViewerRuntimes(true));
  await waitFor(() => expect(result.current.inventory).toEqual(inventory()));
  vi.mocked(listViewerRuntimes).mockRejectedValueOnce(new Error("Runtime status unavailable"));
  await act(async () => { await result.current.refresh(); });
  expect(result.current.inventory).toEqual(inventory()); expect(result.current.error).toBe("Runtime status unavailable");
  await act(async () => { await result.current.refresh(); });
  expect(result.current.error).toBeNull();
});

it("uses the displayed instance and deduplicates repeated restart clicks", async () => {
  const pending = deferred<RuntimeRestartResponse>();
  vi.mocked(restartViewerRuntime).mockReturnValueOnce(pending.promise);
  const { result } = renderHook(() => useViewerRuntimes(true));
  await waitFor(() => expect(result.current.inventory).not.toBeNull());
  let restarting!: Promise<void>;
  act(() => { restarting = result.current.restart(row); void result.current.restart(row); });
  expect(restartViewerRuntime).toHaveBeenCalledExactlyOnceWith({ request_id: expect.any(String), runtime_id: row.runtime_id, expected_instance_id: row.instance_id });
  expect(result.current.actions.get(row.runtime_id)?.busy).toBe(true);
  const request = vi.mocked(restartViewerRuntime).mock.calls[0][0];
  await act(async () => { pending.resolve({ request_id: request.request_id, runtime_id: row.runtime_id, message: "Pi restarted" }); await restarting; });
  expect(result.current.actions.get(row.runtime_id)).toEqual({ busy: false, error: null });
  expect(notify).toHaveBeenCalledExactlyOnceWith({ title: row.name, message: "Pi restarted" });
});

it("refreshes a rejected stale instance without automatically restarting its successor", async () => {
  vi.mocked(listViewerRuntimes).mockResolvedValueOnce(inventory()).mockResolvedValue(inventory({ ...row, instance_id: "new" }));
  vi.mocked(restartViewerRuntime).mockRejectedValueOnce(new Error("Runtime instance changed"));
  const { result } = renderHook(() => useViewerRuntimes(true));
  await waitFor(() => expect(result.current.inventory).not.toBeNull());
  await act(async () => { await result.current.restart(row); });
  expect(result.current.inventory?.runtimes[0].instance_id).toBe("new");
  expect(result.current.actions.get(row.runtime_id)?.error).toBe("Runtime instance changed");
  await act(async () => { await result.current.restart(row); });
  expect(restartViewerRuntime).toHaveBeenCalledOnce();
  expect(result.current.actions.get(row.runtime_id)?.error).toContain("runtime changed");
});

it("never restarts an external runtime even if an incompatible host advertises a capability", async () => {
  const external = { ...row, ownership: "external" as const };
  vi.mocked(listViewerRuntimes).mockResolvedValue(inventory(external));
  const { result } = renderHook(() => useViewerRuntimes(true));
  await waitFor(() => expect(result.current.inventory).not.toBeNull());
  await act(async () => { await result.current.restart(external); });
  expect(restartViewerRuntime).not.toHaveBeenCalled();
});

it("lets an accepted restart finish after leaving the section without continuing inventory reads", async () => {
  const pending = deferred<RuntimeRestartResponse>();
  vi.mocked(restartViewerRuntime).mockReturnValueOnce(pending.promise);
  const { result, rerender } = renderHook(({ active }) => useViewerRuntimes(active), { initialProps: { active: true } });
  await waitFor(() => expect(result.current.inventory).not.toBeNull());
  let restarting!: Promise<void>;
  act(() => { restarting = result.current.restart(row); });
  const request = vi.mocked(restartViewerRuntime).mock.calls[0][0];
  rerender({ active: false });
  await act(async () => { pending.resolve({ request_id: request.request_id, runtime_id: row.runtime_id, message: "Restarted" }); await restarting; });
  expect(result.current.actions.get(row.runtime_id)?.busy).toBe(false);
  expect(listViewerRuntimes).toHaveBeenCalledOnce();
  expect(restartViewerRuntime).toHaveBeenCalledOnce();
  expect(notify).toHaveBeenCalledExactlyOnceWith({ title: row.name, message: "Restarted" });
  rerender({ active: true });
  await waitFor(() => expect(listViewerRuntimes).toHaveBeenCalledTimes(2));
});

it("does not claim success or resubmit when a restart acknowledgement identifies another request", async () => {
  vi.mocked(restartViewerRuntime).mockResolvedValueOnce({ request_id: "different", runtime_id: row.runtime_id, message: "Unrelated restart" });
  const { result } = renderHook(() => useViewerRuntimes(true));
  await waitFor(() => expect(result.current.inventory).not.toBeNull());
  await act(async () => { await result.current.restart(row); });
  expect(result.current.actions.get(row.runtime_id)?.error).toContain("did not match");
  expect(notify).not.toHaveBeenCalled();
  expect(restartViewerRuntime).toHaveBeenCalledOnce();
  expect(listViewerRuntimes).toHaveBeenCalledTimes(2);
});

it("delivers the accepted restart notification after Settings unmounts", async () => {
  const pending = deferred<RuntimeRestartResponse>();
  vi.mocked(restartViewerRuntime).mockReturnValueOnce(pending.promise);
  const { result, unmount } = renderHook(() => useViewerRuntimes(true));
  await waitFor(() => expect(result.current.inventory).not.toBeNull());
  let restarting!: Promise<void>;
  act(() => { restarting = result.current.restart(row); });
  const request = vi.mocked(restartViewerRuntime).mock.calls[0][0];
  unmount();
  await act(async () => {
    pending.resolve({ request_id: request.request_id, runtime_id: row.runtime_id, message: "Restart requested" });
    await restarting;
  });
  expect(notify).toHaveBeenCalledExactlyOnceWith({ title: row.name, message: "Restart requested" });
  expect(listViewerRuntimes).toHaveBeenCalledOnce();
  expect(restartViewerRuntime).toHaveBeenCalledOnce();
});

it("still loads a snapshot when live notifications are unavailable", async () => {
  vi.mocked(listenForViewerRuntimeChanges).mockRejectedValue(new Error("Events unavailable"));
  const { result } = renderHook(() => useViewerRuntimes(true));
  await waitFor(() => expect(result.current.inventory).toEqual(inventory()));
  expect(result.current.live_error).toContain("Refresh to check");
});
