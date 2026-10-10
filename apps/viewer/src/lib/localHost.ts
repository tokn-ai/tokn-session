import { createContext, useContext } from "react";
import type { LocalHostStatus, StartLocalHostRequest } from "./types";

export interface LocalHostControls {
  status?: LocalHostStatus;
  busy: boolean;
  error?: string;
  is_open: boolean;
  return_focus?: HTMLElement;
  open: (opener?: HTMLElement | null) => void;
  close: () => void;
  retry: () => void;
  stop_external: () => Promise<void>;
  change: (request?: StartLocalHostRequest) => Promise<void>;
}
export const LocalHostContext = createContext<LocalHostControls | undefined>(undefined);
export const useLocalHost = () => useContext(LocalHostContext);
export const isHosting = (status?: LocalHostStatus) => !!status && !["stopped", "external", "unavailable"].includes(status.phase);
export const hostLabels: Record<LocalHostStatus["phase"], string> = {
  external: "Hosting externally", unavailable: "Hosting status unavailable",
  stopped: "Hosting off", connecting: "Connecting to Hub…", online: "Hosting online",
  reconnecting: "Reconnecting to Hub…", error: "Hosting needs attention",
};
