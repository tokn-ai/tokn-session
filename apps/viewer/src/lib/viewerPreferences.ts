import { useSyncExternalStore } from "react";
import type { ViewerPreferences } from "./types";

export const VIEWER_PREFERENCES_KEY = "tokn.viewer.preferences.v1";
const MAX_PREFERENCES_LENGTH = 4096;
const LEGACY_KEYS = [
  "tokn.viewer.sidebar-order",
  "tokn.viewer.background-updates",
  "tokn.viewer.background-update-level",
  "tokn.viewer.conversation-update-level",
] as const;

export const DEFAULT_VIEWER_PREFERENCES: ViewerPreferences = Object.freeze({
  version: 1,
  sidebar_order: "time",
  background_updates_scope: "all",
  background_update_level: "messages",
  conversation_update_level: "steps",
  show_lifecycle: false,
});

const listeners = new Set<() => void>();
let cached: ViewerPreferences | undefined;
let storageFingerprint: string | undefined;
let awaitingStorageBaseline = false;

function decodePreferences(value: unknown): ViewerPreferences | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const record = value as Record<string, unknown>;
  if (record.version !== 1
    || (record.sidebar_order !== "time" && record.sidebar_order !== "project")
    || (record.background_updates_scope !== "all" && record.background_updates_scope !== "sidebar")
    || (record.background_update_level !== "final" && record.background_update_level !== "messages")
    || !["final", "messages", "steps", "all"].includes(record.conversation_update_level as string)
    || typeof record.show_lifecycle !== "boolean") return null;
  // Keep only the known display fields: navigation, trust, and host settings
  // belong to their own stores and cannot enter this persisted record.
  return {
    version: 1,
    sidebar_order: record.sidebar_order,
    background_updates_scope: record.background_updates_scope,
    background_update_level: record.background_update_level,
    conversation_update_level: record.conversation_update_level as ViewerPreferences["conversation_update_level"],
    show_lifecycle: record.show_lifecycle,
  };
}

function samePreferences(left: ViewerPreferences, right: ViewerPreferences): boolean {
  return left.sidebar_order === right.sidebar_order
    && left.background_updates_scope === right.background_updates_scope
    && left.background_update_level === right.background_update_level
    && left.conversation_update_level === right.conversation_update_level
    && left.show_lifecycle === right.show_lifecycle;
}

function readStorage(): { fingerprint: string; preferences: ViewerPreferences; migrate: boolean } | null {
  try {
    const saved = localStorage.getItem(VIEWER_PREFERENCES_KEY);
    if (saved !== null) {
      if (saved.length > MAX_PREFERENCES_LENGTH) {
        return { fingerprint: "oversized", preferences: DEFAULT_VIEWER_PREFERENCES, migrate: false };
      }
      let preferences: ViewerPreferences | null = null;
      try { preferences = decodePreferences(JSON.parse(saved)); } catch { /* Malformed display defaults are recoverable. */ }
      return { fingerprint: JSON.stringify([saved]), preferences: preferences ?? DEFAULT_VIEWER_PREFERENCES, migrate: false };
    }
    const legacy = LEGACY_KEYS.map((key) => {
      const value = localStorage.getItem(key);
      return value !== null && value.length > 64 ? "invalid" : value;
    });
    const [order, scope, background, conversation] = legacy;
    return {
      fingerprint: JSON.stringify([null, ...legacy]),
      migrate: legacy.some((value) => value !== null),
      preferences: {
        ...DEFAULT_VIEWER_PREFERENCES,
        sidebar_order: order === "project" ? "project" : "time",
        background_updates_scope: scope === "sidebar" ? "sidebar" : "all",
        background_update_level: background === "final" ? "final" : "messages",
        conversation_update_level: conversation === "final" || conversation === "messages" || conversation === "all" ? conversation : "steps",
      },
    };
  } catch { return null; }
}

function savePreferences(preferences: ViewerPreferences): boolean {
  try { localStorage.setItem(VIEWER_PREFERENCES_KEY, JSON.stringify(preferences)); }
  catch { return false; }
  for (const key of LEGACY_KEYS) {
    try { localStorage.removeItem(key); } catch { /* The v1 record already takes precedence. */ }
  }
  return true;
}

export function readViewerPreferences(): ViewerPreferences {
  const stored = readStorage();
  if (stored && awaitingStorageBaseline) {
    // The user edited while both reads and writes were unavailable. The first
    // recovered read establishes a baseline without discarding those edits.
    storageFingerprint = stored.fingerprint;
    awaitingStorageBaseline = false;
  }
  if (!cached || (stored && stored.fingerprint !== storageFingerprint)) {
    const next = stored?.preferences ?? DEFAULT_VIEWER_PREFERENCES;
    if (!cached || !samePreferences(cached, next)) cached = Object.freeze(next);
    storageFingerprint = stored?.fingerprint;
    if (stored?.migrate && savePreferences(cached)) {
      storageFingerprint = JSON.stringify([JSON.stringify(cached)]);
    }
  }
  return cached;
}

function notifyListeners() {
  for (const listener of listeners) listener();
}

export function updateViewerPreferences(changes: Partial<ViewerPreferences>) {
  const current = readViewerPreferences();
  const next = decodePreferences({ ...current, ...changes, version: 1 });
  if (!next || samePreferences(current, next)) return;
  cached = Object.freeze(next);
  const saved = savePreferences(next);
  // A failed write keeps this window's changes in memory until persisted data
  // actually changes. Disabled/full storage must not make the control bounce.
  if (saved) {
    storageFingerprint = JSON.stringify([JSON.stringify(next)]);
    awaitingStorageBaseline = false;
  } else if (storageFingerprint === undefined) awaitingStorageBaseline = true;
  notifyListeners();
}

export function resetViewerPreferences() {
  const current = readViewerPreferences();
  const changed = !samePreferences(current, DEFAULT_VIEWER_PREFERENCES);
  cached = changed ? DEFAULT_VIEWER_PREFERENCES : current;
  if (savePreferences(cached)) {
    storageFingerprint = JSON.stringify([JSON.stringify(cached)]);
    awaitingStorageBaseline = false;
  } else if (storageFingerprint === undefined) awaitingStorageBaseline = true;
  if (changed) notifyListeners();
}

function onStorage(event: StorageEvent) {
  if (event.key !== null && event.key !== VIEWER_PREFERENCES_KEY && !LEGACY_KEYS.some((key) => key === event.key)) return;
  try { if (event.storageArea && event.storageArea !== localStorage) return; }
  catch { return; }
  const previous = cached;
  // Explicit v1 changes and clears are authoritative. Legacy-key events use
  // the effective record fingerprint so they cannot undo unpersisted edits
  // when the v1 record still takes precedence and has not changed.
  if (event.key === null || event.key === VIEWER_PREFERENCES_KEY) {
    storageFingerprint = undefined;
    awaitingStorageBaseline = false;
  }
  readViewerPreferences();
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

export function useViewerPreferences() {
  const preferences = useSyncExternalStore(subscribePreferences, readViewerPreferences, () => DEFAULT_VIEWER_PREFERENCES);
  return { preferences, updatePreferences: updateViewerPreferences, resetPreferences: resetViewerPreferences };
}
