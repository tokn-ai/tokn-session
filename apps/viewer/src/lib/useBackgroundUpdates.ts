import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createUuid } from "./id";
import { SessionPreviewCache } from "./sessionPreviewCache";
import { errorMessage } from "./state";
import { listenForBackgroundUpdates, listenForTransportReconnect, renewBackgroundSubscriptions, subscribeBackgroundUpdates } from "./tauri";
import type { CommandInvoker, EventSubscriber } from "./transport";
import type { BackgroundUpdatesRequest, BackgroundUpdatesScope } from "./types";

const SCOPE_KEY = "tokn.viewer.background-updates";
export function readBackgroundUpdatesScope(): BackgroundUpdatesScope {
  try { return localStorage.getItem(SCOPE_KEY) === "sidebar" ? "sidebar" : "all"; }
  catch { return "all"; }
}

export function useBackgroundUpdates(send: CommandInvoker, subscribe: EventSubscriber,
  release: (command: string, payload: Record<string, unknown>) => Promise<void>, session_keys: string[]) {
  const [scope, setScope] = useState(readBackgroundUpdatesScope);
  const [status, setStatus] = useState<string | null>(null);
  const [revision, setRevision] = useState(0);
  const [connection, setConnection] = useState(0);
  const cache = useRef(new SessionPreviewCache());
  const keys = scope === "sidebar" ? JSON.stringify([...new Set(session_keys)].sort()) : "[]";
  const requestKeys = useMemo(() => JSON.parse(keys) as string[], [keys]);
  const changeScope = useCallback((value: BackgroundUpdatesScope) => {
    try { localStorage.setItem(SCOPE_KEY, value); } catch { /* Keep the local control usable. */ }
    setScope(value);
  }, []);

  useEffect(() => {
    let disposed = false;
    let active = false;
    let stopUpdates: (() => void) | undefined;
    let stopReconnect: (() => void) | undefined;
    const request: BackgroundUpdatesRequest = { subscription_id: createUuid(), scope, session_keys: requestKeys };
    cache.current.begin(request.subscription_id);
    setRevision((value) => value + 1);
    setStatus("Connecting background updates…");
    const retire = () => { void release("subscribe_background_updates", { request: { ...request, unsubscribe: true } }).catch(() => {}); };
    const updates = listenForBackgroundUpdates((update) => {
      if (!disposed && cache.current.apply(update)) setRevision((value) => value + 1);
    }, subscribe).then((stop) => { if (disposed) stop(); else stopUpdates = stop; });
    const reconnect = listenForTransportReconnect(() => {
      if (!disposed) setConnection((value) => value + 1);
    }, subscribe).then((stop) => { if (disposed) stop(); else stopReconnect = stop; });
    void (async () => {
      try {
        await updates;
        await reconnect.catch(() => {});
        if (disposed) return;
        const ack = await subscribeBackgroundUpdates(request, send);
        if (disposed) { retire(); return; }
        if (!ack || ack.subscription_id !== request.subscription_id || ack.subscribed === false) throw new Error("Background subscription was not accepted");
        cache.current.acknowledge(ack);
        active = true;
        setStatus(null);
      } catch (error) {
        if (!disposed) {
          const message = errorMessage(error);
          const externalReader = /unavailable.*external.*reader/i.test(message);
          const unsupported = externalReader || /unknown.*command|unknown.*variant|command.*not found|unknown viewer api route|unavailable for a session share/i.test(message);
          setStatus(externalReader ? "Background updates unavailable in External reader mode. Open sessions still update."
            : unsupported ? "Background updates unavailable on this host. Open sessions still update." : `Background updates unavailable: ${message}`);
        }
      }
    })();
    // Even a listener failure must consume the parallel reconnect promise.
    void reconnect.catch(() => {});
    const timer = window.setInterval(() => {
      if (!active || disposed) return;
      void renewBackgroundSubscriptions([request.subscription_id], send).catch((error) => {
        if (disposed) return;
        active = false;
        const message = errorMessage(error);
        if (/background subscription expired/i.test(message)) {
          // Expiry retires this identity; the next effect restores only live
          // interests and preserves text without requesting a history baseline.
          setConnection((value) => value + 1);
        } else setStatus(`Background updates paused: ${message}`);
      });
    }, 30_000);
    return () => { disposed = true; active = false; window.clearInterval(timer); stopUpdates?.(); stopReconnect?.(); retire(); };
  }, [connection, release, requestKeys, scope, send, subscribe]);

  const rememberPage = useCallback((session_key: string, preview: string | null, token: ReturnType<SessionPreviewCache["capture"]>) => {
    if (cache.current.rememberPage(session_key, preview, token)) setRevision((value) => value + 1);
  }, []);
  return { scope, changeScope, status, revision, cache: cache.current, rememberPage };
}
