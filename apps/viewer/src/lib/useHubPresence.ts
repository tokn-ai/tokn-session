import { useEffect, useState } from "react";
import type { HubAccessService } from "./hubAccess";
import { hubErrorMessage } from "./hub";
import { MAX_PRESENCE_HOSTS, validatePresenceResult } from "./hubPresence";

export const PRESENCE_REFRESH_MS = 15_000;
export type MachinePresenceStatus =
  | { state: "checking" | "offline"; error?: never; connector_version?: never }
  | { state: "online"; error?: never; connector_version?: string }
  | { state: "unavailable"; error: string; connector_version?: never };
interface PresenceSnapshot {
  service: HubAccessService;
  membership: string;
  statuses: Map<string, MachinePresenceStatus>;
}

/** Presence is transient routing metadata, scoped to this visible gallery. */
export function useHubPresence(service: HubAccessService, host_ids: string[]): Map<string, MachinePresenceStatus> {
  const ids = [...new Set(host_ids)].sort();
  const membership = JSON.stringify(ids);
  const [snapshot, setSnapshot] = useState<PresenceSnapshot>();
  useEffect(() => {
    let disposed = false;
    let running = false;
    let queued = false;
    let request: AbortController | undefined;
    let timer: ReturnType<typeof setInterval> | undefined;
    const visible = () => document.visibilityState !== "hidden";
    const publish = (updates: Map<string, MachinePresenceStatus>) => {
      if (disposed) return;
      setSnapshot((previous) => ({ service, membership, statuses: new Map([
        ...(previous?.service === service && previous.membership === membership ? previous.statuses : []),
        ...updates,
      ]) }));
    };
    const refresh = async () => {
      if (disposed || !visible() || ids.length === 0) return;
      if (running) { if (request?.signal.aborted) queued = true; return; }
      running = true;
      const current = new AbortController();
      request = current;
      try {
        for (let offset = 0; offset < ids.length; offset += MAX_PRESENCE_HOSTS) {
          const batch = ids.slice(offset, offset + MAX_PRESENCE_HOSTS);
          try {
            const result = validatePresenceResult(await service.presence(batch, current.signal), batch);
            if (disposed || current.signal.aborted) return;
            publish(new Map(result.machines.map((machine) => [
              machine.host_id,
              machine.online ? { state: "online", connector_version: machine.connector_version } : { state: "offline" },
            ])));
          } catch (error) {
            if (disposed || current.signal.aborted) return;
            publish(new Map(batch.map((id) => [id, { state: "unavailable", error: hubErrorMessage(error) }])));
          }
        }
      } finally {
        running = false;
        request = undefined;
        if (queued) { queued = false; void refresh(); }
      }
    };
    const visibilityChanged = () => {
      if (timer) clearInterval(timer);
      timer = undefined;
      if (!visible()) { request?.abort(); return; }
      void refresh();
      if (ids.length > 0) timer = setInterval(() => { void refresh(); }, PRESENCE_REFRESH_MS);
    };
    const focused = () => { void refresh(); };
    void Promise.resolve().then(() => { if (!disposed) visibilityChanged(); });
    document.addEventListener("visibilitychange", visibilityChanged);
    window.addEventListener("focus", focused);
    return () => {
      disposed = true;
      request?.abort();
      if (timer) clearInterval(timer);
      document.removeEventListener("visibilitychange", visibilityChanged);
      window.removeEventListener("focus", focused);
    };
  // IDs are normalized into membership so unrelated host metadata cannot restart polling.
  }, [service, membership]);
  return new Map(ids.map((id) => [id, snapshot?.service === service && snapshot.membership === membership
    ? snapshot.statuses.get(id) ?? { state: "checking" }
    : { state: "checking" }]));
}
