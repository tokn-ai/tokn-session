import { useEffect, useState } from "react";
import type {
  AgentActivityCardSummary,
  EventDetail,
  EventSummary,
  SessionSummary,
  ToolCardSummary,
  TrajectoryCardSummary,
  TrajectoryEventPageState,
} from "../lib/types";
import {
  eventButtonId,
  formatTimestamp,
  sessionDisplayTitle,
  shortSessionId,
  subagentDetail,
} from "../lib/state";
import {
  ChevronIcon,
  ReasoningIcon,
  TechnicalIcon,
  ToolIcon,
  UsageIcon,
  WarningIcon,
} from "./Icons";
import { MessageCard } from "./MessageCard";
import type { TechnicalCardHeading } from "./CardPresentation";
import { ReasoningCard, reasoningHeading } from "./ReasoningCard";
import { CompactionCard } from "./CompactionCard";
import { QuestionCard } from "./QuestionCard";
import { UsageCard, usageHeading } from "./UsageCard";
import { AgentCommunicationCard, agentCommunicationHeading, isAgentCommunication } from "./AgentCommunicationCard";
import { DetailRefreshError } from "./DetailRefreshError";
import { isBookkeepingEvent } from "../lib/eventFilter";

interface EventCardProps {
  session_key?: string;
  hide_lifecycle?: boolean;
  event: EventSummary;
  button_id: string;
  is_selected: boolean;
  selected_event_key?: string | null;
  is_expanded: boolean;
  detail: EventDetail | null;
  detail_error: string | null;
  detail_loading: boolean;
  on_select: (event_key: string) => void;
  on_toggle: (event_key: string) => void;
  on_open_related_session?: (target: SessionSummary) => void;
  on_retry_detail: () => void;
  trajectory_page?: TrajectoryEventPageState | null;
  on_trajectory_load_older?: (trajectory_key: string) => void;
  on_trajectory_load_newer?: (trajectory_key: string) => void;
  on_trajectory_retry?: (trajectory_key: string) => void;
  trajectory_expanded_event_key?: string | null;
  trajectory_expanded_detail?: EventDetail | null;
  trajectory_expanded_detail_error?: string | null;
  trajectory_expanded_detail_loading?: boolean;
  on_trajectory_event_toggle?: (trajectory_key: string, event_key: string) => void;
  on_trajectory_retry_expanded_detail?: (trajectory_key: string, event_key: string) => void;
}

function isMessage(event: EventSummary): boolean {
  return event.type === "message";
}

function humanize(value: string): string {
  return value
    .replace(/[_-]+/g, " ")
    .replace(/\b\w/g, (character) => character.toUpperCase());
}

function toolFallbackName(event: EventSummary, tool: ToolCardSummary): string {
  return humanize(tool.tool_name?.trim() || event.title || "Tool call");
}

function toolHeading(event: EventSummary): TechnicalCardHeading {
  const tool = event.tool;
  if (!tool) {
    return {
      action: null,
      primary: event.title || "Tool call",
      secondary: null,
      monospace: false,
    };
  }

  const fallback = toolFallbackName(event, tool);
  switch (tool.kind) {
    case "code_execution":
      return {
        action: "Code",
        primary: tool.language?.trim()
          ? `${humanize(tool.language)} code`
          : fallback,
        secondary: tool.provider_tool_name?.trim() || null,
        monospace: false,
      };
    case "shell":
      return {
        action: "Shell",
        primary: tool.command?.trim() || fallback,
        secondary: tool.cwd,
        monospace: tool.command !== null,
      };
    case "terminal": {
      const session = tool.terminal_session_id?.trim();
      const action = tool.terminal_action;
      const primary = action === "wait"
        ? `Wait for ${session ? `terminal ${session}` : "terminal"}`
        : action === "send"
          ? `Send ${tool.chars_len ?? 0} characters${session ? ` to terminal ${session}` : ""}`
          : fallback;
      return {
        action: "Terminal",
        primary,
        secondary: tool.wait_ms === null || tool.wait_ms === undefined
          ? null
          : `Up to ${formatDuration(tool.wait_ms)}`,
        monospace: false,
      };
    }
    case "file_read":
      return {
        action: "Read",
        primary: tool.path?.trim() || fallback,
        secondary: null,
        monospace: tool.path !== null,
      };
    case "file_write":
      return {
        action: "Write",
        primary: tool.path?.trim() || fallback,
        secondary: tool.bytes === null ? null : formatBytes(tool.bytes),
        monospace: tool.path !== null,
      };
    case "file_edit":
      return {
        action: "Edit",
        primary: tool.path?.trim() || fallback,
        secondary: changeSummary(tool),
        monospace: tool.path !== null,
      };
    case "search":
      return {
        action: "Search",
        primary: tool.query?.trim() || fallback,
        secondary: null,
        monospace: false,
      };
    case "web":
      return {
        action: "Web",
        primary: tool.url?.trim() || fallback,
        secondary: null,
        monospace: tool.url !== null,
      };
    case "task":
      return {
        action: "Task",
        primary: tool.task_title?.trim() || fallback,
        secondary: null,
        monospace: false,
      };
    default:
      return {
        action: "Tool",
        primary: fallback,
        secondary: null,
        monospace: false,
      };
  }
}

function agentActivityHeading(event: EventSummary): TechnicalCardHeading {
  const activity = event.agent_activity;
  const target = activity?.target ?? null;
  if (target) {
    const detail = subagentDetail(target);
    return {
      action: "Subagent",
      primary: sessionDisplayTitle(target),
      secondary: detail ?? `Session ${shortSessionId(target.session_id)}`,
      monospace: false,
    };
  }

  const targetLabel = activity?.target_agent_path?.trim()
    || activity?.target_session_id?.trim()
    || event.title
    || "Agent activity";
  return {
    action: "Subagent",
    primary: targetLabel,
    secondary: null,
    monospace: false,
  };
}

function pluralize(count: number, singular: string, plural = `${singular}s`): string {
  return `${count} ${count === 1 ? singular : plural}`;
}

/**
 * Formats a decimal millisecond value without coercing it through Number.
 *
 * The backend keeps this as a string because it may be a Rust u64, which can
 * exceed JavaScript's safe integer range. BigInt lets the UI retain every
 * recorded millisecond while still presenting a compact, exact duration.
 */
export function formatTrajectoryDuration(durationMs: string | null): string | null {
  const source = durationMs?.trim() ?? "";
  if (!/^\d+$/.test(source)) {
    return null;
  }

  let remaining: bigint;
  try {
    remaining = BigInt(source);
  } catch {
    return null;
  }

  const units: Array<[bigint, string]> = [
    [86_400_000n, "d"],
    [3_600_000n, "h"],
    [60_000n, "m"],
    [1_000n, "s"],
    [1n, "ms"],
  ];
  const parts: string[] = [];
  for (const [unitMs, label] of units) {
    const count = remaining / unitMs;
    if (count > 0n) {
      parts.push(`${count.toString()}${label}`);
      remaining %= unitMs;
    }
  }
  return parts.join(" ") || "0ms";
}

function trajectoryFacts(trajectory: TrajectoryCardSummary): string {
  const facts = [pluralize(trajectory.event_count, "event")];
  if (trajectory.tool_count > 0) {
    facts.push(pluralize(trajectory.tool_count, "tool"));
  }
  if (trajectory.reasoning_count > 0) {
    facts.push(pluralize(trajectory.reasoning_count, "reasoning", "reasoning"));
  }
  if (trajectory.agent_activity_count > 0) {
    facts.push(pluralize(trajectory.agent_activity_count, "subagent activity", "subagent activities"));
  }
  if (trajectory.error_count > 0) {
    facts.push(pluralize(trajectory.error_count, "error"));
  }
  if (trajectory.unknown_count > 0) {
    facts.push(pluralize(trajectory.unknown_count, "unknown event"));
  }
  return facts.join(" · ");
}

function useTrajectoryHeading(event: EventSummary): TechnicalCardHeading {
  const trajectory = event.trajectory;
  const working = trajectory?.status === "working";
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    if (!working) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [working, trajectory?.started_at]);
  const timestamp = trajectory?.started_at;
  const start = timestamp ? (/^\d+$/.test(timestamp) ? Number(timestamp) : Date.parse(timestamp)) : NaN;
  const duration = formatTrajectoryDuration(working && Number.isFinite(start)
    ? String(Math.max(0, now - start))
    : trajectory?.duration_ms ?? null);
  const verb = working ? "Working" : trajectory?.status === "unknown" ? "Work" : "Worked";
  return {
    action: "Turn",
    primary: duration ? `${verb} for ${duration}` : verb,
    secondary: trajectory ? trajectoryFacts(trajectory) : event.summary || null,
    monospace: false,
  };
}

function changeSummary(tool: ToolCardSummary): string | null {
  const changes = [
    tool.added === null ? null : `+${tool.added}`,
    tool.removed === null ? null : `−${tool.removed}`,
  ].filter((change): change is string => change !== null);
  return changes.length > 0 ? changes.join(" ") : null;
}

function formatBytes(bytes: number): string {
  if (bytes < 1_000) {
    return `${bytes} B`;
  }
  if (bytes < 1_000_000) {
    return `${(bytes / 1_000).toFixed(bytes < 10_000 ? 1 : 0)} KB`;
  }
  return `${(bytes / 1_000_000).toFixed(bytes < 10_000_000 ? 1 : 0)} MB`;
}

function formatDuration(milliseconds: number): string {
  if (milliseconds < 1_000) {
    return `${milliseconds} ms`;
  }
  if (milliseconds % 1_000 === 0) {
    return `${milliseconds / 1_000} s`;
  }
  return `${(milliseconds / 1_000).toFixed(1)} s`;
}

function eventIcon(event: EventSummary) {
  if (event.type === "reasoning") {
    return <ReasoningIcon />;
  }
  if (event.type === "usage") {
    return <UsageIcon />;
  }
  if (event.type === "tool_call") {
    return <ToolIcon />;
  }
  if (event.type === "unknown" || event.type === "error" || event.is_error) {
    return <WarningIcon />;
  }
  return <TechnicalIcon />;
}

function eventTone(event: EventSummary): string {
  if (event.type === "unknown") {
    return "unknown";
  }
  if (event.type === "error" || event.is_error || (event.tool?.exit_code ?? 0) !== 0) {
    return "error";
  }
  if (event.type === "reasoning") {
    return "reasoning";
  }
  if (event.type === "usage") {
    return "usage";
  }
  if (event.type === "tool_call") {
    return "tool";
  }
  return "technical";
}

function cardHeading(event: EventSummary): TechnicalCardHeading | null {
  if (event.type === "question_reply") {
    return { action: "User answers", primary: event.summary || event.title, secondary: "Recorded question reply", monospace: false };
  }
  if (event.type === "question_request") {
    return { action: "Questions", primary: event.summary || event.title, secondary: "Recorded question request", monospace: false };
  }
  if (event.type === "tool_call") {
    return toolHeading(event);
  }
  if (event.type === "agent_activity" && event.agent_activity) {
    return isAgentCommunication(event) ? agentCommunicationHeading(event) : agentActivityHeading(event);
  }
  if (event.type === "usage" && event.usage) {
    return usageHeading(event.usage);
  }
  if (event.type === "reasoning" && event.reasoning) {
    return reasoningHeading(event);
  }
  return null;
}

function usesControlledExpansion(event: EventSummary): boolean {
  return event.type === "tool_call" || event.type === "reasoning" || event.type === "compaction"
    || event.type === "question_request" || event.type === "question_reply" || isAgentCommunication(event);
}

function eventStatus(event: EventSummary): { label: string; tone: string } | null {
  if (event.type === "question_request" || event.type === "question_reply") return null;
  if (event.type === "compaction") {
    const state = event.compaction?.state;
    return state ? { label: humanize(state), tone: state === "failed" ? "error" : "neutral" } : null;
  }
  if (event.type === "agent_activity") {
    if (isAgentCommunication(event)) return null;
    const kind = event.agent_activity?.kind.trim();
    if (!kind) {
      return null;
    }
    const tone = /^(failed|interrupted|blocked)$/i.test(kind) ? "error" : "neutral";
    return { label: humanize(kind), tone };
  }
  if (event.type !== "tool_call") {
    if (!event.phase || (event.type === "reasoning" && event.phase === "finished")) {
      return null;
    }
    return { label: event.phase, tone: "neutral" };
  }
  if (event.tool?.exit_code !== null
    && event.tool?.exit_code !== undefined
    && event.tool.exit_code !== 0) {
    return { label: `exit ${event.tool.exit_code}`, tone: "error" };
  }
  if (event.is_error) {
    return { label: "failed", tone: "error" };
  }
  switch (event.tool?.status) {
    case "failed":
      return { label: "failed", tone: "error" };
    case "pending":
      return { label: "pending", tone: "neutral" };
    case "running":
      return { label: "running", tone: "neutral" };
    case "completed":
      return null;
    default:
      break;
  }
  if (event.tool?.exit_code !== null && event.tool?.exit_code !== undefined) {
    return { label: `exit ${event.tool.exit_code}`, tone: "success" };
  }
  if (event.phase && event.phase !== "finished") {
    return { label: event.phase === "started" ? "running" : event.phase, tone: "neutral" };
  }
  return null;
}

function AgentActivityBody({
  activity,
  event,
}: {
  activity: AgentActivityCardSummary | null | undefined;
  event: EventSummary;
}) {
  const target = activity?.target ?? null;
  if (!activity) {
    return <p>{event.summary || "No activity summary available."}</p>;
  }
  if (target) {
    return (
      <div className="delegation-card">
        <p>
          Recorded {humanize(activity.kind).toLowerCase()} activity for this subagent.
        </p>
        <span className="delegation-card__session" title={`Session ${target.session_id}`}>
          Session {shortSessionId(target.session_id)}
        </span>
      </div>
    );
  }
  if (activity.target_session_id || activity.target_agent_path) {
    return (
      <p>
        Child session is not available in this viewer.
        {activity.target_session_id ? ` Recorded target: ${activity.target_session_id}.` : ""}
      </p>
    );
  }
  return <p>{event.summary || "No child session was recorded for this activity."}</p>;
}

function TrajectorySection({
  session_key,
  hide_lifecycle,
  button_id,
  event,
  is_expanded,
  is_selected,
  on_load_newer,
  on_load_older,
  on_open_related_session,
  on_retry,
  on_retry_child_detail,
  on_select,
  on_toggle,
  on_toggle_child,
  page,
  selected_event_key,
  expanded_child_detail,
  expanded_child_detail_error,
  expanded_child_detail_loading,
  expanded_child_event_key,
}: {
  session_key?: string;
  hide_lifecycle: boolean;
  button_id: string;
  event: EventSummary;
  is_expanded: boolean;
  is_selected: boolean;
  on_load_newer?: (trajectory_key: string) => void;
  on_load_older?: (trajectory_key: string) => void;
  on_open_related_session?: (target: SessionSummary) => void;
  on_retry?: (trajectory_key: string) => void;
  on_retry_child_detail?: (trajectory_key: string, event_key: string) => void;
  on_select: (event_key: string) => void;
  on_toggle: (event_key: string) => void;
  on_toggle_child?: (trajectory_key: string, event_key: string) => void;
  page: TrajectoryEventPageState | null | undefined;
  selected_event_key: string | null | undefined;
  expanded_child_detail: EventDetail | null;
  expanded_child_detail_error: string | null;
  expanded_child_detail_loading: boolean;
  expanded_child_event_key: string | null;
}) {
  const heading = useTrajectoryHeading(event);
  const regionId = `${button_id}-details`;
  const labelId = `${button_id}-label`;
  const isLoadingPage = page?.is_loading_older || page?.is_loading_newer;
  const visibleEvents = hide_lifecycle
    ? page?.events.filter((event) => !isBookkeepingEvent(event)) ?? []
    : page?.events ?? [];
  const hiddenCount = (page?.events.length ?? 0) - visibleEvents.length;
  return (
    <section className="trajectory-section" data-selected={is_selected}>
      <div className="trajectory-section__header">
        <button
          aria-controls={regionId}
          aria-expanded={is_expanded}
          aria-label={heading.primary}
          className="trajectory-section__toggle"
          onClick={() => on_toggle(event.event_key)}
          type="button"
        >
          <span aria-hidden="true" className="trajectory-section__line" />
          <span className="trajectory-section__label" id={labelId}>{heading.primary}</span>
          {heading.secondary ? (
            <span className="trajectory-section__facts">{heading.secondary}</span>
          ) : null}
          <span aria-hidden="true" className="trajectory-section__line" />
          <ChevronIcon className={is_expanded ? "chevron chevron--open" : "chevron"} />
        </button>
        <button
          aria-label={`Inspect ${heading.primary}`}
          className="trajectory-section__inspect"
          id={button_id}
          onClick={() => on_select(event.event_key)}
          type="button"
        >
          Inspect
        </button>
      </div>

      {is_expanded ? (
        <div
          aria-labelledby={labelId}
          className="trajectory-section__body"
          id={regionId}
          role="region"
        >
          {!page || (page.events.length === 0 && (page.is_loading || (!page.has_loaded && !page.error))) ? (
            <div className="trajectory-section__state" role="status">
              <span className="inline-spinner" aria-hidden="true" />
              Loading turn events…
            </div>
          ) : (
            <>
              {page.error ? (
                <div className="trajectory-section__error" role="alert">
                  <span>
                    <strong>Turn events unavailable</strong>
                    <small>{page.error}</small>
                  </span>
                  <button
                    className="text-button"
                    disabled={!on_retry}
                    onClick={() => on_retry?.(event.event_key)}
                    type="button"
                  >
                    Try again
                  </button>
                </div>
              ) : null}

              {page.previous_cursor !== null ? (
                <button
                  className="trajectory-section__page-button"
                  disabled={isLoadingPage || !on_load_older}
                  onClick={() => on_load_older?.(event.event_key)}
                  type="button"
                >
                  {page.is_loading_older ? "Loading earlier events…" : "Load earlier events"}
                </button>
              ) : null}

              {hiddenCount > 0 ? (
                <p className="event-filter-notice" role="status">
                  {visibleEvents.length === 0 ? "No events match this filter in the loaded turn range. " : ""}
                  {hiddenCount} {hiddenCount === 1 ? "event hidden" : "events hidden"} in this loaded turn range.
                </p>
              ) : null}

              {visibleEvents.length > 0 ? (
                <div aria-label="Events in this turn" className="trajectory-section__events" role="list">
                  {visibleEvents.map((childEvent) => (
                    <div data-scroll-key={`${event.event_key}/${childEvent.event_key}`} key={childEvent.event_key} role="listitem">
                      <EventCard
                        session_key={session_key}
                        hide_lifecycle={hide_lifecycle}
                        button_id={eventButtonId(childEvent.event_key)}
                        detail={childEvent.event_key === expanded_child_event_key
                          ? expanded_child_detail
                          : null}
                        detail_error={childEvent.event_key === expanded_child_event_key
                          ? expanded_child_detail_error
                          : null}
                        detail_loading={childEvent.event_key === expanded_child_event_key
                          && expanded_child_detail_loading}
                        event={childEvent}
                        is_expanded={childEvent.event_key === expanded_child_event_key}
                        is_selected={childEvent.event_key === selected_event_key}
                        on_open_related_session={on_open_related_session}
                        on_retry_detail={() => {
                          on_retry_child_detail?.(event.event_key, childEvent.event_key);
                        }}
                        on_select={on_select}
                        on_toggle={(eventKey) => on_toggle_child?.(event.event_key, eventKey)}
                        selected_event_key={selected_event_key}
                      />
                    </div>
                  ))}
                </div>
              ) : page.events.length === 0 && !page.error ? (
                <p className="trajectory-section__empty" role="status">No events in this turn.</p>
              ) : null}

              {page.next_cursor !== null ? (
                <button
                  className="trajectory-section__page-button"
                  disabled={isLoadingPage || !on_load_newer}
                  onClick={() => on_load_newer?.(event.event_key)}
                  type="button"
                >
                  {page.is_loading_newer ? "Loading more events…" : "Load more events"}
                </button>
              ) : null}

              {page.total_events !== null && page.events.length < page.total_events ? (
                <p className="trajectory-section__count" role="status">
                  {hide_lifecycle ? "Loaded" : "Showing"} {page.events.length} of {page.total_events} events.
                </p>
              ) : null}
            </>
          )}
        </div>
      ) : null}
    </section>
  );
}

function ToolOutput({
  detail,
  error,
  event,
  is_loading,
  on_retry,
}: {
  detail: EventDetail | null;
  error: string | null;
  event: EventSummary;
  is_loading: boolean;
  on_retry: () => void;
}) {
  if (event.is_hidden || detail?.is_hidden) {
    return <p className="tool-output__empty">Tool output is hidden by the provider.</p>;
  }
  if (is_loading && !detail) {
    return (
      <div className="tool-output__state" role="status">
        <span className="inline-spinner" aria-hidden="true" />
        Loading tool output…
      </div>
    );
  }
  if (error && !detail) {
    return (
      <div className="tool-output__error" role="alert">
        <span>
          <strong>Output unavailable</strong>
          <small>{error}</small>
        </span>
        <button className="text-button" onClick={on_retry} type="button">
          Try again
        </button>
      </div>
    );
  }

  const output = detail?.tool_output ?? null;
  if (!output || output.sections.length === 0) {
    const isPending = event.tool?.status === "pending" || event.tool?.status === "running"
      || (event.tool?.status === undefined && event.phase !== null && event.phase !== "finished");
    return (
      <div aria-busy={is_loading}>
        <DetailRefreshError error={error} on_retry={on_retry} />
        <p className="tool-output__empty" role="status">
          {isPending ? "Output is not available yet." : "No output was captured for this tool call."}
        </p>
      </div>
    );
  }

  const toolLabel = toolHeading(event).primary;
  return (
    <div aria-busy={is_loading} className="tool-output">
      <DetailRefreshError error={error} on_retry={on_retry} />
      {output.sections.map((section, index) => (
        <section className="tool-output__section" key={`${section.label ?? "output"}-${index}`}>
          {section.label ? <h4>{section.label}</h4> : null}
          <pre
            aria-label={`${toolLabel} ${section.label ? `${section.label} output` : "output"}`}
            data-format={section.format}
            tabIndex={0}
          >
            {section.text}
          </pre>
        </section>
      ))}
      {output.truncated ? (
        <p className="tool-output__notice" role="status">
          Output preview is truncated
          {output.original_size_bytes > 0 ? ` from ${formatBytes(output.original_size_bytes)}` : ""}.
          Inspect the event for the complete bounded detail.
        </p>
      ) : null}
    </div>
  );
}

export function EventCard({
  session_key,
  hide_lifecycle = false,
  event,
  button_id,
  is_selected,
  selected_event_key,
  is_expanded,
  detail,
  detail_error,
  detail_loading,
  on_select,
  on_toggle,
  on_open_related_session,
  on_retry_detail,
  trajectory_page,
  on_trajectory_load_older,
  on_trajectory_load_newer,
  on_trajectory_retry,
  trajectory_expanded_event_key,
  trajectory_expanded_detail,
  trajectory_expanded_detail_error,
  trajectory_expanded_detail_loading,
  on_trajectory_event_toggle,
  on_trajectory_retry_expanded_detail,
}: EventCardProps) {
  const [isLocallyExpanded, setIsLocallyExpanded] = useState(
    event.type === "unknown" || event.type === "error",
  );
  const timestampLabel = formatTimestamp(event.timestamp);

  if (event.type === "trajectory") {
    return (
      <TrajectorySection
        session_key={session_key}
        hide_lifecycle={hide_lifecycle}
        button_id={button_id}
        event={event}
        expanded_child_detail={trajectory_expanded_detail ?? null}
        expanded_child_detail_error={trajectory_expanded_detail_error ?? null}
        expanded_child_detail_loading={trajectory_expanded_detail_loading ?? false}
        expanded_child_event_key={trajectory_expanded_event_key ?? null}
        is_expanded={is_expanded}
        is_selected={is_selected}
        on_load_newer={on_trajectory_load_newer}
        on_load_older={on_trajectory_load_older}
        on_open_related_session={on_open_related_session}
        on_retry={on_trajectory_retry}
        on_retry_child_detail={on_trajectory_retry_expanded_detail}
        on_select={on_select}
        on_toggle={on_toggle}
        on_toggle_child={on_trajectory_event_toggle}
        page={trajectory_page}
        selected_event_key={selected_event_key}
      />
    );
  }

  if (isMessage(event)) {
    return (
      <MessageCard event={event} session_key={session_key} button_id={button_id} is_selected={is_selected} on_select={on_select} />
    );
  }

  const heading = cardHeading(event);
  const title = heading?.primary ?? event.title;
  const status = eventStatus(event);
  const regionId = `${button_id}-details`;
  const labelId = `${button_id}-label`;
  const cardIsExpanded = usesControlledExpansion(event) ? is_expanded : isLocallyExpanded;
  const isCommunication = isAgentCommunication(event);
  const subagentTarget = event.type === "agent_activity"
    ? (isCommunication ? event.agent_activity?.actor : event.agent_activity?.target) ?? null
    : null;
  return (
    <article
      className="technical-event"
      data-selected={is_selected}
      data-tone={eventTone(event)}
    >
      <div className="technical-event__header">
        <button
          aria-controls={regionId}
          aria-expanded={cardIsExpanded}
          aria-label={heading?.action ? `${heading.action}: ${title}` : title}
          className="technical-event__toggle"
          onClick={() => {
            if (usesControlledExpansion(event)) {
              on_toggle(event.event_key);
            } else {
              setIsLocallyExpanded((expanded) => !expanded);
            }
          }}
          type="button"
        >
          <span className="technical-event__icon">{eventIcon(event)}</span>
          <span className="technical-event__heading">
            <span className="technical-event__title-row">
              {heading?.action ? <span className="tool-action">{heading.action}</span> : null}
              <span
                className="technical-event__title"
                data-monospace={heading?.monospace || undefined}
                id={labelId}
                title={title}
              >
                {title}
              </span>
            </span>
            {heading?.secondary ? (
              <span className="technical-event__secondary" title={heading.secondary}>
                {heading.secondary}
              </span>
            ) : null}
          </span>
          <span className="technical-event__meta">
            {status ? (
              <span className="event-phase" data-tone={status.tone}>{status.label}</span>
            ) : null}
          </span>
          <time dateTime={event.timestamp ?? undefined} title={timestampLabel}>
            {event.timestamp ? timestampLabel : ""}
          </time>
          <ChevronIcon className={cardIsExpanded ? "chevron chevron--open" : "chevron"} />
        </button>
        <div className="technical-event__actions">
          {subagentTarget ? (
            <button
              aria-label={`Open ${isCommunication ? "sender" : "subagent"} ${sessionDisplayTitle(subagentTarget)}`}
              className="technical-event__open-subagent"
              disabled={!on_open_related_session}
              onClick={() => on_open_related_session?.(subagentTarget)}
              type="button"
            >
              {isCommunication ? "Open sender" : "Open"}
            </button>
          ) : null}
          <button
            aria-label={`Inspect ${title}`}
            className="technical-event__inspect"
            id={button_id}
            onClick={() => on_select(event.event_key)}
            type="button"
          >
            {event.summary_truncated && !event.is_hidden ? "Full detail" : "Inspect"}
          </button>
        </div>
      </div>
      {cardIsExpanded ? (
        <div
          aria-labelledby={labelId}
          className="technical-event__body"
          id={regionId}
          role="region"
        >
          {event.type === "tool_call" ? (
            <ToolOutput
              detail={detail}
              error={detail_error}
              event={event}
              is_loading={detail_loading}
              on_retry={on_retry_detail}
            />
          ) : event.type === "usage" && event.usage ? (
            <UsageCard usage={event.usage} />
          ) : event.type === "reasoning" ? (
            <ReasoningCard
              detail={detail}
              error={detail_error}
              event={event}
              is_loading={detail_loading}
              on_retry={on_retry_detail}
            />
          ) : event.type === "compaction" ? (
            <CompactionCard
              detail={detail}
              error={detail_error}
              event={event}
              is_loading={detail_loading}
              on_retry={on_retry_detail}
            />
          ) : isCommunication ? (
            <AgentCommunicationCard
              detail={detail}
              error={detail_error}
              event={event}
              is_loading={detail_loading}
              on_retry={on_retry_detail}
            />
          ) : event.type === "question_request" || event.type === "question_reply" ? (
            <QuestionCard event={event} detail={detail} error={detail_error} is_loading={detail_loading} on_retry={on_retry_detail} />
          ) : event.type === "agent_activity" ? (
            <AgentActivityBody activity={event.agent_activity} event={event} />
          ) : (
            <p>{event.is_hidden ? "Hidden extension event" : event.summary || "No summary available."}</p>
          )}
          <span className="event-kind-label">{event.type.replace(/_/g, " ")}</span>
        </div>
      ) : null}
    </article>
  );
}
