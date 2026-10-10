import { parseMachineTarget } from "./hubAddress";
import { canonicalHubUrl } from "./hubDeviceStore";
import type { MachinePreferences, MachineSelection } from "./types";

const STORAGE_KEY = "tokn.machines.v1";
const LEGACY_HUB_KEY = "tokn.last-hub.v1";
const MAX_HUBS = 16;
const empty = (): MachinePreferences => ({ version: 1, selected_machine: null, hub_urls: [] });

function parseSelection(value: unknown): MachineSelection | null {
  if (value === null) return null;
  if (!value || typeof value !== "object") throw new Error("Invalid saved machine selection");
  const selection = value as Record<string, unknown>;
  if (selection.kind === "local") return { kind: "local" };
  if (selection.kind !== "hub" || typeof selection.hub_url !== "string" || typeof selection.host_id !== "string") {
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

export function readMachinePreferences(): MachinePreferences {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved) {
      if (saved.length > 32_768) throw new Error("Saved machine preferences exceed the limit");
      const record = JSON.parse(saved) as Record<string, unknown>;
      if (!record || record.version !== 1 || !Array.isArray(record.hub_urls) || record.hub_urls.length > MAX_HUBS) {
        throw new Error("Invalid saved machine preferences");
      }
      const hub_urls = [...new Set(record.hub_urls.map((url: unknown) => {
        if (typeof url !== "string") throw new Error("Invalid saved Hub address");
        return canonicalHubUrl(url);
      }))];
      const selected_machine = parseSelection(record.selected_machine);
      const preferences: MachinePreferences = { version: 1, selected_machine, hub_urls };
      return selected_machine?.kind === "hub" ? withHub(preferences, selected_machine.hub_url) : preferences;
    }
    const legacy = localStorage.getItem(LEGACY_HUB_KEY);
    return legacy ? withHub(empty(), legacy) : empty();
  } catch {
    // Invalid navigation preferences never remove machine keys or prevent access.
    return empty();
  }
}

function save(preferences: MachinePreferences): MachinePreferences {
  try { localStorage.setItem(STORAGE_KEY, JSON.stringify(preferences)); }
  catch { /* Access still works when navigation storage is unavailable. */ }
  return preferences;
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
