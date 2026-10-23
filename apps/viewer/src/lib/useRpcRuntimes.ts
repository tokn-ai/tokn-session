import { useCallback, useEffect, useRef, useState } from "react";
import { createUuid } from "./id";
import { captureTransport } from "./transport";
import {
  closeRpcRuntime, deleteRpcRuntimeProfile, getRpcRuntimeProfiles, listRpcRuntimes,
  saveRpcRuntimeProfile, startRpcRuntime, validateRpcRuntimeProfile,
} from "./tauri";
import type { RpcRuntimeInfo, RpcRuntimeInventory, RpcRuntimeProfileInput, RpcRuntimeProfilesSnapshot } from "./types";

const describe = (failure: unknown) => failure instanceof Error ? failure.message : String(failure);
interface StartFailure {
  request_id: string;
  expected_revision: number;
  can_start_new: boolean;
}

/** Mount with a machine key: every delayed read and mutation keeps its original transport. */
export function useRpcRuntimes(active: boolean, connected: boolean) {
  const [transport] = useState(captureTransport);
  const alive = useRef(true);
  const reads = useRef(0);
  const pending = useRef(false);
  const attempts = useRef(new Map<string, string>());
  const failed_starts = useRef(new Map<string, StartFailure>());
  const [start_failures, setStartFailures] = useState(new Map<string, StartFailure>());
  const [profiles, setProfiles] = useState<RpcRuntimeProfilesSnapshot | null>(null);
  const [inventory, setInventory] = useState<RpcRuntimeInventory | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [live_error, setLiveError] = useState<string | null>(null);

  const acceptProfiles = useCallback((snapshot: RpcRuntimeProfilesSnapshot) => {
    if (snapshot.version !== 1) throw new Error("This host uses an unsupported runtime profile format.");
    setProfiles((before) => before && before.revision > snapshot.revision ? before : snapshot);
  }, []);
  const refresh = useCallback(async (confirm_new_attempt = false) => {
    if (!connected) return;
    const read = ++reads.current;
    const failures_before = failed_starts.current;
    setLoading(true);
    const results = await Promise.allSettled([getRpcRuntimeProfiles(transport.invoke), listRpcRuntimes(transport.invoke)]);
    if (!alive.current || reads.current !== read) return;
    const failures: string[] = [];
    if (results[0].status === "fulfilled") {
      try { acceptProfiles(results[0].value); } catch (failure) { failures.push(describe(failure)); }
    } else failures.push(describe(results[0].reason));
    if (results[1].status === "fulfilled") {
      if (results[1].value.version === 1) setInventory(results[1].value);
      else failures.push("This host uses an unsupported runtime inventory format.");
    } else failures.push(describe(results[1].reason));
    if (results[0].status === "fulfilled" && results[0].value.version === 1
      && results[1].status === "fulfilled" && results[1].value.version === 1) {
      const revision = results[0].value.revision;
      const runtimes = results[1].value.runtimes;
      const verified = new Map(failed_starts.current);
      for (const [profile_id, failure] of failures_before) {
        if (verified.get(profile_id) !== failure) continue;
        verified.set(profile_id, { ...failure, can_start_new: (confirm_new_attempt || failure.can_start_new)
          && failure.expected_revision === revision && !runtimes.some((runtime) => runtime.profile_id === profile_id) });
      }
      failed_starts.current = verified; setStartFailures(verified);
    }
    setError([...new Set(failures)].join("\n") || null);
    setLoading(false);
  }, [acceptProfiles, connected, transport]);

  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; reads.current += 1; };
  }, []);
  useEffect(() => {
    if (!active || !connected) return;
    let cancelled = false;
    const stops: (() => void)[] = [];
    setLiveError(null);
    void refresh();
    void transport.listen("rpc-runtimes-changed", () => { if (!cancelled) void refresh(); }).then((stop) => {
      if (cancelled) stop(); else stops.push(stop);
    }).catch(() => {
      if (!cancelled) setLiveError("Live runtime status is unavailable. Refresh to check changes.");
    });
    stops.push(transport.on_close(() => {
      if (!cancelled) setLiveError("The connection closed. Reconnect to this machine to manage its runtimes.");
    }));
    const focus = () => { if (!cancelled) void refresh(); };
    window.addEventListener("focus", focus);
    return () => { cancelled = true; stops.forEach((stop) => stop()); window.removeEventListener("focus", focus); };
  }, [active, connected, refresh, transport]);

  async function mutate<T>(key: string, operation: () => Promise<T>, report_error = true): Promise<T> {
    if (pending.current) throw new Error("Another runtime change is still in progress.");
    pending.current = true; setBusy(key); setError(null);
    try { return await operation(); }
    catch (failure) {
      if (alive.current && report_error) setError(describe(failure));
      throw failure;
    } finally {
      pending.current = false;
      if (alive.current) setBusy(null);
    }
  }
  function requestId(key: string) {
    const existing = attempts.current.get(key);
    if (existing) return existing;
    const request_id = createUuid(); attempts.current.set(key, request_id); return request_id;
  }
  async function save(profile: RpcRuntimeProfileInput, expected_revision: number) {
    await mutate("save", async () => {
      const snapshot = await saveRpcRuntimeProfile({ expected_revision, profile }, transport.invoke);
      if (alive.current) { reads.current += 1; acceptProfiles(snapshot); setLoading(false); }
    }, false);
  }
  async function remove(profile_id: string) {
    if (!profiles) return;
    await mutate(`delete:${profile_id}`, async () => {
      const snapshot = await deleteRpcRuntimeProfile({ expected_revision: profiles.revision, profile_id }, transport.invoke);
      if (alive.current) { reads.current += 1; acceptProfiles(snapshot); setLoading(false); }
    });
  }
  async function validate(profile: RpcRuntimeProfileInput) {
    return mutate("validate", () => validateRpcRuntimeProfile(profile, transport.invoke), false);
  }
  async function start(profile_id: string) {
    if (!profiles) return;
    const key = `start:${profile_id}:${profiles.revision}`;
    const request_id = requestId(key);
    try {
      await mutate(key, async () => {
        const runtime = await startRpcRuntime({ request_id, profile_id, expected_revision: profiles.revision }, transport.invoke);
        attempts.current.delete(key);
        if (alive.current) {
          const next = new Map(failed_starts.current); next.delete(profile_id);
          failed_starts.current = next; setStartFailures(next);
          setInventory((before) => ({ version: 1, runtimes: [...(before?.runtimes ?? []).filter((row) => row.runtime_id !== runtime.runtime_id), runtime] }));
          await refresh();
        }
      });
    } catch (failure) {
      if (alive.current) {
        const next = new Map(failed_starts.current);
        next.set(profile_id, { request_id, expected_revision: profiles.revision, can_start_new: false });
        failed_starts.current = next; setStartFailures(next);
      }
      throw failure;
    }
  }
  async function startNewAttempt(profile_id: string) {
    const failure = failed_starts.current.get(profile_id);
    if (!profiles || !failure?.can_start_new || failure.expected_revision !== profiles.revision
      || inventory?.runtimes.some((runtime) => runtime.profile_id === profile_id)) {
      throw new Error("Refresh runtime status before starting a new attempt.");
    }
    const key = `start:${profile_id}:${profiles.revision}`;
    if (attempts.current.get(key) !== failure.request_id) throw new Error("The previous runtime attempt changed. Refresh status again.");
    attempts.current.delete(key);
    return start(profile_id);
  }
  async function close(runtime: RpcRuntimeInfo) {
    const key = `close:${runtime.runtime_id}:${runtime.instance_id}`;
    await mutate(key, async () => {
      await closeRpcRuntime({ request_id: requestId(key), runtime_id: runtime.runtime_id, expected_instance_id: runtime.instance_id }, transport.invoke);
      attempts.current.delete(key);
      if (alive.current) {
        setInventory((before) => before && { ...before, runtimes: before.runtimes.filter((row) => row.runtime_id !== runtime.runtime_id || row.instance_id !== runtime.instance_id) });
        await refresh();
      }
    });
  }
  return { profiles, inventory, loading, busy, error, live_error, start_failures, refresh, save, remove, validate, start, startNewAttempt, close };
}
