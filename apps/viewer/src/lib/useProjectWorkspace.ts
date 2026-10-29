import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { errorMessage } from "./state";
import { captureTransport, type CommandInvoker, type EventSubscriber } from "./transport";
import {
  getSessionCreationOptions, getManagedSession, listRegisteredProjects, listenForManagedSessionChanges, listenForRelayStatus,
  registerProject, respondManagedSessionDialog, resumeManagedSession, stopManagedSession,
} from "./tauri";
import type {
  ManagedSessionDialogResponse, ManagedSessionSnapshot,
  ProjectWorkspaceSnapshot, RegisterProjectRequest,
} from "./types";

interface WorkspaceOptions {
  enabled: boolean;
  machine_key: string;
  selected_session_key: string | null;
}
type WorkspaceTransport = { invoke: CommandInvoker; listen: EventSubscriber; release?: (command: string, payload?: Record<string, unknown>) => Promise<void> };
interface PendingManagedRead {
  epoch: number;
  transport: WorkspaceTransport;
  promise: Promise<ManagedSessionSnapshot | null>;
}
function inputCapabilityClass(snapshot: ManagedSessionSnapshot): string {
  if (snapshot.status === "stopped" || snapshot.status === "failed") return "saved";
  return snapshot.status === "stopping" ? "stopping" : "live";
}

/** Local commands, completion, and cleanup remain attached to one machine lifetime. */
export function useProjectWorkspace(options: WorkspaceOptions, transport_override?: WorkspaceTransport) {
  const { enabled, machine_key, selected_session_key } = options;
  const transport = useMemo(() => transport_override ?? captureTransport(), [machine_key, transport_override]);
  const active_transport = useRef(transport);
  active_transport.current = transport;
  const [workspace, setWorkspace] = useState<ProjectWorkspaceSnapshot | null>(null);
  const [loading, setLoading] = useState(enabled);
  const [error, setError] = useState<string | null>(null);
  const [managed, setManaged] = useState<ManagedSessionSnapshot | null>(null);
  const [pending_dialog_session, setPendingDialogSession] = useState<ManagedSessionSnapshot | null>(null);
  const [control_failure, setControlFailure] = useState<{ request_id: string; message: string } | null>(null);
  const [controlling, setControlling] = useState(false);
  const [input_refresh_key, setInputRefreshKey] = useState(0);
  const [retry, setRetry] = useState(0);
  const lifetime = useRef(0);
  const alive = useRef(false);
  const control_busy = useRef(false);
  const snapshots = useRef(new Map<string, ManagedSessionSnapshot>());
  const pending_reads = useRef(new Map<string, PendingManagedRead>());
  const catalog_read = useRef(0);
  const current = useRef(options);
  current.current = options;
  const accept = useCallback((snapshot: ManagedSessionSnapshot, epoch: number) => {
    if (!alive.current || lifetime.current !== epoch) return;
    const old = snapshots.current.get(snapshot.request_id);
    if (old && old.revision > snapshot.revision) return;
    // A GET may enrich an event at the same revision with its indexed session.
    const next = old?.revision === snapshot.revision && old.session && !snapshot.session
      ? { ...snapshot, session: old.session } : snapshot;
    snapshots.current.set(next.request_id, next);
    setPendingDialogSession([...snapshots.current.values()].find((entry) => entry.pending_dialog) ?? null);
    if (next.session_key && next.session_key === current.current.selected_session_key) {
      setManaged(next);
      if (!old || inputCapabilityClass(old) !== inputCapabilityClass(next)) setInputRefreshKey((value) => value + 1);
    }
  }, []);

  const refreshRequest = useCallback((request_id: string, epoch: number): Promise<ManagedSessionSnapshot | null> => {
    const read_key = `${epoch}:${request_id}`;
    const previous = pending_reads.current.get(read_key);
    if (previous?.epoch === epoch && previous.transport === transport) return previous.promise;
    let pending: PendingManagedRead;
    const promise = Promise.resolve().then(() => getManagedSession({ request_id }, transport.invoke)).then((snapshot) => {
      if (snapshot) accept(snapshot, epoch);
      return snapshot;
    }).finally(() => {
      if (pending_reads.current.get(read_key) === pending) pending_reads.current.delete(read_key);
    });
    pending = { epoch, transport, promise };
    pending_reads.current.set(read_key, pending);
    return promise;
  }, [accept, transport]);

  const refreshProjects = useCallback(async (epoch: number) => {
    const read = ++catalog_read.current;
    const snapshot = await listRegisteredProjects(transport.invoke);
    if (alive.current && lifetime.current === epoch && catalog_read.current === read) {
      setWorkspace(snapshot); setError(null);
    }
  }, [transport]);

  useEffect(() => {
    const epoch = ++lifetime.current;
    alive.current = enabled;
    setWorkspace(null); setManaged(null); setPendingDialogSession(null);
    setControlFailure(null); setError(null);
    snapshots.current.clear();
    control_busy.current = false;
    setControlling(false); setLoading(enabled);
    if (!enabled) return;
    const unlisteners: (() => void)[] = [];
    let cancelled = false;
    void (async () => {
      try {
        const stop = await listenForManagedSessionChanges((snapshot) => {
          accept(snapshot, epoch);
          if (snapshot.session_key && !snapshot.session && snapshot.session_key === current.current.selected_session_key) {
            void refreshRequest(snapshot.request_id, epoch).catch(() => {});
          }
        }, transport.listen);
        if (cancelled) { stop(); return; }
        unlisteners.push(stop);
      } catch { /* Selected-session reads still recover managed status. */ }
      try {
        const stop = await transport.listen("registered-projects-changed", () => {
          if (!cancelled) void refreshProjects(epoch).catch(() => {});
        });
        if (cancelled) { stop(); return; }
        unlisteners.push(stop);
      } catch { /* Focusing the window recovers cross-window registrations. */ }
      try {
        const stop = await listenForRelayStatus(() => {
          if (!cancelled) void refreshProjects(epoch).catch(() => {});
        }, transport.listen);
        if (cancelled) { stop(); return; }
        unlisteners.push(stop);
      } catch { /* Backend command checks also enforce reader ownership. */ }
      try {
        await refreshProjects(epoch);
      } catch (failure) { if (!cancelled) setError(errorMessage(failure)); }
      finally { if (!cancelled) setLoading(false); }
    })();
    const focus = () => { if (!cancelled) void refreshProjects(epoch).catch(() => {}); };
    window.addEventListener("focus", focus);
    return () => {
      cancelled = true; alive.current = false; lifetime.current += 1;
      for (const unlisten of unlisteners) unlisten();
      window.removeEventListener("focus", focus);
    };
  }, [accept, enabled, machine_key, refreshProjects, refreshRequest, transport]);

  useEffect(() => {
    if (!enabled || retry === 0) return;
    const epoch = lifetime.current;
    setLoading(true); setError(null);
    void refreshProjects(epoch).catch((failure) => {
      if (alive.current && lifetime.current === epoch) setError(errorMessage(failure));
    }).finally(() => { if (alive.current && lifetime.current === epoch) setLoading(false); });
  }, [enabled, retry, refreshProjects]);

  useEffect(() => {
    setManaged(null);
    if (!enabled || !selected_session_key) return;
    let cancelled = false;
    const epoch = lifetime.current;
    void getManagedSession({ session_key: selected_session_key }, transport.invoke).then((snapshot) => {
      if (!cancelled && snapshot) accept(snapshot, epoch);
    }).catch(() => { /* Historical sessions need no managed-process controls. */ });
    return () => { cancelled = true; };
  }, [accept, enabled, selected_session_key, transport]);

  const addProject = useCallback(async (request: RegisterProjectRequest) => {
    if (!enabled || !alive.current || active_transport.current !== transport || !workspace?.available) throw new Error("Project registration is available on this machine’s desktop app.");
    const epoch = lifetime.current;
    const project = await registerProject(request, transport.invoke);
    if (!alive.current || lifetime.current !== epoch) throw new Error("The selected machine changed. Reopen Add project to check its folders.");
    catalog_read.current += 1;
    setWorkspace((previous) => previous && { ...previous,
      projects: [...previous.projects.filter((entry) => entry.project_id !== project.project_id), project] });
    return project;
  }, [enabled, transport, workspace?.available]);

  const loadCreationOptions = useCallback((project_id: string) => getSessionCreationOptions(project_id, transport.invoke), [transport]);

  const control = useCallback(async (kind: "stop" | "resume", snapshot: ManagedSessionSnapshot) => {
    if (!enabled || !alive.current || active_transport.current !== transport || control_busy.current) return;
    control_busy.current = true; setControlling(true); setControlFailure(null);
    const epoch = lifetime.current;
    try {
      const result = await (kind === "stop" ? stopManagedSession : resumeManagedSession)({ request_id: snapshot.request_id }, transport.invoke);
      if (kind === "resume" && (!alive.current || lifetime.current !== epoch)) {
        void stopManagedSession({ request_id: snapshot.request_id }, transport.invoke).catch(() => {});
        return;
      }
      accept(result, epoch);
      if (kind === "resume" && alive.current && lifetime.current === epoch) setInputRefreshKey((value) => value + 1);
    } catch (failure) { if (alive.current && lifetime.current === epoch) setControlFailure({ request_id: snapshot.request_id, message: errorMessage(failure) }); }
    finally { if (alive.current && lifetime.current === epoch) { control_busy.current = false; setControlling(false); } }
  }, [accept, enabled, transport]);

  const respond = useCallback(async (request: ManagedSessionDialogResponse) => {
    if (!enabled || !alive.current || active_transport.current !== transport || control_busy.current) return;
    control_busy.current = true; setControlling(true); setControlFailure(null);
    const epoch = lifetime.current;
    try { accept(await respondManagedSessionDialog(request, transport.invoke), epoch); }
    catch (failure) { if (alive.current && lifetime.current === epoch) setControlFailure({ request_id: request.request_id, message: errorMessage(failure) }); }
    finally { if (alive.current && lifetime.current === epoch) { control_busy.current = false; setControlling(false); } }
  }, [accept, enabled, transport]);

  const controlErrorFor = useCallback((request_id?: string | null) => control_failure && control_failure.request_id === request_id ? control_failure.message : null, [control_failure]);

  return {
    workspace, loading, error, managed, pending_dialog_session, controlling, input_refresh_key,
    control_error: managed?.request_id === control_failure?.request_id ? control_failure?.message ?? null : null,
    dialog_error: pending_dialog_session?.request_id === control_failure?.request_id ? control_failure?.message ?? null : null,
    addProject, loadCreationOptions, control, respond, controlErrorFor,
    retry: () => setRetry((value) => value + 1),
  };
}
