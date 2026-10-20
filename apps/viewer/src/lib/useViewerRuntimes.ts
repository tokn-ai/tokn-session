import { useCallback, useEffect, useRef, useState } from "react";
import { createUuid } from "./id";
import { errorMessage } from "./state";
import { isDesktop } from "./transport";
import { listViewerRuntimes, listenForViewerRuntimeChanges, restartViewerRuntime } from "./tauri";
import type { RuntimeInfo, RuntimeInventory } from "./types";
import { useActionNotification } from "../components/ActionNotifications";

export interface RuntimeActionState { busy: boolean; error: string | null; }
interface InventoryRead { epoch: number; again: boolean; promise: Promise<void>; }

/** Inventory observation is active only while its section is visible. */
export function useViewerRuntimes(active: boolean) {
  const desktop = isDesktop();
  const notify = useActionNotification();
  const [inventory, setInventory] = useState<RuntimeInventory | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [live_error, setLiveError] = useState<string | null>(null);
  const [actions, setActions] = useState(new Map<string, RuntimeActionState>());
  const inventory_ref = useRef(inventory);
  const actions_ref = useRef(actions);
  const mounted = useRef(false);
  const observing = useRef(false);
  const lifetime = useRef(0);
  const pending = useRef<InventoryRead | null>(null);

  const updateAction = useCallback((runtime_id: string, state: RuntimeActionState) => {
    if (!mounted.current) return;
    const next = new Map(actions_ref.current).set(runtime_id, state);
    actions_ref.current = next;
    setActions(next);
  }, []);

  const refresh = useCallback((): Promise<void> => {
    if (!observing.current) return Promise.resolve();
    const epoch = lifetime.current;
    if (pending.current?.epoch === epoch) {
      pending.current.again = true;
      return pending.current.promise;
    }
    const read: InventoryRead = { epoch, again: false, promise: Promise.resolve() };
    pending.current = read;
    setLoading(true);
    read.promise = (async () => {
      do {
        read.again = false;
        try {
          const next = await listViewerRuntimes();
          if (!observing.current || lifetime.current !== epoch) return;
          if (next.version !== 1) throw new Error("This viewer uses an unsupported runtime inventory format.");
          // An invalidation during the read means its snapshot may already be
          // obsolete. One queued read replaces it without parallel requests.
          if (!read.again) {
            inventory_ref.current = next;
            setInventory(next);
            setError(null);
          }
        } catch (failure) {
          if (!observing.current || lifetime.current !== epoch) return;
          if (!read.again) {
            const message = errorMessage(failure);
            setError(/unknown.*command|unknown viewer api route|command.*not found/i.test(message)
              ? "This viewer does not expose runtime management. Update the desktop app to use it." : message);
          }
        }
      } while (read.again && observing.current && lifetime.current === epoch);
    })().finally(() => {
      if (pending.current === read) {
        pending.current = null;
        if (observing.current && lifetime.current === epoch) setLoading(false);
      }
    });
    return read.promise;
  }, []);

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  useEffect(() => {
    const epoch = ++lifetime.current;
    observing.current = active && desktop;
    setLiveError(null);
    if (!observing.current) { setLoading(false); return; }
    setLoading(true);
    let stop: (() => void) | undefined;
    void (async () => {
      try {
        const unlisten = await listenForViewerRuntimeChanges(() => {
          if (observing.current && lifetime.current === epoch) void refresh();
        });
        if (!observing.current || lifetime.current !== epoch) { unlisten(); return; }
        stop = unlisten;
      } catch {
        if (!observing.current || lifetime.current !== epoch) return;
        setLiveError("Live runtime updates are unavailable. Refresh to check current status.");
      }
      await refresh();
    })();
    const focus = () => { void refresh(); };
    window.addEventListener("focus", focus);
    return () => {
      observing.current = false;
      lifetime.current += 1;
      stop?.();
      window.removeEventListener("focus", focus);
    };
  }, [active, desktop, refresh]);

  const restart = useCallback(async (runtime: RuntimeInfo) => {
    if (!desktop || !observing.current || !mounted.current || actions_ref.current.get(runtime.runtime_id)?.busy) return;
    if (!runtime.can_restart || runtime.ownership !== "viewer") return;
    const current = inventory_ref.current?.runtimes.find((row) => row.runtime_id === runtime.runtime_id);
    if (!current || current.instance_id !== runtime.instance_id || !current.can_restart || current.ownership !== "viewer") {
      updateAction(runtime.runtime_id, { busy: false, error: "This runtime changed. Check its refreshed status before restarting." });
      await refresh();
      return;
    }
    updateAction(runtime.runtime_id, { busy: true, error: null });
    let outcome: RuntimeActionState;
    try {
      const request_id = createUuid();
      const result = await restartViewerRuntime({ request_id, runtime_id: runtime.runtime_id, expected_instance_id: runtime.instance_id });
      if (result.request_id !== request_id || result.runtime_id !== runtime.runtime_id) throw new Error("The restart response did not match this request.");
      notify({ title: runtime.name, message: result.message || "Restart requested" });
      outcome = { busy: false, error: null };
    } catch (failure) {
      outcome = { busy: false, error: errorMessage(failure) };
    }
    // Refresh on success or rejection, including stale instance/lease errors.
    // A read never retries the restart action itself.
    await refresh();
    updateAction(runtime.runtime_id, outcome);
  }, [desktop, notify, refresh, updateAction]);

  return { desktop, inventory, loading, error, live_error, actions, refresh, restart };
}
