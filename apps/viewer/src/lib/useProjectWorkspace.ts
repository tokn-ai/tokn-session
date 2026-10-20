import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { releaseImageUploads, uploadImageDrafts, type ImageDraft, type ImageUploadReceipt } from "./imageDrafts";
import { createUuid } from "./id";
import { errorMessage } from "./state";
import { captureTransport, type CommandInvoker, type EventSubscriber } from "./transport";
import {
  createProjectSession, getManagedSession, listRegisteredProjects, listenForManagedSessionChanges, listenForRelayStatus,
  registerProject, respondManagedSessionDialog, resumeManagedSession, stopManagedSession,
} from "./tauri";
import type {
  CreateProjectSessionRequest, ManagedSessionDialogResponse, ManagedSessionSnapshot,
  ProjectWorkspaceSnapshot, RegisterProjectRequest, SessionCreationError, SessionSummary,
} from "./types";

interface WorkspaceOptions {
  enabled: boolean;
  machine_key: string;
  selected_session_key: string | null;
  on_created: (session: SessionSummary) => void;
}
type WorkspaceTransport = { invoke: CommandInvoker; listen: EventSubscriber; release?: (command: string, payload?: Record<string, unknown>) => Promise<void> };
interface PendingManagedRead {
  epoch: number;
  transport: WorkspaceTransport;
  promise: Promise<ManagedSessionSnapshot | null>;
}
interface CreationAttempt {
  request: CreateProjectSessionRequest;
  images_key: string;
  uploads: ImageUploadReceipt[];
  release_uploads: () => Promise<void>;
  selection_version: number;
  opened: boolean;
}
function creationFailure(value: unknown): SessionCreationError | null {
  if (!value || typeof value !== "object") return null;
  const failure = value as Partial<SessionCreationError>;
  return typeof failure.message === "string" && typeof failure.may_have_started === "boolean"
    ? { message: failure.message, may_have_started: failure.may_have_started } : null;
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
  const [creation, setCreation] = useState<ManagedSessionSnapshot | null>(null);
  const [creation_error, setCreationError] = useState<string | null>(null);
  const [creation_uncertain, setCreationUncertain] = useState(false);
  const [creating, setCreating] = useState(false);
  const [managed, setManaged] = useState<ManagedSessionSnapshot | null>(null);
  const [pending_dialog_session, setPendingDialogSession] = useState<ManagedSessionSnapshot | null>(null);
  const [control_failure, setControlFailure] = useState<{ request_id: string; message: string } | null>(null);
  const [controlling, setControlling] = useState(false);
  const [input_refresh_key, setInputRefreshKey] = useState(0);
  const [retry, setRetry] = useState(0);
  const lifetime = useRef(0);
  const alive = useRef(false);
  const busy = useRef(false);
  const control_busy = useRef(false);
  const attempt = useRef<CreationAttempt | null>(null);
  const snapshots = useRef(new Map<string, ManagedSessionSnapshot>());
  const pending_reads = useRef(new Map<string, PendingManagedRead>());
  const catalog_read = useRef(0);
  const current = useRef(options);
  current.current = options;
  const selection = useRef({ key: selected_session_key, version: 0 });
  if (selection.current.key !== selected_session_key) {
    selection.current = { key: selected_session_key, version: selection.current.version + 1 };
  }

  const accept = useCallback((snapshot: ManagedSessionSnapshot, epoch: number) => {
    if (!alive.current || lifetime.current !== epoch) return;
    const old = snapshots.current.get(snapshot.request_id);
    if (old && old.revision > snapshot.revision) return;
    // A GET may enrich an event at the same revision with its indexed session.
    const next = old?.revision === snapshot.revision && old.session && !snapshot.session
      ? { ...snapshot, session: old.session } : snapshot;
    snapshots.current.set(next.request_id, next);
    setPendingDialogSession([...snapshots.current.values()].find((entry) => entry.pending_dialog) ?? null);
    if (next.session_key && next.session_key === current.current.selected_session_key) setManaged(next);
    const pending = attempt.current;
    if (pending?.request.request_id !== next.request_id) return;
    setCreation(next);
    // A managed receipt means the host retained its admitted attachments.
    // Releasing their staging tokens cannot remove provider history images.
    void pending.release_uploads();
    if (next.session && !pending.opened && pending.selection_version === selection.current.version) {
      pending.opened = true;
      current.current.on_created(next.session);
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
    setWorkspace(null); setCreation(null); setManaged(null); setPendingDialogSession(null);
    setCreationError(null); setCreationUncertain(false); setControlFailure(null); setError(null);
    snapshots.current.clear(); void attempt.current?.release_uploads(); attempt.current = null;
    busy.current = false; control_busy.current = false;
    setCreating(false); setControlling(false); setLoading(enabled);
    if (!enabled) return;
    const unlisteners: (() => void)[] = [];
    let cancelled = false;
    void (async () => {
      try {
        const stop = await listenForManagedSessionChanges((snapshot) => {
          accept(snapshot, epoch);
          const requested = attempt.current?.request.request_id === snapshot.request_id;
          if (snapshot.session_key && !snapshot.session && (requested || snapshot.session_key === current.current.selected_session_key)) {
            void refreshRequest(snapshot.request_id, epoch).catch(() => {});
          }
        }, transport.listen);
        if (cancelled) { stop(); return; }
        unlisteners.push(stop);
      } catch { /* Explicit refresh and bounded creation polling still recover state. */ }
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
      const pending = attempt.current;
      void pending?.release_uploads();
      if (pending && !pending.opened) {
        void stopManagedSession({ request_id: pending.request.request_id }, transport.invoke).catch(() => {});
      }
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

  useEffect(() => {
    if (!enabled || !creation || creation.session || creation.status === "failed" || creation.status === "stopped") return;
    const epoch = lifetime.current;
    // Pi creates its durable file after the first assistant reply. Poll only
    // this pending identity; no catalog or history scans originate in the UI.
    const timer = window.setInterval(() => { void refreshRequest(creation.request_id, epoch).catch(() => {}); }, 1000);
    return () => window.clearInterval(timer);
  }, [creation, enabled, refreshRequest]);

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

  const createSession = useCallback(async (project_id: string, prompt: string, images: ImageDraft[] = []) => {
    if (!enabled || !alive.current || active_transport.current !== transport || !workspace?.available || busy.current || (!prompt.trim() && !images.length)) return;
    busy.current = true; setCreating(true); setCreationError(null); setCreationUncertain(false);
    const epoch = lifetime.current;
    const images_key = images.map((image) => image.draft_id).join(";");
    let pending = attempt.current;
    const previous = pending && snapshots.current.get(pending.request.request_id);
    if (!pending || pending.opened || previous?.status === "failed" || previous?.status === "stopped") {
      let request_id: string;
      try { request_id = createUuid(); }
      catch (failure) { busy.current = false; setCreating(false); setCreationError(errorMessage(failure)); return; }
      void pending?.release_uploads();
      const uploads: ImageUploadReceipt[] = [];
      pending = { request: { request_id, project_id, prompt }, images_key, uploads,
        release_uploads: () => releaseImageUploads(uploads, transport.release ?? transport.invoke),
        selection_version: selection.current.version, opened: false };
      attempt.current = pending;
      setCreation(null);
    } else if (pending.request.project_id !== project_id || pending.request.prompt !== prompt || pending.images_key !== images_key) {
      setCreationError("Check the pending session before changing its folder or prompt.");
      busy.current = false; setCreating(false); return;
    }
    const request = pending.request;
    // An uncertain earlier attempt remains uncertain even if its status read fails.
    let provider_attempted = creation_uncertain;
    try {
      if (creation_uncertain) {
        const existing = await refreshRequest(request.request_id, epoch);
        if (existing) return;
      }
      if (images.length && !request.image_ids) {
        request.image_ids = await uploadImageDrafts(images, { type: "project", project_id }, transport.invoke,
          () => alive.current && lifetime.current === epoch && active_transport.current === transport, (receipt) => pending.uploads.push(receipt));
      }
      if (!alive.current || lifetime.current !== epoch || active_transport.current !== transport) return;
      provider_attempted = true;
      const snapshot = await createProjectSession(request, transport.invoke);
      if (!alive.current || lifetime.current !== epoch) {
        void stopManagedSession({ request_id: request.request_id }, transport.invoke).catch(() => {});
        return;
      }
      accept(snapshot, epoch);
      if (!snapshot.session) await refreshRequest(request.request_id, epoch);
    } catch (failure) {
      if (alive.current && lifetime.current === epoch) {
        const known = creationFailure(failure);
        if (!provider_attempted || known && !known.may_have_started) {
          await pending.release_uploads();
          if (attempt.current?.request.request_id === request.request_id) attempt.current = null;
          setCreation(null); setCreationError(known?.message ?? errorMessage(failure));
        } else {
          setCreationUncertain(true);
          setCreationError(`Session start was not confirmed. ${known?.message ?? errorMessage(failure)} Retry checks the same request.`);
        }
      }
    } finally {
      if (!alive.current || lifetime.current !== epoch) await pending.release_uploads();
      if (alive.current && lifetime.current === epoch) { busy.current = false; setCreating(false); }
    }
  }, [accept, creation_uncertain, enabled, refreshRequest, transport, workspace?.available]);

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

  return {
    workspace, loading, error, creation, creation_error, creation_uncertain, creating, managed, pending_dialog_session, controlling, input_refresh_key,
    control_error: managed?.request_id === control_failure?.request_id ? control_failure?.message ?? null : null,
    creation_control_error: creation?.request_id === control_failure?.request_id ? control_failure?.message ?? null : null,
    dialog_error: pending_dialog_session?.request_id === control_failure?.request_id ? control_failure?.message ?? null : null,
    addProject, createSession, control, respond,
    retry: () => setRetry((value) => value + 1),
    beginCreation: () => {
      const pending = attempt.current;
      if (!pending || pending.opened) { void pending?.release_uploads(); attempt.current = null; setCreation(null); setCreationError(null); setCreationUncertain(false); }
    },
    dismissCreation: () => { if (attempt.current) attempt.current.selection_version = -1; },
    openCreation: () => {
      const pending = attempt.current;
      const snapshot = pending && snapshots.current.get(pending.request.request_id);
      if (pending && snapshot?.session) {
        pending.opened = true;
        current.current.on_created(snapshot.session);
      }
    },
  };
}
