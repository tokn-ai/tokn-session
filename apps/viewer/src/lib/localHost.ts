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
  change: (request?: StartLocalHostRequest) => Promise<void>;
}
export const LocalHostContext = createContext<LocalHostControls | undefined>(undefined);
export const useLocalHost = () => useContext(LocalHostContext);
export const isHosting = (status?: LocalHostStatus) => !!status && status.phase !== "stopped";
export const hostLabels: Record<LocalHostStatus["phase"], string> = {
  stopped: "Hosting off", connecting: "Connecting to Hub…", online: "Hosting online",
  reconnecting: "Reconnecting to Hub…", error: "Hosting needs attention",
};
