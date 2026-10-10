import { useEffect, useRef, useState, type ReactNode } from "react";
import { getLocalHostStatus, listenForLocalHostStatus, startLocalHost, stopLocalHost, stopExternalLocalHost } from "../lib/tauri";
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

  async function change(request?: StartLocalHostRequest) {
    if (pending.current || !status || status.phase === "external" || status.phase === "unavailable") return;
    const hosting = isHosting(status);
    const settings = request ?? status;
    if (!hosting && (!settings.hub_url.trim() || !settings.name.trim())) { open(); return; }
    pending.current = true;
    setBusy(true); setError(undefined);
    const before = revision.current;
    try {
      const next = hosting ? await stopLocalHost() : await startLocalHost({
        hub_url: settings.hub_url.trim(), name: settings.name.trim(), allow_control: settings.allow_control,
      });
      if (alive.current && before === revision.current) { revision.current++; setStatus(next); }
    } catch (error) {
      if (alive.current) {
        setError(error instanceof Error ? error.message : String(error));
        // A connector may have started after the last observation. Refresh ownership
        // after a rejected start rather than leaving an apparently available switch.
        const observed = revision.current;
        try {
          const next = await getLocalHostStatus();
          if (alive.current && observed === revision.current) {
            revision.current++; setStatus(next);
            if (next.phase === "external") setError(undefined);
          }
        } catch { /* Preserve the action error until a later status refresh succeeds. */ }
      }
    } finally {
      pending.current = false;
      if (alive.current) setBusy(false);
    }
  }
  async function stopExternal() {
    if (pending.current || status?.phase !== "external" || !status.external_stop_supported) return;
    pending.current = true; setBusy(true); setError(undefined);
    const before = revision.current;
    try {
      const next = await stopExternalLocalHost();
      if (alive.current && before === revision.current) { revision.current++; setStatus(next); }
    } catch (error) {
      if (alive.current) setError(error instanceof Error ? error.message : String(error));
    } finally {
      pending.current = false;
      if (alive.current) setBusy(false);
    }
  }
  return <LocalHostContext value={{ status, busy, error, is_open, change, stop_external: stopExternal,
    return_focus: return_focus.current, open, close: () => setOpen(false), retry: () => setAttempt((value) => value + 1) }}>
    {children}
    {is_open && <HostDialog />}
  </LocalHostContext>;
}
