import { useEffect, useMemo, useRef, useState } from "react";
import type { RegisteredProject, SessionOrder, SessionSummary } from "./types";
import { reconcileRecent, sidebarGroups, type RecentEntry } from "./sidebarOrder";

const NO_PROJECTS: RegisteredProject[] = [];
export function useSidebarGroups(sessions: SessionSummary[], order: SessionOrder, projects: RegisteredProject[] = NO_PROJECTS) {
  const [now, setNow] = useState(Date.now);
  const recent = useRef<RecentEntry[]>([]);
  useEffect(() => {
    const update = () => setNow(Date.now());
    const timer = window.setInterval(update, 30_000);
    window.addEventListener("focus", update);
    return () => { window.clearInterval(timer); window.removeEventListener("focus", update); };
  }, []);
  const next = useMemo(() => reconcileRecent(recent.current, sessions, now), [sessions, now]);
  // Publish only committed ranks, keeping renders and StrictMode retries pure.
  useEffect(() => { recent.current = next; }, [next]);
  return useMemo(() => sidebarGroups(sessions, order, next, now, projects), [sessions, order, next, now, projects]);
}
