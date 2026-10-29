import { createContext, useCallback, useContext, useMemo, useRef, type ReactNode } from "react";
import { useSessionDrafts } from "../lib/useSessionDrafts";
import type { CommandInvoker, EventSubscriber } from "../lib/transport";
import type { SessionSummary } from "../lib/types";

type CreatedListener = (draft_id: string, session: SessionSummary) => void;
type LocalSessionDrafts = ReturnType<typeof useSessionDrafts> & {
  subscribeCreated: (listener: CreatedListener) => () => void;
};

const SessionDraftsContext = createContext<LocalSessionDrafts | null>(null);

// Draft creation belongs to this computer even when the global viewer transport
// is displaying a remote machine. Load native APIs only when the owner uses them.
const invokeLocal: CommandInvoker = async (command, payload) => {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke(command, payload);
};
const listenLocal: EventSubscriber = async (event, handler) => {
  const { listen } = await import("@tauri-apps/api/event");
  return listen(event, handler);
};
const local_transport = {
  invoke: invokeLocal,
  listen: listenLocal,
  release: async (command: string, payload?: Record<string, unknown>) => {
    await invokeLocal(command, payload);
  },
};

/** Memory-only drafts outlive machine navigation, but end with this app window. */
export function SessionDraftsProvider({ children, enabled }: { children: ReactNode; enabled: boolean }) {
  const listeners = useRef(new Set<CreatedListener>());
  const onCreated = useCallback((draft_id: string, session: SessionSummary) => {
    for (const listener of listeners.current) listener(draft_id, session);
  }, []);
  const subscribeCreated = useCallback((listener: CreatedListener) => {
    listeners.current.add(listener);
    return () => { listeners.current.delete(listener); };
  }, []);
  const controller = useSessionDrafts({ enabled, machine_key: "local", on_created: onCreated }, local_transport);
  const value = useMemo(() => ({ ...controller, subscribeCreated }), [controller, subscribeCreated]);
  return <SessionDraftsContext.Provider value={value}>{children}</SessionDraftsContext.Provider>;
}

export function useLocalSessionDrafts() {
  return useContext(SessionDraftsContext);
}
