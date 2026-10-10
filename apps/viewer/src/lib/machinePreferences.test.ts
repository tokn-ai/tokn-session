import { afterEach, expect, it, vi } from "vitest";
import { preferredHub, readMachinePreferences, rememberHub, rememberMachine } from "./machinePreferences";

const host_id = "550e8400-e29b-41d4-a716-446655440000";
afterEach(() => { localStorage.clear(); vi.restoreAllMocks(); });

it("starts with the picker and migrates a legacy Hub without assuming its machine was selected", () => {
  expect(readMachinePreferences()).toEqual({ version: 1, selected_machine: null, hub_urls: [] });
  localStorage.setItem("tokn.last-hub.v1", "https://hub.example/");
  const preferences = readMachinePreferences();
  expect(preferences.selected_machine).toBeNull();
  expect(preferredHub(preferences)).toBe("https://hub.example");
});

it("keeps last machine selection separate from available Hub addresses and scopes UUIDs by Hub", () => {
  let preferences = rememberMachine(readMachinePreferences(), { kind: "hub", hub_url: "https://first.example", host_id });
  preferences = rememberHub(preferences, "https://second.example/");
  expect(readMachinePreferences().selected_machine).toEqual({ kind: "hub", hub_url: "https://first.example", host_id });
  expect(preferredHub(preferences)).toBe("https://first.example");
  preferences = rememberMachine(preferences, { kind: "hub", hub_url: "https://second.example", host_id });
  expect(preferredHub(preferences)).toBe("https://second.example");
  expect(preferences.hub_urls).toEqual(["https://second.example", "https://first.example"]);
  rememberMachine(preferences, { kind: "local" });
  expect(readMachinePreferences().selected_machine).toEqual({ kind: "local" });
});

it.each([
  { version: 2, selected_machine: null, hub_urls: [] },
  { version: 1, selected_machine: { kind: "hub", hub_url: "https://hub.example", host_id: "alice:workstation" }, hub_urls: [] },
  { version: 1, selected_machine: { kind: "hub", hub_url: "https://hub.example", host_id: `${host_id}@${"H".repeat(43)}` }, hub_urls: [] },
  { version: 1, selected_machine: { kind: "hub", hub_url: "http://remote.example", host_id }, hub_urls: [] },
  { version: 1, selected_machine: null, hub_urls: ["https://hub.example/path"] },
  { version: 1, selected_machine: null, hub_urls: Array(17).fill("https://hub.example") },
])("returns to the picker for invalid navigation preferences without touching device trust: %j", (record) => {
  localStorage.setItem("tokn.machines.v1", JSON.stringify(record));
  localStorage.setItem("unrelated-device-trust", "preserved");
  expect(readMachinePreferences().selected_machine).toBeNull();
  expect(localStorage.getItem("unrelated-device-trust")).toBe("preserved");
});

it("persists only validated navigation fields and bounds the Hub history", () => {
  let preferences = readMachinePreferences();
  for (let index = 0; index < 18; index++) preferences = rememberHub(preferences, `https://hub-${index}.example`);
  preferences = rememberMachine(preferences, { kind: "hub", hub_url: "https://hub.example", host_id, host_public_key: "unused", device_secret: "unused" } as never);
  expect(preferences.hub_urls).toHaveLength(16);
  const saved = localStorage.getItem("tokn.machines.v1")!;
  expect(saved).not.toContain("unused");
  expect(JSON.parse(saved).selected_machine).toEqual({ kind: "hub", hub_url: "https://hub.example", host_id });
});

it("keeps navigation usable if localStorage is unavailable", () => {
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("Storage blocked"); });
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("Storage full"); });
  const preferences = rememberMachine(readMachinePreferences(), { kind: "local" });
  expect(preferences.selected_machine).toEqual({ kind: "local" });
});
