import { createContext, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createUuid } from "./id";
import { getSessionControl, setSessionControl, cancelSessionTurn } from "./tauri";
import { captureTransport, viewerStorageScope } from "./transport";
import { errorMessage } from "./state";
import type { SessionControlStatus, SessionSummary, SessionTransport } from "./types";

interface ControlView {
  owner: string;
  status: SessionControlStatus | null;
  checking: boolean;
  changing: boolean;
  stopping: boolean;
  error: string | null;
  notice: string | null;
  revision: number;
  binding_key: string | null;
}
const emptyView = (owner: string): ControlView => ({ owner, status: null, checking: true,
  changing: false, stopping: false, error: null, notice: null, revision: 0, binding_key: null });
const bindingKey = (status: SessionControlStatus) => JSON.stringify([status.transport, status.runtime_id, status.instance_id]);

export const SessionControlContext = createContext<{ session_key: string | null; revision: string; blocked: boolean } | null>(null);

/** Host-owned routing is explicit. A failed RPC never chooses Desktop locally. */
export function useSessionControl(session: SessionSummary | null, refresh_key = 0,
  on_completed?: (session_key: string) => void | Promise<void>) {
  const provider = session?.provider;
  const gated_input = provider === "codex" || provider === "dsh";
  const live_activity = ["zcode", "qoder", "opencode", "dsh"].includes(provider ?? "");
  const enabled = !!session && (gated_input || live_activity) && !session.is_subagent;
  const session_key = enabled ? session.session_key : null;
  const scope = viewerStorageScope();
  const transport = useMemo(captureTransport, [scope]);
  const owner = JSON.stringify([scope, session_key]);
  const latest_owner = useRef(owner);
  latest_owner.current = owner;
  const sequence = useRef(0);
  const busy = useRef(false);
  const active = useRef(false);
  const [view, setView] = useState(() => emptyView(owner));
  const value = view.owner === owner ? view : emptyView(owner);
  const watch_codex_rpc = provider === "codex" && value.status?.can_select === true && value.status.transport === "rpc";
  const latest_status = useRef(value.status);
  latest_status.current = value.status;
  const completed = useRef(on_completed);
  completed.current = on_completed;
  const update = useCallback((target: string, apply: (old: ControlView) => ControlView) => {
    if (!active.current || latest_owner.current !== target) return;
    setView((old) => apply(old.owner === target ? old : emptyView(target)));
  }, []);
  const refresh = useCallback(async (quiet = false) => {
    if (!session_key || busy.current) return;
    const request = ++sequence.current;
    if (!quiet) update(owner, (old) => ({ ...old, checking: true, error: null }));
    try {
      const status = await getSessionControl({ session_key }, transport.invoke);
      if (sequence.current !== request) return;
      if (status.session_key !== session_key) throw new Error("Control status identifies another session.");
      const previous = latest_status.current;
      let completion_error: string | null = null;
      if (previous?.is_running === true && status.is_running === false
        && previous.runtime_id === status.runtime_id && previous.instance_id === status.instance_id) {
        try { await completed.current?.(session_key); }
        catch { completion_error = "Could not refresh messages."; }
        if (sequence.current !== request) return;
      }
      update(owner, (old) => ({ ...old, status, checking: false, error: completion_error, binding_key: bindingKey(status),
        revision: old.revision + (old.binding_key !== null && old.binding_key !== bindingKey(status) ? 1 : 0) }));
    } catch (error) {
      if (sequence.current === request) update(owner, (old) => ({ ...old, status: null, checking: false, error: errorMessage(error) }));
    }
  }, [owner, session_key, transport, update]);

  useEffect(() => {
    active.current = true;
    busy.current = false;
    const stop = transport.on_close(() => {
      ++sequence.current;
      busy.current = false;
      update(owner, (old) => ({ ...old, status: null, checking: false, changing: false, stopping: false,
        revision: old.revision + 1, error: "Machine disconnected. Reconnect to change session control." }));
    });
    return () => { active.current = false; ++sequence.current; stop(); };
  }, [owner, transport, update]);
  useEffect(() => { void refresh(); }, [refresh, session?.is_running, refresh_key]);

  useEffect(() => {
    if (!session_key || (!live_activity && !watch_codex_rpc)) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void transport.listen("rpc-runtimes-changed", () => {
      if (!cancelled) void refresh(watch_codex_rpc);
    }).then((stop) => {
      if (cancelled) stop();
      else { unlisten = stop; void refresh(watch_codex_rpc); }
    }).catch(() => {
      if (!cancelled) update(owner, (old) => ({ ...old, error: "Live control updates are unavailable. Retry control to check this session." }));
    });
    return () => { cancelled = true; unlisten?.(); };
  }, [owner, live_activity, watch_codex_rpc, refresh, session_key, transport, update]);

  async function choose(next: SessionTransport) {
    if (!session_key || !value.status?.can_select || busy.current || value.checking || next === value.status.transport) return;
    busy.current = true;
    const request = ++sequence.current;
    update(owner, (old) => ({ ...old, changing: true, error: null, notice: null, revision: old.revision + 1 }));
    try {
      const status = await setSessionControl({ session_key, request_id: createUuid(), transport: next }, transport.invoke);
      if (sequence.current !== request) return;
      if (status.session_key !== session_key) throw new Error("Control response identifies another session.");
      update(owner, (old) => ({ ...old, status, changing: false, binding_key: bindingKey(status), revision: old.revision + 1 }));
    } catch (error) {
      if (sequence.current === request) update(owner, (old) => ({ ...old, status: null, changing: false,
        revision: old.revision + 1, error: `Could not confirm session control. ${errorMessage(error)}` }));
    } finally {
      if (sequence.current === request) busy.current = false;
    }
  }
  async function cancel() {
    if (!session_key || !value.status?.can_cancel || value.status.transport !== "rpc" || !value.status.available || busy.current) return;
    busy.current = true;
    const request = ++sequence.current;
    update(owner, (old) => ({ ...old, stopping: true, error: null, notice: null }));
    try {
      const request_id = createUuid();
      const result = await cancelSessionTurn({ session_key, request_id }, transport.invoke);
      if (sequence.current !== request) return;
      if (result.request_id !== request_id || result.status !== "accepted") throw new Error(result.message || "Cancellation was not confirmed. Check the session before retrying.");
      update(owner, (old) => ({ ...old, stopping: false, notice: result.message || "Stop requested." }));
    } catch (error) {
      if (sequence.current === request) update(owner, (old) => ({ ...old, stopping: false, error: errorMessage(error) }));
    } finally {
      if (sequence.current === request) {
        busy.current = false;
        if (live_activity || watch_codex_rpc) void refresh(watch_codex_rpc);
      }
    }
  }
  // Single-transport agents keep optional Stop inspection independent of input.
  const blocked = gated_input && !!session_key && (value.checking || value.changing || !value.status);
  const revision = JSON.stringify([owner, value.revision]);
  return { ...value, provider: session?.provider, enabled, blocked, revision, choose, cancel, refresh };
}
export type SessionControlController = ReturnType<typeof useSessionControl>;
