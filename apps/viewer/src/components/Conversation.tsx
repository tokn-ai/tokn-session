import type { ExpandedActivityState } from "../lib/types";
import { useLayoutEffect, useState } from "react";
import { readingEventKey } from "../lib/readingPosition";
import { useTimelineScroll } from "../lib/useTimelineScroll";
import { isBookkeepingEvent } from "../lib/eventFilter";
import type {
  EventDetail,
  EventSummary,
  OutstandingQuestion,
  SessionHistoryStatus,
  SessionSummary,
  TrajectoryEventPageState,
} from "../lib/types";
import {
  eventButtonId,
  formatTimestamp,
  providerLabel,
  sessionDisplayTitle,
  shortSessionId,
  subagentDetail,
} from "../lib/state";
import { ChevronIcon, FilterIcon, InspectorIcon, PanelIcon, SendIcon } from "./Icons";
import { EventCard } from "./EventCard";
import { LoadingRows, StateView } from "./StateView";
import { SessionComposer } from "./SessionComposer";

interface ConversationProps {
  outstanding_questions?: OutstandingQuestion[];
  question_navigation?: { event_key: string; revision: number } | null;
  on_question_open?: (event_key: string) => void;
  pending_live_activity?: boolean;
  on_show_live_activity?: () => void;
  on_follow_change?: (following: boolean) => void;
  on_input_accepted?: (session_key: string) => void;
  session: SessionSummary | null;
  events: EventSummary[];
  selected_event_key: string | null;
  expanded_event_key: string | null;
  expanded_activity_keys?: Set<string>;
  expanded_activities?: Map<string, ExpandedActivityState>;
  expanded_detail: EventDetail | null;
  expanded_detail_error: string | null;
  expanded_detail_loading: boolean;
  initial_page_loaded: boolean;
  is_loading: boolean;
  is_loading_older: boolean;
  is_loading_newer: boolean;
  error: string | null;
  has_older: boolean;
  has_newer: boolean;
  total_events: number | null;
  history_status: SessionHistoryStatus | null;
  inspector_open: boolean;
  on_sidebar_open: () => void;
  on_inspector_toggle: () => void;
  on_event_select: (event_key: string) => void;
  on_event_toggle: (event_key: string) => void;
  trajectory_pages: ReadonlyMap<string, ReadonlyMap<string, TrajectoryEventPageState>>;
  trajectory_expanded_key: string | null;
  trajectory_expanded_event_key: string | null;
  trajectory_expanded_detail: EventDetail | null;
  trajectory_expanded_detail_error: string | null;
  trajectory_expanded_detail_loading: boolean;
  on_trajectory_load_older: (trajectory_key: string) => void;
  on_trajectory_load_newer: (trajectory_key: string) => void;
  on_trajectory_retry: (trajectory_key: string) => void;
  on_trajectory_event_toggle: (trajectory_key: string, event_key: string) => void;
  on_trajectory_retry_expanded_detail: (trajectory_key: string, event_key: string) => void;
  on_open_related_session: (source_session_key: string, target: SessionSummary) => void;
  on_load_older: () => void;
  on_load_newer: () => void;
  on_retry: () => void;
  on_retry_expanded_detail: () => void;
}

export function Conversation({
  outstanding_questions = [],
  question_navigation,
  on_question_open,
  pending_live_activity = false,
  on_show_live_activity,
  on_follow_change,
  on_input_accepted,
  session,
  events,
  selected_event_key,
  expanded_event_key,
  expanded_activity_keys,
  expanded_activities,
  expanded_detail,
  expanded_detail_error,
  expanded_detail_loading,
  initial_page_loaded,
  is_loading,
  is_loading_older,
  is_loading_newer,
  error,
  has_older,
  has_newer,
  total_events,
  history_status,
  inspector_open,
  on_sidebar_open,
  on_inspector_toggle,
  on_event_select,
  on_event_toggle,
  trajectory_pages,
  trajectory_expanded_key,
  trajectory_expanded_event_key,
  trajectory_expanded_detail,
  trajectory_expanded_detail_error,
  trajectory_expanded_detail_loading,
  on_trajectory_load_older,
  on_trajectory_load_newer,
  on_trajectory_retry,
  on_trajectory_event_toggle,
  on_trajectory_retry_expanded_detail,
  on_open_related_session,
  on_load_older,
  on_load_newer,
  on_retry,
  on_retry_expanded_detail,
}: ConversationProps) {
  const [hideLifecycle, setHideLifecycle] = useState(true);
  const visibleEvents = hideLifecycle ? events.filter((event) => !isBookkeepingEvent(event)) : events;
  const hiddenCount = events.length - visibleEvents.length;
  const scroll = useTimelineScroll({
    session_key: session?.session_key ?? null,
    initial_page_loaded,
    last_event: events.length ? readingEventKey(events[events.length - 1]) : undefined,
    on_follow_change,
  });

  useLayoutEffect(() => {
    if (!question_navigation) return;
    const button = document.getElementById(`${eventButtonId(question_navigation.event_key)}-label`)?.closest("button");
    if (!button || !scroll.timelineRef.current?.contains(button)) return;
    scroll.pause();
    button.scrollIntoView?.({ block: "start" });
    button.focus({ preventScroll: true });
    // Capture the chosen position so later detail/Markdown layout changes
    // preserve it instead of restoring the previously visible timeline row.
    scroll.pause();
  }, [question_navigation, session?.session_key]);

  function loadOlder() {
    scroll.pause();
    on_load_older();
  }

  const knownCount = total_events ?? session?.event_count ?? session?.message_count ?? null;
  const childDetail = session?.is_subagent ? subagentDetail(session) : null;
  const countLabel = total_events !== null || session?.event_count !== null
    ? knownCount !== null
      ? `${knownCount} events${has_older ? " loaded" : ""}`
      : "Event count unavailable"
    : session?.message_count !== null
      ? `${knownCount} messages`
      : is_loading
        ? "Loading events…"
        : "Event count unavailable";

  return (
    <main className="conversation">
      <header className="conversation__header" data-tauri-drag-region>
        <button
          aria-label="Open sessions"
          className="icon-button mobile-sidebar-button"
          onClick={on_sidebar_open}
          type="button"
        >
          <PanelIcon />
        </button>
        {session ? (
          <div className="conversation__identity">
            <div className="conversation__title-row">
              <h2 title={sessionDisplayTitle(session)}>{sessionDisplayTitle(session)}</h2>
              <span className="provider-badge" data-provider={session.provider}>
                {providerLabel(session.provider)}
              </span>
            </div>
            <details className="conversation__metadata" key={session.session_key}>
              <summary>
                {session.is_subagent ? "Subagent · " : ""}{session.project ?? session.cwd ?? "Unassigned project"}
                <ChevronIcon />
              </summary>
              <div className="conversation__metadata-panel">
                <p title={`${session.cwd ?? "Unassigned project"}\nSession ${session.session_id}`}>
                  {session.is_subagent ? (
                    <>
                      <span title={childDetail ?? undefined}>
                        {childDetail ? `Subagent · ${childDetail}` : "Subagent"}
                      </span>
                      <span aria-hidden="true"> · </span>
                    </>
                  ) : null}
                  {session.project ?? session.cwd ?? "Unassigned project"}
                  <span aria-hidden="true"> · </span>
                  <span
                    aria-label={`Session ${session.session_id}`}
                    className="conversation__session-id"
                  >
                    {shortSessionId(session.session_id)}
                  </span>
                  <span aria-hidden="true"> · </span>
                  {countLabel}
                </p>
                <p className="conversation__path">{session.cwd}</p>
              </div>
            </details>
          </div>
        ) : (
          <div className="conversation__identity conversation__identity--empty">
            <h2>Session viewer</h2>
            <p>Conversations across every known provider</p>
          </div>
        )}
        <button
          aria-label={hideLifecycle ? "Show lifecycle" : "Hide lifecycle"}
          aria-pressed={hideLifecycle}
          className="conversation__filter"
          disabled={!session}
          onClick={() => setHideLifecycle((hidden) => !hidden)}
          title={hideLifecycle
            ? "Show lifecycle events and mid-turn usage again."
            : "Hide routine lifecycle, session, configuration, and metadata events without content, plus mid-turn usage. Keep end-of-turn usage and meaningful activity."}
          type="button"
        >
          <FilterIcon />
          <span className="conversation__filter-label">{hideLifecycle ? "Show lifecycle" : "Hide lifecycle"}</span>
        </button>
        <button
          aria-label={inspector_open ? "Close event inspector" : "Open event inspector"}
          aria-pressed={inspector_open}
          className="icon-button inspector-toggle"
          disabled={!selected_event_key}
          onClick={on_inspector_toggle}
          title="Event inspector"
          type="button"
        >
          <InspectorIcon />
        </button>
      </header>

      {outstanding_questions.length > 0 ? <div className="question-attention" role="status" aria-label="Unanswered questions">
        {outstanding_questions.map((question) => <button type="button" key={question.event_key}
          className="question-attention__item" data-blocking={question.requires_input}
          onClick={() => { scroll.pause(); on_question_open?.(question.event_key); }}>
          <strong>{question.requires_input ? "Input required" : "Question available"}</strong>
          <span>{question.unanswered_count} unanswered {question.unanswered_count === 1 ? "question" : "questions"} · View</span>
        </button>)}
      </div> : null}

      <div className="conversation__history">
        <div
          className="conversation__timeline"
          ref={scroll.timelineRef}
          onScroll={scroll.onScroll}
          onWheel={(event) => {
            if (event.deltaY !== 0) scroll.noteUserScroll(event.deltaY < 0);
          }}
          onTouchMove={() => scroll.noteUserScroll()}
          onPointerDown={(event) => {
            if (event.target === event.currentTarget) scroll.noteUserScroll();
          }}
          onKeyDown={(event) => {
            if (["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End"].includes(event.key)) {
              scroll.noteUserScroll(["ArrowUp", "PageUp", "Home"].includes(event.key));
            }
          }}
        >
          {!session ? (
            <StateView
              message="Browse your conversations, search by title, or filter by provider to pick up where you left off."
              action_label="Browse sessions"
              on_action={on_sidebar_open}
              title="Select a session"
            />
          ) : null}

          {session && is_loading && events.length === 0 ? (
            <div className="timeline-loading">
              <LoadingRows count={5} />
            </div>
          ) : null}

          {session && !is_loading && error && events.length === 0 ? (
            <StateView
              action_label="Try again"
              message={error}
              on_action={on_retry}
              title="Conversation unavailable"
              tone="error"
            />
          ) : null}

          {session && !is_loading && !error && events.length === 0 ? (
            <StateView
              message="The provider returned a valid session with no normalized events."
              title="No events in this session"
            />
          ) : null}

          {session && events.length > 0 ? (
            <div className="timeline" aria-label="Session event timeline" ref={scroll.contentRef}>
              {history_status && history_status !== "complete" ? (
                <div className="history-notice" role="status">
                  This provider exposes only part of the subagent history.
                </div>
              ) : null}
              {error ? (
                <div className="pagination-error" role="alert">
                  <span>{error}</span>
                  <button className="text-button" onClick={on_retry} type="button">
                    Reload
                  </button>
                </div>
              ) : null}
              {has_older ? (
                <button
                  className="page-button"
                  disabled={is_loading_older}
                  onClick={loadOlder}
                  type="button"
                >
                  {is_loading_older ? "Loading earlier turns…" : "Load earlier turns"}
                </button>
              ) : (
                <div className="timeline-boundary">
                  <span />
                  <span>Session start</span>
                  <span />
                </div>
              )}

              {hiddenCount > 0 ? (
                <p className="event-filter-notice" role="status">
                  {visibleEvents.length === 0 ? "No events match this filter in the loaded range. " : ""}
                  {hiddenCount} {hiddenCount === 1 ? "event hidden" : "events hidden"} in this loaded range.
                </p>
              ) : null}

              {visibleEvents.map((event) => (
                <div data-reading-slot={event.slot_key ?? event.event_key} data-reading-type={event.type}
                  data-reading-timestamp={event.timestamp ?? ""}
                  data-event-key={event.event_key} data-scroll-key={event.event_key} key={`${session.session_key}:${event.event_key}`}>
                <EventCard
                  expanded_activity_keys={expanded_activity_keys}
                  expanded_activities={expanded_activities}
                  session_key={session.session_key}
                  hide_lifecycle={hideLifecycle}
                  button_id={eventButtonId(event.event_key)}
                  event={event}
                  detail={event.event_key === expanded_event_key ? expanded_detail : null}
                  detail_error={event.event_key === expanded_event_key ? expanded_detail_error : null}
                  detail_loading={
                    event.event_key === expanded_event_key && expanded_detail_loading
                  }
                  is_expanded={event.event_key === expanded_event_key}
                  is_selected={event.event_key === selected_event_key}
                  key={`${session.session_key}:${event.event_key}`}
                  on_trajectory_load_newer={on_trajectory_load_newer}
                  on_trajectory_load_older={on_trajectory_load_older}
                  on_trajectory_retry={on_trajectory_retry}
                  on_trajectory_event_toggle={on_trajectory_event_toggle}
                  on_trajectory_retry_expanded_detail={on_trajectory_retry_expanded_detail}
                  on_select={on_event_select}
                  on_toggle={on_event_toggle}
                  on_open_related_session={(target) => {
                    if (session) {
                      on_open_related_session(session.session_key, target);
                    }
                  }}
                  on_retry_detail={on_retry_expanded_detail}
                  selected_event_key={selected_event_key}
                  trajectory_page={event.type === "trajectory"
                    ? trajectory_pages.get(session.session_key)?.get(event.event_key) ?? null
                    : null}
                  trajectory_expanded_detail={event.type === "trajectory"
                    && event.event_key === trajectory_expanded_key
                    ? trajectory_expanded_detail
                    : null}
                  trajectory_expanded_detail_error={event.type === "trajectory"
                    && event.event_key === trajectory_expanded_key
                    ? trajectory_expanded_detail_error
                    : null}
                  trajectory_expanded_detail_loading={event.type === "trajectory"
                    && event.event_key === trajectory_expanded_key
                    && trajectory_expanded_detail_loading}
                  trajectory_expanded_event_key={event.type === "trajectory"
                    && event.event_key === trajectory_expanded_key
                    ? trajectory_expanded_event_key
                    : null}
                />
                </div>
              ))}

              {has_newer ? (
                <button
                  className="page-button"
                  disabled={is_loading_newer}
                  onClick={on_load_newer}
                  type="button"
                >
                  {is_loading_newer ? "Loading newer events…" : "Load newer events"}
                </button>
              ) : (
                <div className="timeline-boundary timeline-boundary--end">
                  <span />
                  <span>{formatTimestamp(events[events.length - 1]?.timestamp ?? null)}</span>
                  <span />
                </div>
              )}
            </div>
          ) : null}
        </div>
        {pending_live_activity || !scroll.isFollowing ? (
          <button
            aria-label={pending_live_activity ? "New activity · Jump to latest" : "Jump to latest"}
            className="conversation__jump"
            title="Jump to latest"
            type="button"
            onClick={() => {
              scroll.jumpToLatest();
              on_show_live_activity?.();
            }}
          >
            {pending_live_activity ? <span>New activity</span> : null}
            <SendIcon />
          </button>
        ) : null}
      </div>
      <SessionComposer session={session} on_accepted={on_input_accepted} />
    </main>
  );
}
