import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke as nativeInvoke } from "@tauri-apps/api/core";
import { listen as nativeListen } from "@tauri-apps/api/event";
import {
  initializeDevicePreferences, listenForMachinePreferences, listenForViewerPreferences,
  writeMachinePreferences, writeViewerPreferences,
} from "./tauri";
import { RemoteClient, selectMachine } from "./transport";
import type { MachinePreferences, ViewerPreferences } from "./types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
const viewer: ViewerPreferences = { version: 1, sidebar_order: "time", background_updates_scope: "all",
  background_update_level: "messages", conversation_update_level: "steps", show_lifecycle: false };
const machines: MachinePreferences = { version: 1, selected_machine: null, hub_urls: [] };
beforeEach(() => { vi.clearAllMocks(); });
afterEach(() => { selectMachine(); vi.unstubAllGlobals(); });

it("desktop preferences always use direct native commands/events while another machine is selected", async () => {
  vi.stubGlobal("__TAURI_INTERNALS__", {});
  const remote = new RemoteClient("https://machine.example", "unused");
  const send = vi.spyOn(remote, "invoke");
  const subscribe = vi.spyOn(remote, "listen");
  selectMachine(remote);
  vi.mocked(nativeInvoke).mockResolvedValue(viewer);
  const stop = vi.fn();
  vi.mocked(nativeListen).mockResolvedValue(stop);
  const request = { viewer_preferences: viewer, machine_preferences: machines };
  await initializeDevicePreferences(request);
  await writeViewerPreferences(viewer, "viewer-write");
  await writeMachinePreferences(machines, "machines-write");
  expect(nativeInvoke).toHaveBeenCalledWith("initialize_device_preferences", { request });
  expect(nativeInvoke).toHaveBeenCalledWith("write_viewer_preferences", { preferences: viewer, write_id: "viewer-write" });
  expect(nativeInvoke).toHaveBeenCalledWith("write_machine_preferences", { preferences: machines, write_id: "machines-write" });
  const on_viewer = vi.fn(); const on_machines = vi.fn();
  const stop_viewer = await listenForViewerPreferences(on_viewer);
  await listenForMachinePreferences(on_machines);
  expect(nativeListen).toHaveBeenCalledWith("viewer-preferences-changed", expect.any(Function));
  expect(nativeListen).toHaveBeenCalledWith("machine-preferences-changed", expect.any(Function));
  const calls = vi.mocked(nativeListen).mock.calls;
  const viewer_update = { revision: 1, write_id: "viewer-write", preferences: viewer };
  const machine_update = { revision: 2, write_id: "machines-write", preferences: machines };
  calls[0][1]({ payload: viewer_update } as never);
  calls[1][1]({ payload: machine_update } as never);
  expect(on_viewer).toHaveBeenCalledWith(viewer_update); expect(on_machines).toHaveBeenCalledWith(machine_update);
  stop_viewer(); expect(stop).toHaveBeenCalledTimes(1);
  expect(send).not.toHaveBeenCalled(); expect(subscribe).not.toHaveBeenCalled();
});

it("browser clients cannot send native preference commands through their selected host", async () => {
  const remote = new RemoteClient("https://machine.example", "unused");
  const send = vi.spyOn(remote, "invoke");
  const subscribe = vi.spyOn(remote, "listen");
  selectMachine(remote);
  await expect(initializeDevicePreferences({ viewer_preferences: null, machine_preferences: null })).rejects.toThrow("desktop app");
  await expect(writeViewerPreferences(viewer, "viewer-write")).rejects.toThrow("desktop app");
  await expect(writeMachinePreferences(machines, "machines-write")).rejects.toThrow("desktop app");
  await expect(listenForViewerPreferences(vi.fn())).rejects.toThrow("desktop app");
  await expect(listenForMachinePreferences(vi.fn())).rejects.toThrow("desktop app");
  expect(send).not.toHaveBeenCalled(); expect(subscribe).not.toHaveBeenCalled();
  expect(nativeInvoke).not.toHaveBeenCalled(); expect(nativeListen).not.toHaveBeenCalled();
});
