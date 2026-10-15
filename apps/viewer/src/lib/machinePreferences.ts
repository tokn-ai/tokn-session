import { useSyncExternalStore } from "react";
import { parseMachineTarget } from "./hubAddress";
import { canonicalHubUrl } from "./hubDeviceStore";
import { NativePreferenceStore } from "./nativePreferenceStore";
import type { MachinePreferences, MachineSelection } from "./types";

export const MACHINE_PREFERENCES_KEY = "tokn.machines.v1";
const LEGACY_HUB_KEY = "tokn.last-hub.v1";
const MAX_HUBS = 16;
const empty = (): MachinePreferences => ({ version: 1, selected_machine: null, hub_urls: [] });
const listeners = new Set<() => void>();
let cached: MachinePreferences | undefined;

function samePreferences(left: MachinePreferences, right: MachinePreferences): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function notifyListeners() { for (const listener of listeners) listener(); }

export const nativeMachinePreferences = new NativePreferenceStore<MachinePreferences>(samePreferences, notifyListeners);

function parseSelection(value: unknown): MachineSelection | null {
  if (value === null) return null;
  if (!value || typeof value !== "object") throw new Error("Invalid saved machine selection");
  const selection = value as Record<string, unknown>;
  if (selection.kind === "local") return { kind: "local" };
  if (selection.kind !== "hub" || typeof selection.hub_url !== "string" || selection.hub_url.length > 4096 || typeof selection.host_id !== "string") {
    throw new Error("Invalid saved machine selection");
  }
  const target = parseMachineTarget(selection.host_id);
  if (target.host_id !== selection.host_id || target.host_public_key !== undefined) throw new Error("Invalid saved machine ID");
  return { kind: "hub", hub_url: canonicalHubUrl(selection.hub_url), host_id: selection.host_id };
}

function withHub(preferences: MachinePreferences, hub_url: string): MachinePreferences {
  const canonical = canonicalHubUrl(hub_url);
  return { ...preferences, hub_urls: [canonical, ...preferences.hub_urls.filter((url) => url !== canonical)].slice(0, MAX_HUBS) };
}

export function decodeMachinePreferences(value: unknown): MachinePreferences | null {
  try {
    if (!value || typeof value !== "object" || Array.isArray(value)) return null;
    const record = value as Record<string, unknown>;
    if (record.version !== 1 || !Array.isArray(record.hub_urls) || record.hub_urls.length > MAX_HUBS) {
      return null;
    }
    const hub_urls = [...new Set(record.hub_urls.map((url: unknown) => {
      if (typeof url !== "string" || url.length > 4096) throw new Error("Invalid saved Hub address");
      return canonicalHubUrl(url);
    }))];
    const selected_machine = parseSelection(record.selected_machine);
    const preferences: MachinePreferences = { version: 1, selected_machine, hub_urls };
    return selected_machine?.kind === "hub" ? withHub(preferences, selected_machine.hub_url) : preferences;
  } catch { return null; }
}

/** Native migration never includes identities, reading positions, or unbounded data. */
export function readMachinePreferencesForNativeImport(): MachinePreferences | null {
  try {
    const saved = localStorage.getItem(MACHINE_PREFERENCES_KEY);
    if (saved) return saved.length <= 32_768 ? decodeMachinePreferences(JSON.parse(saved)) : null;
    const legacy = localStorage.getItem(LEGACY_HUB_KEY);
    return legacy && legacy.length <= 2048 ? withHub(empty(), legacy) : null;
  } catch { return null; }
}

export function readMachinePreferences(): MachinePreferences {
  const native = nativeMachinePreferences.read();
  if (native) return native;
  const next = readMachinePreferencesForNativeImport() ?? empty();
  if (!cached || !samePreferences(cached, next)) cached = Object.freeze(next);
  return cached;
}

function save(preferences: MachinePreferences): MachinePreferences {
  const next = decodeMachinePreferences(preferences);
  if (!next) throw new Error("Invalid machine preferences");
  if (nativeMachinePreferences.read()) {
    nativeMachinePreferences.update(next);
    return readMachinePreferences();
  }
  try { localStorage.setItem(MACHINE_PREFERENCES_KEY, JSON.stringify(next)); }
  catch { /* Access still works when navigation storage is unavailable. */ }
  notifyListeners();
  return next;
}

/** Record an available Hub without changing the last successfully opened machine. */
export function rememberHub(preferences: MachinePreferences, hub_url: string): MachinePreferences {
  return save(withHub(preferences, hub_url));
}

export function rememberMachine(preferences: MachinePreferences, selection: MachineSelection): MachinePreferences {
  const selected_machine = parseSelection(selection)!;
  const next = { ...preferences, selected_machine };
  return save(selected_machine.kind === "hub" ? withHub(next, selected_machine.hub_url) : next);
}

export function preferredHub(preferences: MachinePreferences): string {
  return preferences.selected_machine?.kind === "hub"
    ? preferences.selected_machine.hub_url : preferences.hub_urls[0] ?? "https://";
}

function onStorage(event: StorageEvent) {
  if (nativeMachinePreferences.read()) return;
  if (event.key !== null && event.key !== MACHINE_PREFERENCES_KEY && event.key !== LEGACY_HUB_KEY) return;
  try { if (event.storageArea && event.storageArea !== localStorage) return; } catch { return; }
  const previous = cached;
  readMachinePreferences();
  if (previous !== cached) notifyListeners();
}

function subscribePreferences(listener: () => void) {
  if (listeners.size === 0) window.addEventListener("storage", onStorage);
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) window.removeEventListener("storage", onStorage);
  };
}

export function useMachinePreferences() {
  return useSyncExternalStore(subscribePreferences, readMachinePreferences, readMachinePreferences);
}
