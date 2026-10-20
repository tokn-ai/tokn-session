import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { listViewerRuntimes, listenForViewerRuntimeChanges, restartViewerRuntime } from "./tauri";
const { nativeInvoke, nativeListen, remoteInvoke, desktop } = vi.hoisted(() => ({
  nativeInvoke: vi.fn(), nativeListen: vi.fn(), remoteInvoke: vi.fn(), desktop: { value: true },
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: nativeInvoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: nativeListen }));
vi.mock("./transport", () => ({ isDesktop: () => desktop.value, invoke: remoteInvoke, listen: vi.fn() }));
beforeEach(() => { vi.resetAllMocks(); desktop.value = true; nativeListen.mockResolvedValue(vi.fn()); });
afterEach(() => vi.restoreAllMocks());

it("manages this viewer through native commands while another machine is selected", async () => {
  const inventory = { version: 1, runtimes: [] };
  nativeInvoke.mockResolvedValueOnce(inventory).mockResolvedValueOnce({ request_id: "request", runtime_id: "reader", message: "Restarted" });
  expect(await listViewerRuntimes()).toEqual(inventory);
  const request = { request_id: "request", runtime_id: "reader", expected_instance_id: "reader:1" };
  await restartViewerRuntime(request);
  expect(nativeInvoke.mock.calls).toEqual([["list_viewer_runtimes", undefined], ["restart_viewer_runtime", { request }]]);
  const changed = vi.fn();
  const stop = await listenForViewerRuntimeChanges(changed);
  expect(nativeListen).toHaveBeenCalledExactlyOnceWith("viewer-runtimes-changed", changed);
  nativeListen.mock.calls[0][1]({ payload: {} });
  expect(changed).toHaveBeenCalledOnce();
  expect(stop).toBeTypeOf("function");
  expect(remoteInvoke).not.toHaveBeenCalled();
});

it("does not contact native or remote runtimes from a browser", async () => {
  desktop.value = false;
  await expect(listViewerRuntimes()).rejects.toThrow("desktop app");
  await expect(restartViewerRuntime({ request_id: "request", runtime_id: "reader", expected_instance_id: "old" })).rejects.toThrow("desktop app");
  await expect(listenForViewerRuntimeChanges(vi.fn())).rejects.toThrow("desktop app");
  expect(nativeInvoke).not.toHaveBeenCalled();
  expect(nativeListen).not.toHaveBeenCalled();
  expect(remoteInvoke).not.toHaveBeenCalled();
});
