import type { ModelIdentity } from "./types";

/** Runtime input and model selection share one synchronous admission guard. */
export interface SessionMutationLock { send_busy: boolean; model_busy: boolean; model_uncertain?: boolean }

export function modelLabel(model: ModelIdentity): string {
  return model.display_name?.trim() || model.model_id;
}

export function sameModel(left: ModelIdentity | null | undefined, right: ModelIdentity | null | undefined): boolean {
  return !!left && !!right && left.model_id === right.model_id && left.model_provider === right.model_provider;
}

/** Identical IDs from different providers must remain distinguishable. */
export function modelChangeLabels(previous: ModelIdentity, current: ModelIdentity) {
  const include_provider = previous.model_id === current.model_id && previous.model_provider !== current.model_provider;
  const label = (model: ModelIdentity) => include_provider ? `${model.model_provider ?? "unknown"}/${modelLabel(model)}` : modelLabel(model);
  return { previous: label(previous), current: label(current) };
}
