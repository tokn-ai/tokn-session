import { useEffect, useRef } from "react";
import { SESSION_ACTIVITY_LABELS, sessionActivity } from "../lib/sessionActivity";
import { formatRelativeTime, providerLabel, sessionDisplayTitle } from "../lib/state";
import { PROVIDERS, type SessionActivityFilter, type SessionSummary } from "../lib/types";
import { BranchIcon, PanelIcon, SearchIcon, ServerIcon } from "./Icons";
import { LoadingRows } from "./StateView";
import "./SessionInbox.css";

interface SessionInboxProps {
  machine_label: string;
  sessions: SessionSummary[];
  activity: SessionActivityFilter;
  search: string;
  provider_count: number;
  pending: boolean;
  loading: boolean;
  loading_more: boolean;
  has_more: boolean;
  error: string | null;
  source_warning: string | null;
  on_filter: (filter: SessionActivityFilter) => void;
  on_open: (key: string, question: boolean) => void;
  on_find: () => void;
  on_create?: () => void;
  on_load_more: () => void;
  on_retry: () => void;
}

const FILTERS: SessionActivityFilter[] = ["all", "needs_input", "running", "unread"];
const EMPTY: Record<SessionActivityFilter, string> = {
  all: "No sessions yet",
  needs_input: "No blocking questions reported",
  running: "No running sessions reported",
  unread: "No unread replies reported",
};

function InboxRow({ session, filter, on_open }: Pick<SessionInboxProps, "on_open"> & { session: SessionSummary; filter: SessionActivityFilter }) {
  const activity = sessionActivity(session, filter);
  const title = sessionDisplayTitle(session);
  const preview = session.latest_assistant_preview?.trim() || session.preview?.trim();
  const project = session.project || session.cwd?.split(/[\\/]/).filter(Boolean).slice(-1)[0];
  return <li>
    <button className="inbox-row" type="button" onClick={() => on_open(session.session_key, activity.question)}>
      <span className="inbox-row__body">
        <span className="inbox-row__context">
          <span className="provider-badge" data-provider={session.provider}>{providerLabel(session.provider)}</span>
          {project && <span className="inbox-row__project" title={session.cwd ?? project}>{project}</span>}
          {session.is_subagent && <span className="inbox-row__branch"><BranchIcon />Subagent</span>}
        </span>
        <strong className="inbox-row__title">{title}</strong>
        {preview && preview !== title && <span className="inbox-row__preview">{preview}</span>}
      </span>
      <span className="inbox-row__aside">
        <span className="inbox-row__time">{formatRelativeTime(session.timestamp, session.updated_at_ms)}</span>
        {activity.label && <span className="activity-label" data-tone={activity.tone}>{activity.label}</span>}
      </span>
    </button>
  </li>;
}

export function SessionInbox({ machine_label, sessions, activity, search, provider_count, pending, loading,
  loading_more, has_more, error, source_warning, on_filter, on_open, on_find, on_create, on_load_more, on_retry }: SessionInboxProps) {
  const list = useRef<HTMLDivElement>(null);
  useEffect(() => { if (list.current) list.current.scrollTop = 0; }, [activity, search, provider_count]);
  const filtered = !!search.trim() || provider_count < PROVIDERS.length;
  const waiting = loading || pending;
  return <main className="conversation session-inbox" aria-label="Session workspace">
    <header className="inbox-bar" data-tauri-drag-region>
      <button type="button" aria-label="Open sessions" className="mobile-sidebar-button icon-button" onClick={on_find}><PanelIcon /></button>
      <span className="inbox-bar__machine"><ServerIcon />{machine_label}</span>
      <button type="button" className="inbox-find" onClick={on_find}><SearchIcon />Find session</button>
    </header>
    <div className="inbox-scroll" ref={list}>
      <div className="inbox-content">
        <div className="inbox-heading">
          <h1 tabIndex={-1}>Session inbox</h1>
          {on_create && <button className="inbox-create" type="button" onClick={on_create}><span aria-hidden="true">+</span>Start session</button>}
        </div>
        <div className="inbox-filters" aria-label="Session activity" role="group">
          {FILTERS.map((filter) => <button key={filter} type="button" aria-pressed={activity === filter}
            onClick={() => on_filter(filter)}>{SESSION_ACTIVITY_LABELS[filter]}</button>)}
        </div>
        {filtered && <button type="button" className="inbox-scope" onClick={on_find}><SearchIcon />
          {search.trim() && <span>Matching “{search.trim()}”</span>}
          {provider_count < PROVIDERS.length && <span>{provider_count} of {PROVIDERS.length} providers</span>}
        </button>}
        {error && <p className="inbox-alert" role="alert">{error} <button type="button" className="text-button" onClick={on_retry}>Retry sessions</button></p>}
        {source_warning && <p className="inbox-alert" role="status">{source_warning}</p>}
        {sessions.length > 0 ? <ul className="inbox-list" aria-label={SESSION_ACTIVITY_LABELS[activity]} aria-busy={loading}>
          {sessions.map((session) => <InboxRow key={session.session_key} session={session} filter={activity} on_open={on_open} />)}
        </ul> : waiting && provider_count > 0 ? <LoadingRows count={4} /> : !error && <div className="inbox-empty">
          <h2>{!provider_count ? "Select a provider" : filtered ? "No matching sessions" : EMPTY[activity]}</h2>
          {filtered ? <button type="button" className="text-button" onClick={on_find}>Edit search and providers</button>
            : activity !== "all" ? <button type="button" className="text-button" onClick={() => on_filter("all")}>View all sessions</button>
            : <p>Saved agent conversations on this machine appear here.</p>}
        </div>}
        {has_more && <button className="page-button inbox-more" type="button" disabled={loading || loading_more}
          onClick={on_load_more}>{loading_more ? "Loading sessions…" : "Show more sessions"}</button>}
      </div>
    </div>
  </main>;
}
