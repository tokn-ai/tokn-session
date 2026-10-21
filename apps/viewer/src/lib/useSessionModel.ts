import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createUuid } from "./id";
import { getSessionModelStatus, listSessionModels, selectSessionModel } from "./tauri";
import { captureTransport, viewerStorageScope } from "./transport";
import { errorMessage } from "./state";
import { sameModel, type SessionMutationLock } from "./sessionModels";
import type { ModelChoice, ModelIdentity, SessionModelState, SessionModelStatus } from "./types";

interface ModelView {
  status: SessionModelStatus | null;
  state: SessionModelState | null;
  checking: boolean;
  loading: boolean;
  changing: boolean;
  uncertain: boolean;
  choices: ModelChoice[] | null;
  error: string | null;
}
const emptyView = (): ModelView => ({ status: null, state: null, checking: true, loading: false,
  changing: false, uncertain: false, choices: null, error: null });

/** One selected session owns requests and their runtime generation. Catalogs are lazy. */
export function useSessionModel(session_key: string | null, fallback: SessionModelState | undefined,
  lock: SessionMutationLock, running = false, refresh_key = 0) {
  const scope = viewerStorageScope();
  const owner = JSON.stringify([scope, session_key]);
  const transport = useMemo(captureTransport, [scope]);
  const [view, setView] = useState<{ owner: string; value: ModelView }>(() => ({ owner, value: emptyView() }));
  const [input_revision, setInputRevision] = useState(0);
  const latest_owner = useRef(owner);
  latest_owner.current = owner;
  const active = useRef(false);
  const epoch = useRef(0);
  const status_sequence = useRef(0);
  const catalog_sequence = useRef(0);
  const value = view.owner === owner ? view.value : emptyView();
  const latest = useRef(value);
  latest.current = value;
  const current = useRef({ running, lock });
  current.current = { running, lock };
  const update = useCallback((target: string, apply: (value: ModelView) => ModelView) => {
    if (!active.current || latest_owner.current !== target) return;
    const next = apply(latest.current);
    latest.current = next;
    setView({ owner: target, value: next });
  }, []);
  const refreshInputForModel = useCallback((next: SessionModelState) => {
    const previous = latest.current.state;
    if (previous && next.model && next.source === "live"
      && (previous.revision !== next.revision || !sameModel(previous.model, next.model))) {
      setInputRevision((revision) => revision + 1);
    }
  }, []);

  useEffect(() => {
    active.current = true;
    const lifetime = ++epoch.current;
    const stop = transport.on_close(() => {
      if (epoch.current !== lifetime) return;
      epoch.current += 1;
      update(owner, (old) => ({ ...old, checking: false, loading: false, changing: false,
        status: null, error: "Machine disconnected. Reconnect to change the model." }));
    });
    return () => { active.current = false; epoch.current += 1; stop(); };
  }, [owner, transport, update]);

  const refresh = useCallback(async () => {
    if (!session_key || current.current.lock.model_busy) return;
    const sequence = ++status_sequence.current;
    const lifetime = epoch.current;
    update(owner, (old) => ({ ...old, checking: true }));
    try {
      const status = await getSessionModelStatus({ session_key }, transport.invoke);
      if (lifetime !== epoch.current || latest_owner.current !== owner || sequence !== status_sequence.current) return;
      current.current.lock.model_uncertain = false;
      refreshInputForModel(status.state);
      update(owner, (old) => ({ ...old, status, state: status.state, checking: false, uncertain: false, error: null,
        choices: old.status?.runtime_generation === status.runtime_generation
          && old.status?.state.revision === status.state.revision ? old.choices : null }));
    } catch (error) {
      if (lifetime !== epoch.current || sequence !== status_sequence.current) return;
      update(owner, (old) => ({ ...old, checking: false, status: null,
        error: `Model selection is unavailable on this connection. ${errorMessage(error)}` }));
    }
  }, [owner, session_key, transport, update, refreshInputForModel]);

  useEffect(() => { void refresh(); }, [refresh, fallback?.revision, running, refresh_key]);

  const loadChoices = useCallback(async () => {
    const before = latest.current;
    if (!session_key || before.loading || before.changing || before.checking || !before.status?.can_list) return;
    const sequence = ++catalog_sequence.current;
    const status_version = status_sequence.current;
    const lifetime = epoch.current;
    update(owner, (old) => ({ ...old, loading: true, error: null }));
    try {
      const result = await listSessionModels({ session_key }, transport.invoke);
      if (lifetime !== epoch.current || latest_owner.current !== owner || sequence !== catalog_sequence.current) return;
      if (status_version !== status_sequence.current) {
        update(owner, (old) => ({ ...old, loading: false }));
        return;
      }
      refreshInputForModel(result.status.state);
      update(owner, (old) => ({ ...old, status: result.status, state: result.status.state,
        choices: result.models, loading: false }));
    } catch (error) {
      if (lifetime !== epoch.current || sequence !== catalog_sequence.current) return;
      update(owner, (old) => ({ ...old, loading: false, error: errorMessage(error) }));
    }
  }, [owner, session_key, transport, update, refreshInputForModel]);

  const select = useCallback(async (model: ModelIdentity) => {
    const before = latest.current;
    const guard = current.current.lock;
    if (!session_key || guard.send_busy || guard.model_busy || current.current.running || before.checking || before.loading
      || before.uncertain || before.error || !before.status?.can_select || !before.choices?.some((choice) => sameModel(choice.model, model))
      || sameModel(before.state?.model, model)) return;
    guard.model_busy = true;
    const lifetime = epoch.current;
    ++status_sequence.current;
    ++catalog_sequence.current;
    update(owner, (old) => ({ ...old, changing: true, error: null }));
    let attempted = false;
    try {
      const request_id = createUuid();
      attempted = true;
      const result = await selectSessionModel({ session_key, request_id,
        model_provider: model.model_provider, model_id: model.model_id,
        expected_revision: before.status.state.revision, runtime_generation: before.status.runtime_generation }, transport.invoke);
      if (lifetime !== epoch.current || latest_owner.current !== owner) return;
      if (result.request_id === request_id && result.status === "applied" && result.state) {
        update(owner, (old) => ({ ...old, state: result.state, changing: false,
          status: old.status ? { ...old.status, state: result.state! } : null, error: null }));
        setInputRevision((old) => old + 1);
      } else {
        const uncertain = result.request_id !== request_id || result.status !== "not_applied";
        guard.model_uncertain = uncertain;
        update(owner, (old) => ({ ...old, changing: false, uncertain,
          error: uncertain ? "Model change was not confirmed. Refresh model status before trying again."
            : result.message || "The model was not changed." }));
      }
    } catch (error) {
      guard.model_uncertain = attempted;
      if (lifetime === epoch.current) update(owner, (old) => ({ ...old, changing: false, uncertain: attempted,
        error: `${attempted ? "Model change was not confirmed. Refresh model status before trying again." : "The model was not changed."} ${errorMessage(error)}` }));
    } finally { guard.model_busy = false; }
  }, [owner, session_key, transport, update]);

  const display_state = !value.state?.model && fallback?.model ? fallback : value.state ?? fallback ?? null;
  return { ...value, state: display_state, input_revision,
    reason: running ? "Wait for the current response to finish before changing the model." : value.status?.reason,
    can_open: !!value.status?.can_list && !value.checking,
    can_select: !!value.status?.can_select && !value.checking && !value.loading && !value.changing && !value.uncertain && !value.error && !running,
    refresh, loadChoices, select };
}

export type SessionModelController = ReturnType<typeof useSessionModel>;
