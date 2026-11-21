import { useEffect, useRef, useState, type ReactNode } from "react";
import { getLocalHostStatus, listenForLocalHostStatus, startLocalHost, stopLocalHost, stopExternalLocalHost, removeExternalLocalHost, renameLocalHost } from "../lib/tauri";
import { isHosting, LocalHostContext } from "../lib/localHost";
import type { LocalHostStatus, StartLocalHostRequest } from "../lib/types";
import { HostDialog } from "./HostSetup";

/** App-owned hosting survives navigation and always uses local Tauri commands. */
export function LocalHostProvider({ children }: { children: ReactNode }) {
  const [status, setStatus] = useState<LocalHostStatus>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [is_open, setOpen] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const alive = useRef(false);
  const revision = useRef(0);
  const pending = useRef(false);
  const operation = useRef(0);
  const canceling = useRef(false);
  const return_focus = useRef<HTMLElement | undefined>(undefined);
  function open(opener?: HTMLElement | null) {
    return_focus.current = opener ?? (document.activeElement instanceof HTMLElement ? document.activeElement : undefined);
    setOpen(true);
  }
  useEffect(() => {
    alive.current = true;
    let active = true;
    let unlisten: (() => void) | undefined;
    let refreshing = false;
    async function refresh() {
      if (refreshing || pending.current) return;
      refreshing = true;
      const before = revision.current;
      try {
        const next = await getLocalHostStatus();
        if (active && before === revision.current) { setStatus(next); setError(undefined); }
      } catch (error) {
        if (active && before === revision.current) {
          const detail = error instanceof Error ? error.message : String(error);
          setError(detail);
          setStatus((previous) => previous && { ...previous, phase: "unavailable", error: detail });
        }
      } finally { refreshing = false; }
    }
    void (async () => {
      try {
        const stop = await listenForLocalHostStatus((next) => {
          if (active) { revision.current++; setStatus(next); setError(undefined); }
        });
        if (!active) { stop(); return; }
        unlisten = stop;
        await refresh();
      } catch (error) {
        if (active) setError(error instanceof Error ? error.message : String(error));
      }
    })();
    const timer = setInterval(() => { if (document.visibilityState !== "hidden") void refresh(); }, 15_000);
    const focus = () => { void refresh(); };
    window.addEventListener("focus", focus);
    return () => {
      active = false; alive.current = false; unlisten?.(); clearInterval(timer); window.removeEventListener("focus", focus);
    };
  }, [attempt]);

  async function perform(action: () => Promise<LocalHostStatus>, options: { supersede?: boolean; clear_external_conflict?: boolean } = {}): Promise<boolean> {
    if (pending.current && !options.supersede) return false;
    const current = ++operation.current;
    pending.current = true;
    setBusy(true); setError(undefined);
    const before = revision.current;
    try {
      const next = await action();
      if (current !== operation.current) return false;
      if (alive.current && before === revision.current) { revision.current++; setStatus(next); }
      return true;
    } catch (error) {
      if (alive.current && current === operation.current) {
        setError(error instanceof Error ? error.message : String(error));
        // A connector may have started after the last observation. Refresh ownership
        // after a rejected start rather than leaving an apparently available switch.
        const observed = revision.current;
        try {
          const next = await getLocalHostStatus();
          if (alive.current && current === operation.current && observed === revision.current) {
            revision.current++; setStatus(next);
            if (next.phase === "external" && options.clear_external_conflict) setError(undefined);
          }
        } catch { /* Preserve the action error until a later status refresh succeeds. */ }
      }
      return false;
    } finally {
      if (current === operation.current) {
        pending.current = false;
        if (alive.current) setBusy(false);
      }
    }
  }
  async function change(request?: StartLocalHostRequest) {
    if (pending.current || !status || ["external", "background", "unavailable"].includes(status.phase)) return;
    if (isHosting(status)) { await perform(stopLocalHost); return; }
    const settings = request ?? status;
    if (!settings.hub_url.trim() || !settings.name.trim()) { open(); return; }
    const machine_name = settings.machine_name?.trim();
    await perform(() => startLocalHost({
      hub_url: settings.hub_url.trim(), name: settings.name.trim(), allow_control: settings.allow_control,
      ...(machine_name ? { machine_name } : {}),
    }), { clear_external_conflict: true });
  }
  async function cancel() {
    if (canceling.current) return;
    canceling.current = true;
    try { await perform(stopLocalHost, { supersede: true }); }
    finally { canceling.current = false; }
  }
  async function rename(machine_name: string): Promise<boolean> {
    if (!status?.machine_address || ["external", "background", "unavailable"].includes(status.phase)) return false;
    return perform(() => renameLocalHost({ machine_name: machine_name.trim() }));
  }
  async function manageExternal(remove: boolean) {
    if (pending.current || !status || isHosting(status)
      || !(remove ? status.external_remove_supported : status.external_stop_supported)) return;
    await perform(remove ? removeExternalLocalHost : stopExternalLocalHost);
  }
  return <LocalHostContext value={{ status, busy, error, is_open, change, cancel, rename, stop_external: () => manageExternal(false), remove_external: () => manageExternal(true),
    return_focus: return_focus.current, open, close: () => setOpen(false), retry: () => setAttempt((value) => value + 1) }}>
    {children}
    {is_open && <HostDialog />}
  </LocalHostContext>;
}
