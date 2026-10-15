import { useSyncExternalStore } from "react";
import {
  decodeMachinePreferences, nativeMachinePreferences, readMachinePreferencesForNativeImport,
} from "./machinePreferences";
import {
  initializeDevicePreferences, listenForMachinePreferences, listenForViewerPreferences,
  writeMachinePreferences, writeViewerPreferences,
} from "./tauri";
import type { InitializeDevicePreferencesRequest, MachinePreferences, NativePreferenceUpdate, ViewerPreferences } from "./types";
import {
  decodeViewerPreferences, nativeViewerPreferences, readViewerPreferencesForNativeImport,
} from "./viewerPreferences";

interface NativeDevicePreferencesStatus {
  ready: boolean;
  loading: boolean;
  error: string | null;
}

let status: NativeDevicePreferencesStatus = Object.freeze({ ready: false, loading: false, error: null });
const listeners = new Set<() => void>();
let initialization: Promise<void> | undefined;
let import_request: InitializeDevicePreferencesRequest | undefined;

function publish(next: NativeDevicePreferencesStatus) {
  status = Object.freeze(next);
  for (const listener of listeners) listener();
}

function validViewer(value: unknown): ViewerPreferences {
  const preferences = decodeViewerPreferences(value);
  if (!preferences) throw new Error("Saved native viewer preferences are invalid. Restore the preferences file and retry.");
  return preferences;
}

function validMachine(value: unknown): MachinePreferences {
  const preferences = decodeMachinePreferences(value);
  if (!preferences) throw new Error("Saved native machine preferences are invalid. Restore the machines file and retry.");
  return preferences;
}

function validRevision(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function decodeUpdate<T>(value: unknown, decode: (value: unknown) => T | null): NativePreferenceUpdate<T> | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const record = value as Record<string, unknown>;
  if (!validRevision(record.revision) || typeof record.write_id !== "string" || !record.write_id || record.write_id.length > 128) return null;
  const preferences = decode(record.preferences);
  return preferences ? { revision: record.revision, write_id: record.write_id, preferences } : null;
}

function validUpdate<T>(value: unknown, decode: (value: unknown) => T | null): NativePreferenceUpdate<T> {
  const update = decodeUpdate(value, decode);
  if (!update) throw new Error("Native preference save returned an invalid update.");
  return update;
}

/** Register device events before hydration so a delayed snapshot cannot miss a newer save. */
export function initializeNativeDevicePreferences(): Promise<void> {
  if (status.ready) return Promise.resolve();
  if (initialization) return initialization;
  import_request ??= {
    viewer_preferences: readViewerPreferencesForNativeImport(),
    machine_preferences: readMachinePreferencesForNativeImport(),
  };
  publish({ ready: false, loading: true, error: null });
  initialization = (async () => {
    const subscriptions: (() => void)[] = [];
    let viewer_event: NativePreferenceUpdate<ViewerPreferences> | undefined;
    let machine_event: NativePreferenceUpdate<MachinePreferences> | undefined;
    let hydrated = false;
    try {
      subscriptions.push(await listenForViewerPreferences((value) => {
        const update = decodeUpdate(value, decodeViewerPreferences);
        if (!update) return;
        if (hydrated) nativeViewerPreferences.receive(update);
        else if (!viewer_event || update.revision > viewer_event.revision) viewer_event = update;
      }));
      subscriptions.push(await listenForMachinePreferences((value) => {
        const update = decodeUpdate(value, decodeMachinePreferences);
        if (!update) return;
        if (hydrated) nativeMachinePreferences.receive(update);
        else if (!machine_event || update.revision > machine_event.revision) machine_event = update;
      }));
      const snapshot = await initializeDevicePreferences(import_request!);
      if (snapshot.version !== 1 || !validRevision(snapshot.revision)) throw new Error("Native device preferences use an unsupported version or revision.");
      const viewer = validViewer(snapshot.viewer_preferences);
      const machine = validMachine(snapshot.machine_preferences);
      const latest_viewer = viewer_event && viewer_event.revision > snapshot.revision ? viewer_event : undefined;
      const latest_machine = machine_event && machine_event.revision > snapshot.revision ? machine_event : undefined;
      nativeViewerPreferences.initialize(latest_viewer?.preferences ?? viewer,
        async (value, write_id) => validUpdate(await writeViewerPreferences(value, write_id), decodeViewerPreferences),
        latest_viewer?.revision ?? snapshot.revision);
      nativeMachinePreferences.initialize(latest_machine?.preferences ?? machine,
        async (value, write_id) => validUpdate(await writeMachinePreferences(value, write_id), decodeMachinePreferences),
        latest_machine?.revision ?? snapshot.revision);
      hydrated = true;
      publish({ ready: true, loading: false, error: null });
      // These subscriptions belong to this native document, not its current machine.
      // They stay active when Machines or a remote conversation replaces the view.
    } catch (error) {
      for (const unsubscribe of subscriptions) unsubscribe();
      initialization = undefined;
      publish({ ready: false, loading: false, error: error instanceof Error ? error.message : String(error) });
      throw error;
    }
  })();
  return initialization;
}

const subscribe = (listener: () => void) => {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
};

export function useNativeDevicePreferencesStatus() {
  return useSyncExternalStore(subscribe, () => status, () => status);
}
