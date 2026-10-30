import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { sameModel } from "./sessionModels";
import { errorMessage } from "./state";
import type { ListSessionCreationModelsRequest, ModelChoice, SessionCreationModels } from "./types";

export interface SessionDraftModel {
  choice: ModelChoice;
  catalog_revision: string;
  runtime_profile_id: string | null;
  runtime_revision: number | null;
  runtime_plan_id?: string | null;
  scope_key: string;
}

export function creationModelScope(request: ListSessionCreationModelsRequest, default_profile_id: string | null, default_revision: number | null): string {
  return JSON.stringify([request.project_id, request.provider, request.runtime_profile_id ?? null, request.runtime_revision ?? null,
    request.runtime_profile_id ? null : default_profile_id, request.runtime_profile_id ? null : default_revision, request.runtime_plan_id ?? null]);
}

interface CatalogView {
  loading: boolean;
  result: SessionCreationModels | null;
  error: string | null;
}
const emptyView = (): CatalogView => ({ loading: false, result: null, error: null });

/** Catalog reads never create a session. Their proof belongs to the first-message draft. */
export function useSessionCreationModels(request: ListSessionCreationModelsRequest, scope_key: string, enabled: boolean,
  load: (request: ListSessionCreationModelsRequest) => Promise<SessionCreationModels>) {
  const owner = useMemo(() => ({}), [scope_key, enabled, load]);
  const [view, setView] = useState<{ owner: object; value: CatalogView }>(() => ({ owner, value: emptyView() }));
  const latest_owner = useRef(owner);
  latest_owner.current = owner;
  const value = view.owner === owner ? view.value : emptyView();
  const latest = useRef(value);
  latest.current = value;
  const request_ref = useRef(request);
  request_ref.current = request;
  const active = useRef(false);
  const epoch = useRef(0);
  useEffect(() => {
    active.current = true;
    ++epoch.current;
    return () => { active.current = false; ++epoch.current; };
  }, [owner]);
  const update = useCallback((next: CatalogView) => {
    if (!active.current || latest_owner.current !== owner) return;
    latest.current = next;
    setView({ owner, value: next });
  }, [owner]);
  const loadChoices = useCallback(async () => {
    if (!enabled || !active.current || latest_owner.current !== owner || latest.current.loading) return;
    const lifetime = epoch.current;
    update({ ...latest.current, loading: true, error: null });
    try {
      const target = request_ref.current;
      const result = await load(target);
      if (lifetime !== epoch.current) return;
      if (target.runtime_plan_id && result.runtime_plan_id !== target.runtime_plan_id) {
        throw new Error("The runtime configuration changed while loading models. Refresh the agent configuration before choosing a model.");
      }
      update({ loading: false, result, error: null });
    } catch (failure) {
      if (lifetime !== epoch.current) return;
      update({ ...latest.current, loading: false, error: errorMessage(failure) });
    }
  }, [enabled, load, owner, update]);
  const selectionFor = useCallback((choice: ModelChoice): SessionDraftModel | null => {
    const current = latest.current;
    const verified = current.result?.models.find((entry) => sameModel(entry.model, choice.model));
    if (!enabled || !active.current || latest_owner.current !== owner || current.loading || current.error
      || !current.result?.catalog_revision || !verified) return null;
    return { choice: verified, catalog_revision: current.result.catalog_revision, scope_key,
      runtime_profile_id: current.result.runtime_profile_id, runtime_revision: current.result.runtime_revision,
      ...(current.result.runtime_plan_id !== undefined ? { runtime_plan_id: current.result.runtime_plan_id } : {}) };
  }, [enabled, owner, scope_key]);
  const selectionValid = (selected: SessionDraftModel | null) => !selected || selected.scope_key === scope_key && !value.loading && !value.error
    && (!value.result || value.result.catalog_revision === selected.catalog_revision
      && (value.result.runtime_plan_id ?? null) === (selected.runtime_plan_id ?? null)
      && value.result.models.some((choice) => sameModel(choice.model, selected.choice.model)));

  return {
    state: null, checking: false, loading: value.loading, choices: value.result?.models ?? null, error: value.error,
    reason: value.result?.reason ?? (!enabled ? "Choose an available project, agent and runtime first." : null),
    can_open: enabled, can_select: enabled && !value.loading && !value.error && !!value.result?.catalog_revision,
    refresh: loadChoices, loadChoices, selectionFor, selectionValid,
  };
}
