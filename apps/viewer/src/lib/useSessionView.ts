import { createUuid } from "./id";
import { useEffect, useMemo, useRef } from "react";
import { updateSessionView } from "./tauri";
import { captureTransport } from "./transport";
import type { SessionChildrenState, SessionSummary, SessionViewRequest } from "./types";

const HEARTBEAT_MS = 30_000;
const CANDIDATE_LIMIT = 128;

type ViewSelection = Pick<SessionViewRequest, "session_key" | "candidate_session_keys">;

export function sessionViewCandidates(
  selected: SessionSummary | null,
  sessions: SessionSummary[],
  children: ReadonlyMap<string, SessionChildrenState>,
): string[] {
  const result = new Set<string>();
  const visited = new Set<string>();
  function visit(session: SessionSummary) {
    if (result.size >= CANDIDATE_LIMIT || visited.has(session.session_key)) return;
    visited.add(session.session_key);
    // Full paths distinguish projects that share a directory basename.
    const sameProject = !selected || (selected.cwd
      ? session.cwd === selected.cwd : session.project === selected.project);
    if (sameProject) result.add(session.session_key);
    for (const child of children.get(session.session_key)?.sessions ?? []) visit(child);
  }
  for (const session of sessions) visit(session);
  return [...result];
}

/** One viewer owns a renewable lease, independent of message-read acknowledgements. */
export function useSessionView(
  session_key: string | null,
  selected: SessionSummary | null,
  sessions: SessionSummary[],
  children: ReadonlyMap<string, SessionChildrenState>,
  visible = true,
) {
  const candidates = useMemo(() => sessionViewCandidates(selected, sessions, children), [selected, sessions, children]);
  // Metadata-only sidebar refreshes do not change the lease's candidate set.
  const selectionKey = JSON.stringify([session_key, candidates]);
  const latest = useRef<ViewSelection>({ session_key: null, candidate_session_keys: [] });
  latest.current = { session_key, candidate_session_keys: candidates };
  const report = useRef<(() => void) | null>(null);

  useEffect(() => {
    if (!visible) return;
    // New id for each effect lifetime also makes StrictMode cleanup independent.
    const view_id = createUuid();
    const transport = captureTransport();
    let revision = 0;
    let disposed = false;
    let inFlight = false;
    let queued = false;
    const publish = () => {
      if (disposed) return;
      if (inFlight) { queued = true; return; }
      inFlight = true;
      const request: SessionViewRequest = { view_id, revision: ++revision, ...latest.current };
      void updateSessionView(request, transport.invoke).catch(() => {
        // The next heartbeat retries; timeline loading reports actionable errors.
      }).finally(() => {
        inFlight = false;
        if (queued) { queued = false; publish(); }
      });
    };
    const release = () => {
      if (disposed) return;
      disposed = true;
      const request: SessionViewRequest = {
        view_id, revision: ++revision, session_key: null, candidate_session_keys: [],
      };
      // Higher revisions ensure a delayed update cannot resurrect this lease.
      void transport.release("update_session_view", { request }).catch(() => {});
    };
    report.current = publish;
    const stopListening = transport.on_close(release);
    const heartbeat = window.setInterval(publish, HEARTBEAT_MS);
    return () => {
      report.current = null;
      window.clearInterval(heartbeat);
      stopListening();
      release();
    };
  }, [visible]);

  useEffect(() => { report.current?.(); }, [selectionKey, visible]);
}
