import { useCallback, useEffect, useId, useState } from "react";
import type {
  SessionChildrenState,
  SourceError,
  SessionSummary,
  ViewerProvider,
  SessionOrder,
} from "../lib/types";
import {
  formatRelativeTime,
  knownSessionAncestors,
  providerLabel,
  sessionDisplayTitle,
  subagentDetail,
} from "../lib/state";
import { BranchIcon, ChevronIcon, CloseIcon, PanelIcon, SearchIcon, WarningIcon } from "./Icons";
import { useSidebarGroups } from "../lib/useSidebarGroups";
import { LoadingRows } from "./StateView";

interface SidebarProps {
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
  on_children_retry: (parent_session_key: string) => void;
  on_children_load_more: (parent_session_key: string) => void;
  on_retry: () => void;
  on_load_more: () => void;
}

interface SessionBranchProps {
  show_project?: boolean;
  session: SessionSummary;
  depth: number;
  expanded_session_keys: ReadonlySet<string>;
  session_children: ReadonlyMap<string, SessionChildrenState>;
  selected_session_key: string | null;
  on_toggle: (session_key: string) => void;
  on_children_load: (parent_session_key: string) => void;
  on_session_select: (session_key: string) => void;
  on_question_session_select?: (session_key: string) => void;
  on_children_retry: (parent_session_key: string) => void;
  on_children_load_more: (parent_session_key: string) => void;
}

const PROVIDER_FILTERS: ViewerProvider[] = ["codex", "pi", "opencode", "zcode", "workbuddy", "dsh"];

function subagentCountLabel(count: number): string {
  return `${count} subagent${count === 1 ? "" : "s"}`;
}

function pendingProviderLabel(providers: ViewerProvider[]): string {
  const labels = providers.map((provider) => providerLabel(provider));
  if (labels.length < 3) {
    return labels.join(" and ");
  }
  return `${labels.slice(0, -1).join(", ")}, and ${labels[labels.length - 1]!}`;
}

function SessionBranch({
  show_project = false,
  session,
  depth,
  expanded_session_keys,
  session_children,
  selected_session_key,
  on_toggle,
  on_children_load,
  on_session_select,
  on_question_session_select,
  on_children_retry,
  on_children_load_more,
}: SessionBranchProps) {
  const metadataId = useId();
  const hasChildren = session.child_count > 0;
  const isExpanded = expanded_session_keys.has(session.session_key);
  const childrenState = session_children.get(session.session_key);
  const children = childrenState?.sessions ?? [];
  const relationship = depth > 0 ? subagentDetail(session) : null;
  const title = sessionDisplayTitle(session);
  const preview = session.preview?.replace(/\s+/g, " ").trim();
  const sessionDescription = depth > 0 ? `subagent ${title}` : title;
  const requiredCount = session.question_attention?.required_count ?? 0;
  const availableCount = session.question_attention?.available_count ?? 0;
  const isRunning = requiredCount === 0 && (session.is_running === true || session.has_running_descendant === true);
  const unreadCount = session.unread_final_count ?? Number(session.has_unread);
  const hasUnread = !isRunning && unreadCount > 0;
  const unreadLabel = `${unreadCount} unread final ${unreadCount === 1 ? "reply" : "replies"}`;
  const runningLabel = session.is_running ? "Running" : "Subagent running";
  const questionCount = requiredCount + availableCount;
  const questionLabel = requiredCount > 0 ? "Input required" : "Question available";

  useEffect(() => {
    if (hasChildren && isExpanded && !childrenState) {
      on_children_load(session.session_key);
    }
  }, [childrenState, hasChildren, isExpanded, on_children_load, session.session_key]);

  return (
    <div className="session-tree__branch" data-depth={depth}>
      <div className="session-tree__row">
        {hasChildren ? (
          <button
            aria-expanded={isExpanded}
            aria-label={`${isExpanded ? "Hide" : "Show"} ${subagentCountLabel(session.child_count)} for ${title}`}
            className="session-tree__toggle"
            onClick={() => on_toggle(session.session_key)}
            type="button"
          >
            <ChevronIcon className={isExpanded ? "is-expanded" : undefined} />
          </button>
        ) : (
          <span aria-hidden="true" className="session-tree__toggle-spacer" />
        )}
        <button
          aria-current={session.session_key === selected_session_key ? "page" : undefined}
          aria-describedby={metadataId}
          aria-label={`${sessionDescription}, ${providerLabel(session.provider)} session ${session.session_id}${questionCount ? `, ${questionLabel.toLowerCase()}` : ""}${isRunning ? `, ${runningLabel.toLowerCase()}` : hasUnread ? `, ${unreadLabel}` : ""}`}
          className="session-row"
          data-selected={session.session_key === selected_session_key}
          data-subagent={depth > 0}
          data-unread={hasUnread || undefined}
          onClick={() => questionCount && on_question_session_select ? on_question_session_select(session.session_key) : on_session_select(session.session_key)}
          title={`${title}\n${providerLabel(session.provider)} · ${session.project || session.cwd || "Unassigned project"}\n${session.session_id}`}
          type="button"
        >
          <span className="session-row__body">
            <span className="session-row__headline">
              {depth > 0 ? <BranchIcon className="session-row__branch-icon" /> : null}
              <span className="session-row__title">{title}</span>
              <span className="session-row__time" aria-hidden="true">{formatRelativeTime(session.timestamp, session.updated_at_ms)}</span>
              {questionCount > 0 ? <span className="session-row__question" data-blocking={requiredCount > 0} role="img" aria-label={`${questionLabel}, ${questionCount} unanswered ${questionCount === 1 ? "question" : "questions"}`} title={questionLabel}>?{questionCount > 1 ? questionCount : ""}</span> : null}
              {isRunning ? (
                <span aria-label={runningLabel} className="session-row__running inline-spinner" role="img" />
              ) : hasUnread ? (
                <span
                  aria-label={unreadLabel}
                  className={unreadCount > 1 ? "session-row__unread-count" : "session-row__unread-dot"}
                  data-unread-source="direct"
                  role="img"
                >{unreadCount > 1 ? unreadCount : null}</span>
              ) : null}
            </span>
            {preview && preview !== title ? <span className="session-row__preview sr-only">{preview}</span> : null}
            <span className="session-row__meta sr-only" id={metadataId}>
              <span className="session-row__provider" data-provider={session.provider}>{providerLabel(session.provider)}</span>
              {show_project && (session.project || session.cwd) ? <>
                <span aria-hidden="true">·</span>
                <span className="session-row__project" title={session.cwd ?? session.project ?? undefined}>{session.project || session.cwd}</span>
              </> : null}
              <span aria-hidden="true">·</span>
              <span>{formatRelativeTime(session.timestamp, session.updated_at_ms)}</span>
              {relationship ? (
                <>
                  <span aria-hidden="true">·</span>
                  <span className="session-row__relationship" title={relationship}>{relationship}</span>
                </>
              ) : null}
              {session.child_count > 0 ? (
                <>
                  <span aria-hidden="true">·</span>
                  <span>{subagentCountLabel(session.child_count)}</span>
                </>
              ) : null}
            </span>
          </span>
        </button>
      </div>

      {hasChildren && isExpanded ? (
        <div className="session-tree__children">
          {childrenState?.is_loading && children.length === 0 ? (
            <div className="session-tree__state">
              <span className="inline-spinner" />
              Loading subagents…
            </div>
          ) : null}

          {childrenState && !childrenState.is_loading && !childrenState.error && children.length === 0 ? (
            <div className="session-tree__state">No current subagents.</div>
          ) : null}

          {children.map((child) => (
            <SessionBranch
              depth={depth + 1}
              expanded_session_keys={expanded_session_keys}
              key={child.session_key}
              on_children_load={on_children_load}
              on_children_load_more={on_children_load_more}
              on_children_retry={on_children_retry}
              on_session_select={on_session_select}
              on_question_session_select={on_question_session_select}
              on_toggle={on_toggle}
              selected_session_key={selected_session_key}
              session={child}
              session_children={session_children}
            />
          ))}

          {childrenState?.error ? (
            <div className="session-tree__error" role="alert">
              <span>Subagents unavailable: {childrenState.error}</span>
              <button onClick={() => on_children_retry(session.session_key)} type="button">Retry</button>
            </div>
          ) : null}

          {childrenState?.next_cursor ? (
            <button
              className="session-tree__load-more"
              disabled={childrenState.is_loading || childrenState.is_loading_more}
              onClick={() => on_children_load_more(session.session_key)}
              type="button"
            >
              {childrenState.is_loading_more ? "Loading subagents…" : "Load more subagents"}
            </button>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

export function Sidebar({
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
  on_children_retry,
  on_children_load_more,
  on_retry,
  on_load_more,
}: SidebarProps) {
  const groups = useSidebarGroups(sessions, order);
  const noProviders = enabled_providers.size === 0;
  const isIndexingCatalog = pending_providers.length > 0;
  const indexingLabel = pendingProviderLabel(pending_providers);
  const [expandedSessionKeys, setExpandedSessionKeys] = useState<Set<string>>(() => new Set());

  useEffect(() => {
    const ancestors = knownSessionAncestors(sessions, session_children, selected_session_key);
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
  }, [selected_session_key, session_children, sessions]);

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
    <aside aria-label="Sessions" className="sidebar">
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
        <div className="sidebar__filter-heading">
          {search || enabled_providers.size !== PROVIDER_FILTERS.length ? (
            <button className="text-button" type="button" onClick={() => {
              on_search_change("");
              for (const provider of PROVIDER_FILTERS) {
                if (!enabled_providers.has(provider)) on_provider_toggle(provider);
              }
            }}>Reset filters</button>
          ) : null}
        </div>
        <label className="search-field">
          <span className="sr-only">Search sessions</span>
          <SearchIcon />
          <input
            onChange={(event) => on_search_change(event.currentTarget.value)}
            placeholder="Search sessions"
            spellCheck={false}
            type="search"
            value={search}
          />
          {is_loading && sessions.length > 0 ? <span className="inline-spinner" /> : null}
        </label>

        <details className="sidebar-providers">
          <summary>Providers <span>{enabled_providers.size === PROVIDER_FILTERS.length ? "All" : `${enabled_providers.size} selected`}</span><ChevronIcon /></summary>
          <div aria-label="Filter by provider" className="provider-filters">
            {PROVIDER_FILTERS.map((provider) => (
              <button
                aria-pressed={enabled_providers.has(provider)}
                className="provider-filter"
                data-provider={provider}
                key={provider}
                onClick={() => on_provider_toggle(provider)}
                type="button"
              >
                <span className="provider-dot" />
                {providerLabel(provider)}
              </button>
            ))}
          </div>
        </details>
        <div aria-label="Group sessions" className="sidebar-order" role="group">
          <button aria-pressed={order === "time"} onClick={() => on_order_change?.("time")} type="button">Time</button>
          <button aria-pressed={order === "project"} onClick={() => on_order_change?.("project")} type="button">Projects</button>
        </div>
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
            <strong>Session catalog could not refresh</strong>
            <span>{error}</span>
            <button className="text-button" onClick={on_retry} type="button">
              Try again
            </button>
          </div>
        ) : null}

        {!is_loading && !error && sessions.length === 0 ? (
          <div className="sidebar-state">
            <strong>
              {noProviders
                ? "No providers selected"
                : isIndexingCatalog
                  ? "Building session catalog"
                  : "No sessions found"}
            </strong>
            <span>
              {noProviders
                ? "Enable a provider to browse its sessions."
                : isIndexingCatalog
                  ? `Indexing ${indexingLabel}. Known sessions will appear here shortly.`
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

        {groups.map((group) => (
          <section className="session-group" key={group.key}>
            <h2 title={group.project}>
              {group.project}
              <span className="session-group__count" title={`${group.sessions.length} loaded sessions`}>
                {group.sessions.length}
              </span>
            </h2>
            <div className="session-group__items">
              {group.sessions.map((session) => (
                <SessionBranch
                  depth={0}
                  show_project={order === "time"}
                  expanded_session_keys={expandedSessionKeys}
                  key={session.session_key}
                  on_children_load={on_children_load}
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
            </div>
          </section>
        ))}

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
