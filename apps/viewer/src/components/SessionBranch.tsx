import { useEffect, useId } from "react";
import type { SessionActivityFilter, SessionChildrenState, SessionOrder, SessionSummary } from "../lib/types";
import { formatRelativeTime, providerLabel, sessionDisplayTitle, subagentDetail } from "../lib/state";
import { BranchIcon, ChevronIcon } from "./Icons";

interface SessionBranchProps {
  order: SessionOrder;
  session: SessionSummary;
  depth: number;
  activity_filter?: SessionActivityFilter;
  expanded_session_keys: ReadonlySet<string>;
  session_children: ReadonlyMap<string, SessionChildrenState>;
  selected_session_key: string | null;
  on_toggle: (session_key: string) => void;
  on_children_load: (parent_session_key: string) => void;
  on_children_release?: (parent_session_key: string) => void;
  on_session_select: (session_key: string) => void;
  on_question_session_select?: (session_key: string) => void;
  on_children_retry: (parent_session_key: string) => void;
  on_children_load_more: (parent_session_key: string) => void;
}

function subagentCountLabel(count: number): string {
  return `${count} subagent${count === 1 ? "" : "s"}`;
}

export function SessionBranch({
  order,
  session,
  depth,
  activity_filter = "all",
  expanded_session_keys,
  session_children,
  selected_session_key,
  on_toggle,
  on_children_load,
  on_children_release,
  on_session_select,
  on_question_session_select,
  on_children_retry,
  on_children_load_more,
}: SessionBranchProps) {
  const metadataId = useId();
  const all = activity_filter === "all";
  const hasChildren = all && session.child_count > 0;
  const isSubagent = depth > 0 || session.is_subagent;
  const isExpanded = expanded_session_keys.has(session.session_key);
  const childrenState = session_children.get(session.session_key);
  const children = childrenState?.sessions ?? [];
  const relationship = isSubagent ? subagentDetail(session) : null;
  const title = sessionDisplayTitle(session);
  const hasTimestamp = session.updated_at_ms !== null || !!session.timestamp;
  const timeId = `${metadataId}-time`;
  const preview = session.latest_assistant_preview?.split(/\r?\n/).find((line) => line.trim())?.replace(/\s+/g, " ").trim();
  const project = session.project?.trim() || session.cwd?.split(/[\\/]/).filter(Boolean).slice(-1)[0] || "Unassigned project";
  const sessionDescription = isSubagent ? `subagent ${title}` : title;
  const requiredCount = session.question_attention?.required_count ?? 0;
  const availableCount = session.question_attention?.available_count ?? 0;
  const isRunning = (all || activity_filter === "running") && requiredCount === 0
    && (session.is_running === true || (all && session.has_running_descendant === true));
  const unreadCount = session.unread_final_count ?? Number(session.has_unread);
  const hasUnread = (activity_filter === "unread" || (all && !isRunning)) && unreadCount > 0;
  const unreadLabel = `${unreadCount} unread final ${unreadCount === 1 ? "reply" : "replies"}`;
  const runningLabel = session.is_running ? "Running" : "Subagent running";
  const questionCount = all ? requiredCount || availableCount : activity_filter === "needs_input" ? requiredCount : 0;
  const questionLabel = requiredCount > 0 ? "Input required" : "Question available";

  const missingChildren = childrenState === undefined;
  useEffect(() => {
    if (hasChildren && isExpanded) on_children_load(session.session_key);
  }, [hasChildren, isExpanded, missingChildren, on_children_load, session.session_key]);
  useEffect(() => {
    if (hasChildren && isExpanded) return () => on_children_release?.(session.session_key);
  }, [hasChildren, isExpanded, on_children_release, session.session_key]);

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
          aria-describedby={hasTimestamp ? `${metadataId} ${timeId}` : metadataId}
          aria-label={`${sessionDescription}, ${providerLabel(session.provider)} session ${session.session_id}${questionCount ? `, ${questionLabel.toLowerCase()}` : ""}${isRunning ? `, ${runningLabel.toLowerCase()}` : hasUnread ? `, ${unreadLabel}` : ""}`}
          className="session-row"
          data-selected={session.session_key === selected_session_key}
          data-subagent={isSubagent}
          data-unread={hasUnread || undefined}
          onClick={() => questionCount && on_question_session_select ? on_question_session_select(session.session_key) : on_session_select(session.session_key)}
          title={`${title}\n${providerLabel(session.provider)} · ${session.project || session.cwd || "Unassigned project"}\n${session.session_id}`}
          type="button"
        >
          <span className="session-row__body">
            <span className="session-row__headline">
              {isSubagent ? <BranchIcon className="session-row__branch-icon" /> : null}
              <span className="session-row__title">{title}</span>
              {hasTimestamp && <span className="session-row__time" id={timeId}>{formatRelativeTime(session.timestamp, session.updated_at_ms)}</span>}
            </span>
            {order === "time" && preview ? <span className="session-row__preview" title={preview}>{preview}</span> : null}
            <span className="session-row__meta" id={metadataId}>
              <span className="session-row__provider" data-provider={session.provider}>
                {providerLabel(session.provider)}
              </span>
              {order === "time" && <><span aria-hidden="true">·</span><span className="session-row__project" title={session.cwd ?? project}>{project}</span></>}
              {!all && isSubagent && <span className="session-row__children" title={relationship ?? undefined}>Subagent</span>}
              {hasChildren && <span className="session-row__children" title={subagentCountLabel(session.child_count)}>
                <BranchIcon />{subagentCountLabel(session.child_count)}
              </span>}
              {relationship && <span className="session-row__relationship sr-only">{relationship}</span>}
            </span>
            {(questionCount > 0 || isRunning || hasUnread) && <span className="session-row__signals">
              {questionCount > 0 && <span className="session-row__question" data-blocking={requiredCount > 0}
                role="img" aria-label={`${questionLabel}, ${questionCount} unanswered ${questionCount === 1 ? "question" : "questions"}`}>
                {questionLabel}{questionCount > 1 && ` (${questionCount})`}
              </span>}
              {isRunning ? <span aria-label={runningLabel} className="session-row__activity" role="img">
                <span aria-hidden="true" className="session-row__running inline-spinner" />{runningLabel}
              </span> : hasUnread ? <span aria-label={unreadLabel} className="session-row__activity" role="img">
                <span aria-hidden="true" className="session-row__unread-dot" />{unreadCount === 1 ? "New reply" : `${unreadCount} new replies`}
              </span> : null}
            </span>}
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
              order={order}
              depth={depth + 1}
              expanded_session_keys={expanded_session_keys}
              key={child.session_key}
              on_children_load={on_children_load}
              on_children_release={on_children_release}
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
