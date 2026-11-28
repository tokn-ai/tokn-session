import { createContext, useContext, useLayoutEffect, useState, type ReactNode } from "react";
import { disposeImageDrafts, type ImageDraft } from "./imageDrafts";
import type { ModelChoice, SessionInputStatus } from "./types";

interface InputState {
  control_revision?: string;
  control_binding?: string;
  draft: string;
  model_draft: ModelChoice | null;
  images: ImageDraft[];
  availability: SessionInputStatus | null;
  availability_error: boolean;
  checking: boolean;
  checking_connection: boolean;
  check_id: number;
  sending: boolean;
  resuming: boolean;
  delivery_uncertain: boolean;
  notice: string | null;
}

const EMPTY_INPUT: InputState = {
  draft: "", model_draft: null, images: [], availability: null, availability_error: false, checking: true, checking_connection: false, check_id: 0,
  sending: false, resuming: false, delivery_uncertain: false, notice: null,
};

/** One session workspace owns drafts and acknowledgements while the editor is away. */
class SessionInputStore {
  private states = new Map<string, InputState>();
  private listeners = new Set<() => void>();
  next_check = 0;
  active = true;
  on_accepted?: (session_key: string, refresh_model: boolean) => void | Promise<void>;

  get(key: string | null) {
    return key === null ? EMPTY_INPUT : this.states.get(key) ?? EMPTY_INPUT;
  }

  subscribe = (handler: () => void) => {
    this.listeners.add(handler);
    return () => { this.listeners.delete(handler); };
  };

  update = (key: string, apply: (previous: InputState) => InputState) => {
    if (!this.active) return;
    const previous = this.get(key);
    const next = apply(previous);
    if (previous === next) return;
    this.states.set(key, next);
    for (const handler of this.listeners) handler();
  };

  dispose() {
    this.active = false;
    this.on_accepted = undefined;
    for (const state of this.states.values()) disposeImageDrafts(state.images);
    this.states.clear();
  }
}

const InputWorkspace = createContext<SessionInputStore | null>(null);

function useOwnedInputStore(shared: SessionInputStore | null) {
  const [store] = useState(() => shared ?? new SessionInputStore());
  useLayoutEffect(() => {
    if (shared) return;
    store.active = true;
    return () => store.dispose();
  }, [shared, store]);
  return shared ?? store;
}

export function SessionInputWorkspace({ children }: { children: ReactNode }) {
  const store = useOwnedInputStore(null);
  return <InputWorkspace.Provider value={store}>{children}</InputWorkspace.Provider>;
}

export function useSessionInputStore() {
  return useOwnedInputStore(useContext(InputWorkspace));
}
