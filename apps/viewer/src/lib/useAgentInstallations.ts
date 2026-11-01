import { useCallback, useEffect, useRef, useState } from "react";
import { createUuid } from "./id";
import { captureTransport } from "./transport";
import { cancelAgentInstallation, getAgentInstallations, removeAgentInstallation, startAgentInstallation } from "./tauri";
import type { RpcProvider, RuntimeInstallInventory, RuntimeInstallJob } from "./types";

export function installationJobActive(job: RuntimeInstallJob | null): boolean {
  return !!job && !["completed", "failed", "cancelled"].includes(job.state);
}

const describe = (failure: unknown) => failure instanceof Error ? failure.message : String(failure);

/** The parent remounts on machine changes; delayed operations retain this host. */
export function useAgentInstallations(active: boolean, connected: boolean) {
  const [transport] = useState(captureTransport);
  const alive = useRef(true);
  const reads = useRef(0);
  const pending = useRef(false);
  const attempts = useRef(new Map<RpcProvider, string>());
  const [inventory, setInventory] = useState<RuntimeInstallInventory | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState<RpcProvider | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [action_error, setActionError] = useState<string | null>(null);
  const [live_error, setLiveError] = useState<string | null>(null);

  const accept = useCallback((snapshot: RuntimeInstallInventory) => {
    for (const agent of snapshot.agents) {
      if (agent.job && attempts.current.get(agent.provider) === agent.job.request_id) {
        setActionError(null);
        if (!installationJobActive(agent.job)) attempts.current.delete(agent.provider);
      }
    }
    setInventory(snapshot);
  }, []);
  const refresh = useCallback(async (clear_action_error = false) => {
    if (!connected) return;
    const read = ++reads.current;
    setLoading(true);
    try {
      const snapshot = await getAgentInstallations(transport.invoke);
      if (alive.current && reads.current === read) {
        accept(snapshot); setError(null);
        if (clear_action_error) setActionError(null);
      }
    } catch (failure) {
      if (alive.current && reads.current === read) setError(describe(failure));
    } finally {
      if (alive.current && reads.current === read) setLoading(false);
    }
  }, [accept, connected, transport]);

  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; reads.current += 1; };
  }, []);
  useEffect(() => {
    if (!active || !connected) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const stops: (() => void)[] = [];
    setLiveError(null);
    const changed = () => {
      if (cancelled || timer) return;
      timer = setTimeout(() => { timer = undefined; if (!cancelled) void refresh(); }, 150);
    };
    for (const event of ["agent-installations-changed", "rpc-runtimes-changed", "managed-session-changed", "transport-reconnected"]) {
      void transport.listen(event, changed).then((stop) => {
        if (cancelled) stop(); else stops.push(stop);
      }).catch(() => {
        if (!cancelled && event === "agent-installations-changed") setLiveError("Live download status is unavailable. Refresh to check changes.");
      });
    }
    stops.push(transport.on_close(() => {
      if (!cancelled) setLiveError("The connection closed. Downloads already started continue on this host.");
    }));
    void refresh();
    const focus = () => { if (!cancelled) void refresh(); };
    window.addEventListener("focus", focus);
    return () => {
      cancelled = true; clearTimeout(timer); stops.forEach((stop) => stop());
      window.removeEventListener("focus", focus);
    };
  }, [active, connected, refresh, transport]);

  const running = inventory?.agents.some((agent) => installationJobActive(agent.job)) ?? false;
  useEffect(() => {
    if (!active || !connected || !running) return;
    const timer = setInterval(() => { void refresh(); }, 2000);
    return () => clearInterval(timer);
  }, [active, connected, refresh, running]);

  async function mutate(provider: RpcProvider, operation: () => Promise<void>) {
    if (!active || !connected || pending.current) return false;
    pending.current = true; setBusy(provider); setActionError(null);
    // Invalidate reads started before this mutation so they cannot erase it.
    reads.current += 1;
    try { await operation(); return true; }
    catch (failure) { if (alive.current) setActionError(describe(failure)); return false; }
    finally { pending.current = false; if (alive.current) { setBusy(null); setLoading(false); } }
  }
  function acceptJob(job: RuntimeInstallJob) {
    if (!alive.current) return;
    reads.current += 1;
    setInventory((before) => before && { agents: before.agents.map((agent) => agent.provider === job.provider ? { ...agent, job } : agent) });
  }
  async function start(provider: RpcProvider) {
    await mutate(provider, async () => {
      let request_id = attempts.current.get(provider);
      if (!request_id) { request_id = createUuid(); attempts.current.set(provider, request_id); }
      const job = await startAgentInstallation({ request_id, provider }, transport.invoke);
      acceptJob(job);
      if (alive.current) await refresh();
    });
  }
  async function cancel(job: RuntimeInstallJob) {
    await mutate(job.provider, async () => {
      const current = await cancelAgentInstallation({ job_id: job.job_id }, transport.invoke);
      acceptJob(current);
      if (alive.current) await refresh();
    });
  }
  async function remove(provider: RpcProvider, generation: string) {
    return mutate(provider, async () => {
      const snapshot = await removeAgentInstallation({ provider, generation }, transport.invoke);
      if (alive.current) { reads.current += 1; accept(snapshot); }
    });
  }
  return { inventory, loading, busy, error: action_error ?? error, live_error, refresh, start, cancel, remove };
}
