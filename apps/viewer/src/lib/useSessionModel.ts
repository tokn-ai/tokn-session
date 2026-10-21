import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getSessionModelStatus, listSessionModels } from "./tauri";
import { captureTransport, viewerStorageScope } from "./transport";
import { errorMessage } from "./state";
import { sameModel } from "./sessionModels";
import type { ModelChoice, ModelIdentity, SessionModelState, SessionModelStatus } from "./types";

interface ModelView {
  status: SessionModelStatus | null;
  state: SessionModelState | null;
  checking: boolean;
  loading: boolean;
  choices: ModelChoice[] | null;
  error: string | null;
}
const emptyView = (): ModelView => ({ status: null, state: null, checking: true, loading: false, choices: null, error: null });

/** Runtime state is read-only here. Model choices belong to the message draft. */
export function useSessionModel(session_key: string | null, fallback: SessionModelState | undefined,
  running = false, refresh_key = 0) {
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
  const response_epoch = epoch.current;
  const response_generation = value.status?.runtime_generation;
  const latest = useRef(value);
  latest.current = value;
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
      update(owner, (old) => ({ ...old, checking: false, loading: false,
        status: null, error: "Machine disconnected. Reconnect to choose a model." }));
    });
    return () => { active.current = false; epoch.current += 1; stop(); };
  }, [owner, transport, update]);

  const refresh = useCallback(async () => {
    if (!session_key) return;
    const sequence = ++status_sequence.current;
    const lifetime = epoch.current;
    update(owner, (old) => ({ ...old, checking: true }));
    try {
      const status = await getSessionModelStatus({ session_key }, transport.invoke);
      if (lifetime !== epoch.current || latest_owner.current !== owner || sequence !== status_sequence.current) return;
      refreshInputForModel(status.state);
      update(owner, (old) => ({ ...old, status, state: status.state, checking: false, error: null,
        choices: old.status?.runtime_generation === status.runtime_generation ? old.choices : null }));
    } catch (error) {
      if (lifetime !== epoch.current || sequence !== status_sequence.current) return;
      update(owner, (old) => ({ ...old, checking: false, status: null,
        error: `Model selection is unavailable on this connection. ${errorMessage(error)}` }));
    }
  }, [owner, session_key, transport, update, refreshInputForModel]);

  useEffect(() => { void refresh(); }, [refresh, fallback?.revision, running, refresh_key]);

  const loadChoices = useCallback(async () => {
    const before = latest.current;
    if (!session_key || before.loading || before.checking || !before.status?.can_list || before.status.supports_model_input !== true) return;
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

  const acceptState = useCallback((state: SessionModelState, refresh_input = false) => {
    if (!active.current || latest_owner.current !== owner || epoch.current !== response_epoch
      || latest.current.status?.runtime_generation !== response_generation) return false;
    ++status_sequence.current;
    ++catalog_sequence.current;
    if (refresh_input) setInputRevision((revision) => revision + 1);
    else refreshInputForModel(state);
    update(owner, (old) => ({ ...old, state, checking: false, loading: false,
      status: old.status ? { ...old.status, state } : null }));
    return true;
  }, [owner, response_epoch, response_generation, refreshInputForModel, update]);

  const choiceAllowed = useCallback((model: ModelIdentity) => {
    const current = latest.current;
    return active.current && latest_owner.current === owner && !current.checking && !current.loading && !current.error
      && current.status?.supports_model_input === true && current.status.can_list
      && current.choices?.some((choice) => sameModel(choice.model, model)) === true;
  }, [owner]);

  const display_state = !value.state?.model && fallback?.model ? fallback : value.state ?? fallback ?? null;
  const supported = value.status?.supports_model_input === true;
  return { ...value, state: display_state, input_revision, running,
    reason: value.status?.can_list && !supported ? "Update this host to choose a model when sending a message."
      : supported && value.status?.can_list ? null : value.status?.reason,
    can_open: supported && !!value.status?.can_list && !value.checking,
    can_select: supported && !!value.status?.can_list && !value.checking && !value.loading && !value.error,
    refresh, loadChoices, acceptState, choiceAllowed };
}

export type SessionModelController = ReturnType<typeof useSessionModel>;
