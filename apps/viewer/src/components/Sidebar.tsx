import { useCallback, useEffect, useId, useMemo, useState, type KeyboardEvent } from "react";
import {
  PROVIDERS,
  type RegisteredProject,
  type SessionActivityFilter,
  type SessionChildrenState,
  type SessionOrder,
  type SessionSummary,
  type SourceError,
  type ViewerProvider,
} from "../lib/types";
import { knownSessionAncestors, providerLabel } from "../lib/state";
import { SESSION_ACTIVITY_LABELS } from "../lib/sessionActivity";
import { ActivityIcon, CloseIcon, FilterIcon, PanelIcon, SearchIcon, WarningIcon } from "./Icons";
import { useSidebarGroups } from "../lib/useSidebarGroups";
import { LoadingRows } from "./StateView";
import { SessionDraftRows } from "./SessionDraftRows";
import type { SessionDraft } from "../lib/useSessionDrafts";
import { sidebarDraftEntries, type SidebarDraftEntry } from "../lib/sidebarDrafts";
import { SessionBranch } from "./SessionBranch";
import { SessionSidebarGroup } from "./SessionSidebarGroup";
import "./Sidebar.css";

interface SidebarProps {
  on_workspace_open?: () => void;
  workspace_selected?: boolean;
  activity_filter?: SessionActivityFilter;
  on_clear_activity_filter?: () => void;
  drafts?: readonly SessionDraft[];
  selected_draft_id?: string | null;
  on_draft_select?: (draft_id: string) => void;
  registered_projects?: RegisteredProject[];
  workspace_available?: boolean;
  workspace_notice?: string | null;
  on_workspace_retry?: () => void;
  on_add_project?: () => void;
  on_new_session?: (group_key?: string) => void;
  order?: SessionOrder;
  on_order_change?: (order: SessionOrder) => void;
  on_close?: () => void;
  on_collapse?: () => void;
  sessions: SessionSummary[];
  session_children: ReadonlyMap<string, SessionChildrenState>;
  selected_session_key: string | null;
  enabled_providers: ReadonlySet<ViewerProvider>;
  search: string;
  is_loading: boolean;
  error: string | null;
  source_errors: SourceError[];
  pending_providers: ViewerProvider[];
  has_more: boolean;
  is_loading_more: boolean;
  on_search_change: (value: string) => void;
  on_provider_toggle: (provider: ViewerProvider) => void;
  on_session_select: (session_key: string) => void;
  on_question_session_select?: (session_key: string) => void;
  on_children_load: (parent_session_key: string) => void;
  on_children_release?: (parent_session_key: string) => void;
  on_children_retry: (parent_session_key: string) => void;
  on_children_load_more: (parent_session_key: string) => void;
  on_retry: () => void;
  on_load_more: () => void;
}

const NO_PROJECTS: RegisteredProject[] = [];

function pendingProviderLabel(providers: ViewerProvider[]): string {
  const labels = providers.map((provider) => providerLabel(provider));
  if (labels.length < 3) {
    return labels.join(" and ");
  }
  return `${labels.slice(0, -1).join(", ")}, and ${labels[labels.length - 1]!}`;
}

// Arrow keys move focus through rendered roots, children and drafts. Activation
// remains a separate native button action, so browsing never opens a session.
function navigateSessionRows(event: KeyboardEvent<HTMLElement>) {
  if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey || event.nativeEvent.isComposing) return;
  if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
  const target = event.target;
  if (!(target instanceof HTMLElement)) return;
  const fromSearch = target.matches('input[type="search"]');
  if (!fromSearch && !target.matches("button.session-row")) return;
  const rows = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>("button.session-row"));
  if (!rows.length || (fromSearch && event.key === "ArrowUp")) return;
  const index = rows.indexOf(target as HTMLButtonElement);
  const next = event.key === "ArrowDown" ? rows[Math.min(index + 1, rows.length - 1)]
    : rows[index - 1] ?? event.currentTarget.querySelector<HTMLInputElement>('input[type="search"]');
  event.preventDefault();
  next?.focus();
}

export function Sidebar({
  on_workspace_open,
  workspace_selected = false,
  activity_filter = "all",
  on_clear_activity_filter,
  drafts = [],
  selected_draft_id = null,
  on_draft_select,
  registered_projects = NO_PROJECTS,
  workspace_available = false,
  workspace_notice,
  on_workspace_retry,
  on_add_project,
  on_new_session,
  order = "time",
  on_order_change,
  on_close,
  on_collapse,
  sessions,
  session_children,
  selected_session_key,
  enabled_providers,
  search,
  is_loading,
  error,
  source_errors,
  pending_providers,
  has_more,
  is_loading_more,
  on_search_change,
  on_provider_toggle,
  on_session_select,
  on_question_session_select,
  on_children_load,
  on_children_release,
  on_children_retry,
  on_children_load_more,
  on_retry,
  on_load_more,
}: SidebarProps) {
  const [filtersOpen, setFiltersOpen] = useState(false);
  const filtersId = useId();
  const searchHelpId = useId();
  const filtered = activity_filter !== "all";
  const draft_entries = useMemo(() => filtered ? [] : sidebarDraftEntries(drafts, registered_projects, selected_draft_id, search),
    [drafts, registered_projects, selected_draft_id, search, filtered]);
  const draft_projects = useMemo(() => new Set(draft_entries.flatMap(({ project }) => project ? [project.project_id] : [])), [draft_entries]);
  const visible_projects = useMemo(() => registered_projects.filter((project) => !search.trim()
    || draft_projects.has(project.project_id)
    || `${project.name} ${project.folder_path}`.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase())), [registered_projects, search, draft_projects]);
  const groups = useSidebarGroups(sessions, order, filtered ? NO_PROJECTS : visible_projects);
  const project_drafts = useMemo(() => {
    const grouped = new Map<string, SidebarDraftEntry[]>();
    if (order === "project") for (const entry of draft_entries) {
      if (!entry.project) continue;
      const entries = grouped.get(entry.project.group_key) ?? [];
      entries.push(entry);
      grouped.set(entry.project.group_key, entries);
    }
    return grouped;
  }, [draft_entries, order]);
  const ungrouped_drafts = order === "project" ? draft_entries.filter(({ project }) => !project) : draft_entries;
  const noProviders = enabled_providers.size === 0;
  const isIndexingCatalog = pending_providers.length > 0;
  const indexingLabel = pendingProviderLabel(pending_providers);
  const [expandedSessionKeys, setExpandedSessionKeys] = useState<Set<string>>(() => new Set());

  const ancestors = useMemo(() => filtered ? [] : knownSessionAncestors(sessions, session_children, selected_session_key),
    [sessions, session_children, selected_session_key, filtered]);
  const selectedRoot = ancestors[0] ?? selected_session_key;

  useEffect(() => {
    if (ancestors.length === 0) {
      return;
    }
    setExpandedSessionKeys((current) => {
      const next = new Set(current);
      for (const ancestor of ancestors) {
        next.add(ancestor);
      }
      return next.size === current.size ? current : next;
    });
  }, [ancestors]);

  const toggleSessionBranch = useCallback((session_key: string) => {
    setExpandedSessionKeys((current) => {
      const next = new Set(current);
      if (next.has(session_key)) {
        next.delete(session_key);
      } else {
        next.add(session_key);
      }
      return next;
    });
  }, []);

  return (
    <aside aria-label="Sessions" className="sidebar" onKeyDown={navigateSessionRows}>
      <header className="sidebar__header" data-tauri-drag-region>
        <div className="brand-mark" aria-hidden="true">
          <span />
          <span />
          <span />
        </div>
        <h1>tokn</h1>
        {on_collapse ? <button aria-label="Collapse sessions" className="icon-button sidebar-collapse" onClick={on_collapse} type="button"><PanelIcon /></button> : null}
        <button aria-label="Close sessions" className="icon-button sidebar-close" onClick={on_close} type="button">
          <CloseIcon />
        </button>
      </header>

      <div className="sidebar__controls">
        {on_workspace_open && <button type="button" className="workspace-nav"
          aria-current={workspace_selected ? "page" : undefined} onClick={on_workspace_open}>
          <ActivityIcon />Workspace
        </button>}
        {(on_new_session || on_add_project) && <div className="sidebar__creation">
          {on_new_session && <button className="sidebar__new" type="button"
            disabled={!workspace_available} onClick={() => on_new_session()}>
            <span aria-hidden="true">+</span>New session
          </button>}
          {on_add_project && <button className="sidebar__add-project" type="button"
            aria-label="Add project" title="Add project" disabled={!workspace_available} onClick={on_add_project}>
            <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
              <path d="M3 8V6a2 2 0 0 1 2-2h4l2 3h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8Z" /><path d="M12 10v7m-3.5-3.5h7" />
            </svg>
          </button>}
        </div>}
        {workspace_notice && <p className="workspace-sidebar-notice" role="status">{workspace_notice}
          {on_workspace_retry && <button className="text-button" type="button" onClick={on_workspace_retry}>Retry projects</button>}
        </p>}
        <label className="search-field">
          <span className="sr-only">Search sessions</span>
          <SearchIcon />
          <input
            aria-describedby={searchHelpId}
            aria-keyshortcuts="Control+k Meta+k"
            onChange={(event) => on_search_change(event.currentTarget.value)}
            placeholder="Search sessions"
            spellCheck={false}
            type="search"
            value={search}
          />
          {is_loading && sessions.length > 0 ? <span className="inline-spinner" /> : !search &&
            <kbd aria-hidden="true" className="sidebar__shortcut">{/Mac|iPad|iPhone/.test(navigator.platform) ? "⌘ K" : "Ctrl K"}</kbd>}
        </label>
        <span id={searchHelpId} className="sr-only">Press the down arrow to browse sessions, then Enter to open.</span>
        <div className="sidebar__tools">
          <div aria-label="Group sessions" className="sidebar-order" role="group">
            <button aria-pressed={order === "time"} onClick={() => on_order_change?.("time")} type="button">Time</button>
            <button aria-pressed={order === "project"} onClick={() => on_order_change?.("project")} type="button">Projects</button>
          </div>
          <button type="button" className="sidebar__filter-trigger" aria-controls={filtersOpen ? filtersId : undefined}
            aria-label={`Providers, ${enabled_providers.size} of ${PROVIDERS.length} selected`}
            aria-expanded={filtersOpen} data-filtered={enabled_providers.size !== PROVIDERS.length}
            onClick={() => setFiltersOpen(!filtersOpen)}>
            <FilterIcon />Providers{enabled_providers.size !== PROVIDERS.length && <span>{enabled_providers.size}</span>}
          </button>
        </div>
        {filtered && <button type="button" className="sidebar__activity-filter"
          aria-label={`Clear activity filter: ${SESSION_ACTIVITY_LABELS[activity_filter]}`} onClick={on_clear_activity_filter}>
          {SESSION_ACTIVITY_LABELS[activity_filter]}<CloseIcon />
        </button>}
        {filtersOpen && <div aria-label="Filter by provider" className="provider-filters" id={filtersId} role="group">
          {PROVIDERS.map((provider) => (
            <button aria-pressed={enabled_providers.has(provider)} className="provider-filter" data-provider={provider}
              key={provider} onClick={() => on_provider_toggle(provider)} type="button">
              <span className="provider-dot" />{providerLabel(provider)}
            </button>
          ))}
          {enabled_providers.size !== PROVIDERS.length && <button className="text-button" type="button" onClick={() => {
            for (const provider of PROVIDERS) if (!enabled_providers.has(provider)) on_provider_toggle(provider);
          }}>Select all</button>}
        </div>}
      </div>

      {source_errors.length > 0 ? (
        <details className="source-warning">
          <summary>
            <WarningIcon />
            <span>
              {source_errors.length === 1
                ? `${providerLabel(source_errors[0]!.provider)} could not be read.`
                : `${source_errors.length} providers could not be read.`}
            </span>
          </summary>
          <div className="source-warning__detail">
            <ul>
              {source_errors.map((sourceError) => (
                <li key={sourceError.provider}>
                  <strong>{providerLabel(sourceError.provider)}</strong>
                  <span>{sourceError.message}</span>
                </li>
              ))}
            </ul>
            <button className="text-button" onClick={on_retry} type="button">
              Retry providers
            </button>
          </div>
        </details>
      ) : null}

      <div className="sidebar__list">
        {on_draft_select && ungrouped_drafts.length > 0 && <SessionSidebarGroup label="Draft sessions"
          title={order === "project" ? "Choose a project" : "Drafts"} count={ungrouped_drafts.length}
          search={search} selected_key={ungrouped_drafts.some(({ draft }) => draft.draft_id === selected_draft_id) ? selected_draft_id : null}>
          <SessionDraftRows entries={ungrouped_drafts} selected_draft_id={selected_draft_id} on_select={on_draft_select} />
        </SessionSidebarGroup>}
        {is_loading && sessions.length === 0 ? <LoadingRows count={6} /> : null}

        {!is_loading && error && sessions.length === 0 ? (
          <div className="sidebar-state" role="alert">
            <strong>Sessions unavailable</strong>
            <span>{error}</span>
            <button className="text-button" onClick={on_retry} type="button">
              Try again
            </button>
          </div>
        ) : null}

        {!is_loading && error && sessions.length > 0 ? (
          <div className="sidebar-refresh-error" role="alert">
            <strong>Couldn’t refresh the session list</strong>
            <span>{error}</span>
            <button className="text-button" onClick={on_retry} type="button">
              Try again
            </button>
          </div>
        ) : null}

        {!is_loading && !error && sessions.length === 0 && groups.length === 0 && draft_entries.length === 0 ? (
          <div className="sidebar-state">
            <strong>
              {noProviders
                ? "No providers selected"
                : isIndexingCatalog
                  ? "Finding sessions"
                  : "No sessions found"}
            </strong>
            <span>
              {noProviders
                ? "Enable a provider to browse its sessions."
                : isIndexingCatalog
                  ? `Indexing ${indexingLabel}. Known sessions will appear here shortly.`
                  : filtered
                    ? `No sessions match “${SESSION_ACTIVITY_LABELS[activity_filter]}” with your current filters.`
                  : search.trim()
                    ? "Try a different search."
                    : "Known sessions will appear here."}
            </span>
          </div>
        ) : null}

        {!is_loading && !error && sessions.length > 0 && isIndexingCatalog ? (
          <div className="sidebar-indexing" role="status">
            <span className="inline-spinner" />
            <span>{`Indexing ${indexingLabel}…`}</span>
          </div>
        ) : null}

        {groups.map((group) => {
          const entries = project_drafts.get(group.key) ?? [];
          const selectedKey = group.sessions.some((session) => session.session_key === selectedRoot)
            ? selected_session_key : entries.find(({ draft }) => draft.draft_id === selected_draft_id)?.draft.draft_id ?? null;
          return (
            <SessionSidebarGroup key={`${order}:${group.key}`} title={group.project} count={group.sessions.length + entries.length}
              search={search} selected_key={selectedKey}
              action={group.registered_projects && on_new_session && <button type="button" className="session-group__new"
                aria-label={`New session in ${group.project}`} disabled={!workspace_available || !group.registered_projects.some((project) => project.available)}
                title={group.registered_projects.some((project) => project.available) ? `New session in ${group.project}` : group.registered_projects.map((project) => project.unavailable_reason ?? "This folder is unavailable.").join("\n")}
                onClick={() => on_new_session(group.key)}>+</button>}>
              {on_draft_select ? <SessionDraftRows entries={entries} selected_draft_id={selected_draft_id}
                show_project={false} on_select={on_draft_select} /> : null}
              {group.sessions.length === 0 && entries.length === 0 ? <p className="workspace-empty-project">No sessions yet</p> : null}
              {group.sessions.map((session) => (
                <SessionBranch
                  order={order}
                  activity_filter={activity_filter}
                  depth={0}
                  expanded_session_keys={expandedSessionKeys}
                  key={session.session_key}
                  on_children_load={on_children_load}
                  on_children_release={on_children_release}
                  on_children_load_more={on_children_load_more}
                  on_children_retry={on_children_retry}
                  on_session_select={on_session_select}
                  on_question_session_select={on_question_session_select}
                  on_toggle={toggleSessionBranch}
                  selected_session_key={selected_session_key}
                  session={session}
                  session_children={session_children}
                />
              ))}
            </SessionSidebarGroup>
          );
        })}

        {has_more ? (
          <button
            className="load-more-button"
            disabled={is_loading || is_loading_more}
            onClick={on_load_more}
            type="button"
          >
            {is_loading_more ? "Loading…" : is_loading ? "Updating sessions…" : "Load more sessions"}
          </button>
        ) : null}
      </div>
    </aside>
  );
}
