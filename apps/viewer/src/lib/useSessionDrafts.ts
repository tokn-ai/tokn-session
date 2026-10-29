import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  addImageDrafts, DEFAULT_IMAGE_LIMITS, disposeImageDrafts, releaseImageUploads, uploadImageDrafts,
  type ImageDraft, type ImageUploadReceipt,
} from "./imageDrafts";
import { createUuid } from "./id";
import { errorMessage } from "./state";
import { captureTransport, type CommandInvoker, type EventSubscriber } from "./transport";
import { createProjectSession, getManagedSession, listenForManagedSessionChanges } from "./tauri";
import type { CreateProjectSessionRequest, ManagedSessionSnapshot, SessionCreationError, SessionCreationSelection, SessionSummary } from "./types";
import type { SessionDraftModel } from "./useSessionCreationModels";

/** App-only identity. Never pass draft_id to native session/history APIs. */
export interface SessionDraft {
  draft_id: string;
  project_id: string;
  selection: SessionCreationSelection;
  model: SessionDraftModel | null;
  prompt: string;
  images: ImageDraft[];
  snapshot: ManagedSessionSnapshot | null;
  sending: boolean;
  uncertain: boolean;
  locked: boolean;
  can_discard: boolean;
  error: string | null;
  image_error: string | null;
}
export interface SessionDraftPatch {
  project_id?: string;
  selection?: SessionCreationSelection;
  prompt?: string;
  model?: SessionDraftModel | null;
}
interface DraftOptions {
  enabled: boolean;
  machine_key: string;
  on_created: (draft_id: string, session: SessionSummary) => void;
}
type DraftTransport = { invoke: CommandInvoker; listen: EventSubscriber; release?: (command: string, payload?: Record<string, unknown>) => Promise<void> };
interface DraftAttempt {
  request: CreateProjectSessionRequest;
  uploads: ImageUploadReceipt[];
  release_uploads: () => Promise<void>;
}
function creationFailure(value: unknown): SessionCreationError | null {
  if (!value || typeof value !== "object") return null;
  const failure = value as Partial<SessionCreationError>;
  return typeof failure.message === "string" && typeof failure.may_have_started === "boolean"
    ? { message: failure.message, may_have_started: failure.may_have_started } : null;
}
function uncertain(snapshot: ManagedSessionSnapshot): boolean {
  return snapshot.creation_input_state === "uncertain" || snapshot.creation_phase === "native_create_uncertain"
    || snapshot.creation_phase === "first_input_uncertain";
}
function terminal(snapshot: ManagedSessionSnapshot): boolean {
  return snapshot.status === "failed" || snapshot.status === "stopped" || snapshot.creation_phase === "failed";
}
function canDiscard(snapshot: ManagedSessionSnapshot): boolean {
  return (terminal(snapshot) || snapshot.creation_input_state === "rejected") && !uncertain(snapshot);
}

/** Own drafts and their creation receipts independently of the selected conversation. */
export function useSessionDrafts(options: DraftOptions, transport_override?: DraftTransport) {
  const { enabled, machine_key } = options;
  const transport = useMemo(() => transport_override ?? captureTransport(), [machine_key, transport_override]);
  const current = useRef(options);
  current.current = options;
  const active_transport = useRef(transport);
  active_transport.current = transport;
  const [drafts, setDrafts] = useState<SessionDraft[]>([]);
  const records = useRef(new Map<string, SessionDraft>());
  const attempts = useRef(new Map<string, DraftAttempt>());
  const reads = useRef(new Map<string, Promise<ManagedSessionSnapshot | null>>());
  const lifetime = useRef(0);
  const alive = useRef(false);
  const publish = useCallback(() => setDrafts([...records.current.values()]), []);
  const isCurrent = useCallback((epoch: number) => alive.current && lifetime.current === epoch && active_transport.current === transport, [transport]);

  const accept = useCallback((snapshot: ManagedSessionSnapshot, epoch: number) => {
    if (!isCurrent(epoch)) return;
    const entry = [...attempts.current].find(([, attempt]) => attempt.request.request_id === snapshot.request_id);
    if (!entry) return;
    const [draft_id, attempt] = entry;
    const draft = records.current.get(draft_id);
    if (!draft || draft.snapshot && draft.snapshot.revision > snapshot.revision) return;
    const next = draft.snapshot?.revision === snapshot.revision && draft.snapshot.session && !snapshot.session
      ? { ...snapshot, session: draft.snapshot.session } : snapshot;
    // A receipt means the host retained any admitted images, independently of
    // upload staging tokens. Local previews remain until input is confirmed.
    void attempt.release_uploads();
    if (next.session && next.creation_input_state === "accepted") {
      attempts.current.delete(draft_id);
      records.current.delete(draft_id);
      disposeImageDrafts(draft.images);
      publish();
      current.current.on_created(draft_id, next.session);
      return;
    }
    const is_uncertain = uncertain(next);
    records.current.set(draft_id, { ...draft, snapshot: next, uncertain: is_uncertain,
      can_discard: !draft.sending && canDiscard(next),
      error: next.message ?? (next.creation_input_state === "rejected" ? "The session was created, but its first message was rejected. Your draft has been retained." : null) });
    publish();
  }, [isCurrent, publish]);

  const refreshRequest = useCallback((draft_id: string, epoch: number) => {
    const attempt = attempts.current.get(draft_id);
    if (!attempt) return Promise.resolve(null);
    const key = `${epoch}:${attempt.request.request_id}`;
    const previous = reads.current.get(key);
    if (previous) return previous;
    const promise = Promise.resolve().then(() => getManagedSession({ request_id: attempt.request.request_id }, transport.invoke))
      .then((snapshot) => { if (snapshot) accept(snapshot, epoch); return snapshot; })
      .finally(() => { if (reads.current.get(key) === promise) reads.current.delete(key); });
    reads.current.set(key, promise);
    return promise;
  }, [accept, transport]);

  useEffect(() => {
    const epoch = ++lifetime.current;
    alive.current = enabled;
    records.current.clear(); attempts.current.clear(); reads.current.clear(); publish();
    if (!enabled) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void listenForManagedSessionChanges((snapshot) => {
      if (!isCurrent(epoch)) return;
      accept(snapshot, epoch);
      if (snapshot.session_key && !snapshot.session) {
        const entry = [...attempts.current].find(([, attempt]) => attempt.request.request_id === snapshot.request_id);
        if (entry) void refreshRequest(entry[0], epoch).catch(() => {});
      }
    }, transport.listen).then((stop) => { if (cancelled) stop(); else unlisten = stop; }).catch(() => {
      // Receipt polling remains available if native event subscription fails.
    });
    const timer = window.setInterval(() => {
      for (const [draft_id, draft] of records.current) {
        if (draft.snapshot && !terminal(draft.snapshot) && draft.snapshot.creation_phase !== "native_create_uncertain"
          && draft.snapshot.creation_input_state !== "rejected") void refreshRequest(draft_id, epoch).catch(() => {});
      }
    }, 1000);
    return () => {
      cancelled = true; alive.current = false; lifetime.current += 1;
      unlisten?.(); window.clearInterval(timer);
      for (const draft of records.current.values()) disposeImageDrafts(draft.images);
      for (const attempt of attempts.current.values()) void attempt.release_uploads();
      records.current.clear(); attempts.current.clear(); reads.current.clear();
      // Leaving the view must not cancel an already admitted native session.
    };
  }, [accept, enabled, isCurrent, machine_key, publish, refreshRequest, transport]);

  const createDraft = useCallback((project_id: string): string => {
    if (!enabled || !isCurrent(lifetime.current)) return "";
    const draft_id = createUuid();
    records.current.set(draft_id, { draft_id, project_id, selection: { provider: "pi" }, model: null, prompt: "", images: [],
      snapshot: null, sending: false, uncertain: false, locked: false, can_discard: true, error: null, image_error: null });
    publish();
    return draft_id;
  }, [enabled, isCurrent, publish]);

  const updateDraft = useCallback((draft_id: string, patch: SessionDraftPatch) => {
    const draft = records.current.get(draft_id);
    if (!draft || draft.locked || !isCurrent(lifetime.current)) return;
    const selection = patch.selection ? { ...patch.selection } : patch.project_id !== undefined && patch.project_id !== draft.project_id
      ? { provider: draft.selection.provider } : draft.selection;
    const scope_changed = patch.project_id !== undefined && patch.project_id !== draft.project_id
      || selection.provider !== draft.selection.provider
      || (selection.runtime_profile_id ?? null) !== (draft.selection.runtime_profile_id ?? null)
      || (selection.runtime_revision ?? null) !== (draft.selection.runtime_revision ?? null);
    records.current.set(draft_id, { ...draft, ...patch, selection, model: scope_changed ? null : patch.model === undefined ? draft.model : patch.model, error: null });
    publish();
  }, [isCurrent, publish]);

  const addImages = useCallback((draft_id: string, files: File[]) => {
    const draft = records.current.get(draft_id);
    if (!draft || draft.locked || !isCurrent(lifetime.current)) return;
    const result = addImageDrafts(draft.images, files, DEFAULT_IMAGE_LIMITS);
    records.current.set(draft_id, { ...draft, images: result.images, image_error: result.error });
    publish();
  }, [isCurrent, publish]);

  const removeImage = useCallback((draft_id: string, image_id: string) => {
    const draft = records.current.get(draft_id);
    if (!draft || draft.locked || !isCurrent(lifetime.current)) return;
    disposeImageDrafts(draft.images.filter((image) => image.draft_id === image_id));
    records.current.set(draft_id, { ...draft, images: draft.images.filter((image) => image.draft_id !== image_id), image_error: null });
    publish();
  }, [isCurrent, publish]);

  const discardDraft = useCallback((draft_id: string) => {
    const draft = records.current.get(draft_id);
    if (!draft?.can_discard || draft.sending || !isCurrent(lifetime.current)) return;
    disposeImageDrafts(draft.images);
    void attempts.current.get(draft_id)?.release_uploads();
    attempts.current.delete(draft_id); records.current.delete(draft_id); publish();
  }, [isCurrent, publish]);

  const checkDraft = useCallback(async (draft_id: string) => {
    const draft = records.current.get(draft_id);
    if (!draft || draft.sending || !attempts.current.has(draft_id) || !isCurrent(lifetime.current)) return;
    const epoch = lifetime.current;
    records.current.set(draft_id, { ...draft, sending: true, can_discard: false, error: null }); publish();
    try {
      const snapshot = await refreshRequest(draft_id, epoch);
      const latest = records.current.get(draft_id);
      if (!snapshot && latest && isCurrent(epoch)) {
        records.current.set(draft_id, { ...latest, uncertain: true,
          error: "No creation receipt is available yet. Check again; the first message will not be sent twice." });
      }
    } catch (failure) {
      const latest = records.current.get(draft_id);
      if (latest && isCurrent(epoch)) records.current.set(draft_id, { ...latest, error: `Could not check session creation. ${errorMessage(failure)}` });
    } finally {
      const latest = records.current.get(draft_id);
      if (latest && isCurrent(epoch)) {
        records.current.set(draft_id, { ...latest, sending: false,
          can_discard: !!latest.snapshot && canDiscard(latest.snapshot) }); publish();
      }
    }
  }, [isCurrent, publish, refreshRequest]);

  const sendDraft = useCallback(async (draft_id: string) => {
    const draft = records.current.get(draft_id);
    if (!draft || draft.sending || !enabled || !isCurrent(lifetime.current)) return;
    if (draft.locked) { await checkDraft(draft_id); return; }
    if (!draft.project_id || !draft.prompt.trim() && !draft.images.length) return;
    const epoch = lifetime.current;
    let request: CreateProjectSessionRequest;
    try { request = { request_id: createUuid(), project_id: draft.project_id, prompt: draft.prompt, ...draft.selection,
      ...(draft.model ? { model: { model_provider: draft.model.choice.model.model_provider,
        model_id: draft.model.choice.model.model_id, catalog_revision: draft.model.catalog_revision,
        runtime_profile_id: draft.model.runtime_profile_id, runtime_revision: draft.model.runtime_revision } } : {}) }; }
    catch (failure) { records.current.set(draft_id, { ...draft, error: errorMessage(failure) }); publish(); return; }
    const uploads: ImageUploadReceipt[] = [];
    const attempt: DraftAttempt = { request, uploads, release_uploads: () => releaseImageUploads(uploads, transport.release ?? transport.invoke) };
    attempts.current.set(draft_id, attempt);
    // Update the authoritative map synchronously: two clicks in one render
    // cannot admit two creates or alter the frozen first-message fingerprint.
    records.current.set(draft_id, { ...draft, sending: true, locked: true, can_discard: false, error: null }); publish();
    let provider_attempted = false;
    try {
      if (draft.images.length) request.image_ids = await uploadImageDrafts(draft.images, { type: "project", project_id: draft.project_id }, transport.invoke,
        () => isCurrent(epoch), (receipt) => uploads.push(receipt));
      if (!isCurrent(epoch)) return;
      provider_attempted = true;
      accept(await createProjectSession(request, transport.invoke), epoch);
      if (records.current.has(draft_id) && isCurrent(epoch)) await refreshRequest(draft_id, epoch);
    } catch (failure) {
      const latest = records.current.get(draft_id);
      if (latest && isCurrent(epoch)) {
        const known = creationFailure(failure);
        // A live receipt wins over a failed command/status acknowledgement.
        const rejected_before_admission = !latest.snapshot && (!provider_attempted || known?.may_have_started === false);
        if (rejected_before_admission) {
          await attempt.release_uploads();
          const after_cleanup = records.current.get(draft_id);
          if (isCurrent(epoch) && after_cleanup && !after_cleanup.snapshot && attempts.current.get(draft_id) === attempt) {
            attempts.current.delete(draft_id);
            records.current.set(draft_id, { ...after_cleanup, locked: false, uncertain: false, can_discard: true,
              error: known?.message ?? errorMessage(failure) });
          }
        } else {
          records.current.set(draft_id, { ...latest, uncertain: !latest.snapshot || uncertain(latest.snapshot),
            error: `Session start was not confirmed. ${known?.message ?? errorMessage(failure)} Check status to inspect the same request.` });
        }
      }
    } finally {
      if (!isCurrent(epoch)) await attempt.release_uploads();
      const latest = records.current.get(draft_id);
      if (latest && isCurrent(epoch)) {
        records.current.set(draft_id, { ...latest, sending: false,
          can_discard: !latest.locked || !!latest.snapshot && canDiscard(latest.snapshot) }); publish();
      }
    }
  }, [accept, checkDraft, enabled, isCurrent, publish, refreshRequest, transport]);

  return { drafts, createDraft, updateDraft, addImages, removeImage, discardDraft, sendDraft, checkDraft };
}
