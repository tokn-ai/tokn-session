import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { DevicePreferencesSnapshot, MachinePreferences, NativePreferenceUpdate, ViewerPreferences } from "./types";

const native = vi.hoisted(() => ({
  initialize: vi.fn(), write_viewer: vi.fn(), write_machines: vi.fn(),
  listen_viewer: vi.fn(), listen_machines: vi.fn(), desktop: vi.fn(),
}));
vi.mock("./tauri", () => ({
  initializeDevicePreferences: native.initialize,
  writeViewerPreferences: native.write_viewer,
  writeMachinePreferences: native.write_machines,
  listenForViewerPreferences: native.listen_viewer,
  listenForMachinePreferences: native.listen_machines,
}));
vi.mock("./transport", () => ({ isDesktop: native.desktop }));

const viewer: ViewerPreferences = { version: 1, sidebar_order: "time", background_updates_scope: "all",
  background_update_level: "messages", conversation_update_level: "steps", show_lifecycle: false };
const machines: MachinePreferences = { version: 1, selected_machine: null, hub_urls: [] };
const snapshot: DevicePreferencesSnapshot = { version: 1, revision: 0, viewer_preferences: viewer, machine_preferences: machines };
const update = <T,>(preferences: T, revision: number, write_id = "another-window"): NativePreferenceUpdate<T> => ({ preferences, revision, write_id });
let viewer_event!: (value: NativePreferenceUpdate<ViewerPreferences>) => void;
let machine_event!: (value: NativePreferenceUpdate<MachinePreferences>) => void;
let stop_viewer: ReturnType<typeof vi.fn>;
let stop_machines: ReturnType<typeof vi.fn>;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => { resolve = accept; });
  return { promise, resolve };
}

beforeEach(() => {
  vi.resetModules(); vi.clearAllMocks(); localStorage.clear();
  native.desktop.mockReturnValue(true);
  stop_viewer = vi.fn(); stop_machines = vi.fn();
  native.listen_viewer.mockImplementation(async (handler) => { viewer_event = handler; return stop_viewer; });
  native.listen_machines.mockImplementation(async (handler) => { machine_event = handler; return stop_machines; });
  native.initialize.mockResolvedValue(snapshot);
  let revision = 0;
  native.write_viewer.mockImplementation(async (value, write_id) => update(value, ++revision, write_id));
  native.write_machines.mockImplementation(async (value, write_id) => update(value, ++revision, write_id));
});
afterEach(() => { cleanup(); localStorage.clear(); });

it("registers native listeners before one hydration and gates hosting/viewer under StrictMode", async () => {
  const pending = deferred<DevicePreferencesSnapshot>();
  native.initialize.mockImplementation(() => {
    expect(native.listen_viewer).toHaveBeenCalledTimes(1);
    expect(native.listen_machines).toHaveBeenCalledTimes(1);
    return pending.promise;
  });
  const { NativeDevicePreferencesProvider } = await import("../components/NativeDevicePreferencesProvider");
  const mount = vi.fn();
  function Workspace() { mount(); return <p>Hosting and sessions ready</p>; }
  render(<StrictMode><NativeDevicePreferencesProvider><Workspace /></NativeDevicePreferencesProvider></StrictMode>);
  expect(screen.getByRole("status")).toHaveTextContent("Loading this device’s preferences");
  expect(mount).not.toHaveBeenCalled();
  await waitFor(() => expect(native.initialize).toHaveBeenCalledTimes(1));
  act(() => pending.resolve(snapshot));
  expect(await screen.findByText("Hosting and sessions ready")).toBeInTheDocument();
  expect(native.initialize).toHaveBeenCalledTimes(1);
  expect(native.listen_viewer).toHaveBeenCalledTimes(1);
});

it("imports only bounded display/navigation fields and treats native files as authoritative", async () => {
  const browser_viewer = { ...viewer, sidebar_order: "project", unexpected_secret: "discard" };
  localStorage.setItem("tokn.viewer.preferences.v1", JSON.stringify(browser_viewer));
  localStorage.setItem("tokn.machines.v1", JSON.stringify({ ...machines, selected_machine: { kind: "local" }, secret: "discard" }));
  localStorage.setItem("device-trust", "untouched");
  const { initializeNativeDevicePreferences } = await import("./devicePreferences");
  await initializeNativeDevicePreferences();
  expect(native.initialize).toHaveBeenCalledWith({
    viewer_preferences: { ...viewer, sidebar_order: "project" },
    machine_preferences: { ...machines, selected_machine: { kind: "local" } },
  });
  const preferences = await import("./viewerPreferences");
  const machine_preferences = await import("./machinePreferences");
  expect(preferences.readViewerPreferences()).toEqual(viewer);
  expect(machine_preferences.readMachinePreferences()).toEqual(machines);
  preferences.updateViewerPreferences({ show_lifecycle: true });
  machine_preferences.rememberMachine(machines, { kind: "local" });
  await waitFor(() => expect(native.write_machines).toHaveBeenCalledTimes(1));
  expect(native.write_viewer).toHaveBeenCalledWith({ ...viewer, show_lifecycle: true }, expect.any(String));
  expect(localStorage.getItem("tokn.viewer.preferences.v1")).toBe(JSON.stringify(browser_viewer));
  expect(localStorage.getItem("device-trust")).toBe("untouched");
});

it("does not import malformed or oversized browser records and preserves them", async () => {
  const invalid = "x".repeat(4097);
  localStorage.setItem("tokn.viewer.preferences.v1", invalid);
  localStorage.setItem("tokn.machines.v1", "x".repeat(32769));
  const { initializeNativeDevicePreferences } = await import("./devicePreferences");
  await initializeNativeDevicePreferences();
  expect(native.initialize).toHaveBeenCalledWith({ viewer_preferences: null, machine_preferences: null });
  expect(localStorage.getItem("tokn.viewer.preferences.v1")).toBe(invalid);
});

it("retains events that arrive before the hydration snapshot and keeps listeners across view replacement", async () => {
  const pending = deferred<DevicePreferencesSnapshot>();
  native.initialize.mockReturnValue(pending.promise);
  const { initializeNativeDevicePreferences } = await import("./devicePreferences");
  const started = initializeNativeDevicePreferences();
  await waitFor(() => expect(native.initialize).toHaveBeenCalledTimes(1));
  viewer_event(update({ ...viewer, sidebar_order: "project" }, 2));
  machine_event(update({ ...machines, selected_machine: { kind: "local" } }, 1));
  pending.resolve(snapshot); await started;
  const preferences = await import("./viewerPreferences");
  const machine_preferences = await import("./machinePreferences");
  expect(preferences.readViewerPreferences().sidebar_order).toBe("project");
  expect(machine_preferences.readMachinePreferences().selected_machine).toEqual({ kind: "local" });
  viewer_event(update({ ...viewer, background_update_level: "final" }, 3));
  expect(preferences.readViewerPreferences().background_update_level).toBe("final");
  expect(stop_viewer).not.toHaveBeenCalled(); expect(stop_machines).not.toHaveBeenCalled();
});

it("uses the snapshot revision to ignore pre-hydration events from an older commit", async () => {
  const pending = deferred<DevicePreferencesSnapshot>();
  native.initialize.mockReturnValue(pending.promise);
  const { initializeNativeDevicePreferences } = await import("./devicePreferences");
  const started = initializeNativeDevicePreferences();
  await waitFor(() => expect(native.initialize).toHaveBeenCalledTimes(1));
  viewer_event(update({ ...viewer, sidebar_order: "project" }, 3));
  viewer_event(update({ ...viewer, show_lifecycle: true }, 2));
  machine_event(update({ ...machines, selected_machine: { kind: "local" } }, 4));
  pending.resolve({ ...snapshot, revision: 5 }); await started;
  const preferences = await import("./viewerPreferences");
  const machine_preferences = await import("./machinePreferences");
  expect(preferences.readViewerPreferences()).toEqual(viewer);
  expect(machine_preferences.readMachinePreferences()).toEqual(machines);
  viewer_event(update({ ...viewer, sidebar_order: "project" }, 6));
  viewer_event(update({ ...viewer, show_lifecycle: true }, 5));
  expect(preferences.readViewerPreferences()).toEqual({ ...viewer, sidebar_order: "project" });
});

it("invalid native records fail visibly without falling back or writing and retry uses the original import", async () => {
  localStorage.setItem("tokn.viewer.sidebar-order", "project");
  native.initialize.mockResolvedValueOnce({ ...snapshot, machine_preferences: { ...machines, version: 2 } });
  const { NativeDevicePreferencesProvider } = await import("../components/NativeDevicePreferencesProvider");
  render(<NativeDevicePreferencesProvider><p>Hosting and sessions ready</p></NativeDevicePreferencesProvider>);
  expect(await screen.findByRole("alert")).toHaveTextContent("Saved native machine preferences are invalid");
  expect(screen.queryByText("Hosting and sessions ready")).not.toBeInTheDocument();
  expect(native.write_viewer).not.toHaveBeenCalled(); expect(native.write_machines).not.toHaveBeenCalled();
  expect(stop_viewer).toHaveBeenCalledTimes(1); expect(stop_machines).toHaveBeenCalledTimes(1);
  localStorage.setItem("tokn.viewer.sidebar-order", "time");
  fireEvent.click(screen.getByRole("button", { name: "Retry" }));
  expect(await screen.findByText("Hosting and sessions ready")).toBeInTheDocument();
  expect(native.initialize.mock.calls[1]).toEqual(native.initialize.mock.calls[0]);
});

it("reports a failed save inside Settings and retains the user's edit for retry", async () => {
  const { initializeNativeDevicePreferences } = await import("./devicePreferences");
  await initializeNativeDevicePreferences();
  native.write_viewer.mockRejectedValueOnce(new Error("Disk full"));
  const { NativePreferenceSaveNotice } = await import("../components/NativePreferenceSaveNotice");
  const { updateViewerPreferences, readViewerPreferences } = await import("./viewerPreferences");
  render(<NativePreferenceSaveNotice placement="settings" />);
  act(() => updateViewerPreferences({ sidebar_order: "project" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Disk full");
  expect(readViewerPreferences().sidebar_order).toBe("project");
  fireEvent.click(screen.getByRole("button", { name: "Retry save" }));
  await waitFor(() => expect(screen.queryByRole("alert")).not.toBeInTheDocument());
  expect(native.write_viewer).toHaveBeenCalledTimes(2);
  expect(readViewerPreferences().sidebar_order).toBe("project");
});

it("browser rendering never hydrates native files or blocks children", async () => {
  native.desktop.mockReturnValue(false);
  const { NativeDevicePreferencesProvider } = await import("../components/NativeDevicePreferencesProvider");
  render(<NativeDevicePreferencesProvider><p>Browser preferences</p></NativeDevicePreferencesProvider>);
  expect(screen.getByText("Browser preferences")).toBeInTheDocument();
  expect(native.initialize).not.toHaveBeenCalled();
  expect(native.listen_viewer).not.toHaveBeenCalled();
});
