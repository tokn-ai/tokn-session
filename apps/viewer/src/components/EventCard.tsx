import { useEffect, useMemo, useState } from "react";
import type {
  AgentActivityCardSummary,
  EventDetail,
  ExpandedActivityState,
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
  timestampMs,
} from "../lib/state";
import {
  ChevronIcon,
  BranchIcon,
  EditFileIcon,
  InspectorIcon,
  ReadFileIcon,
  ReasoningIcon,
  SearchIcon,
  TechnicalIcon,
  TerminalIcon,
  ToolIcon,
  UsageIcon,
  WarningIcon,
} from "./Icons";
import { DeferredActivityGroup } from "./DeferredActivityGroup";
import { ActivityGroup, groupActivity } from "./ActivityGroup";
import { modelChangeLabels } from "../lib/sessionModels";
import { ToolTextSection } from "./ToolTextSection";
import { ToolInputDetails } from "./ToolInputDetails";
import "./ToolActivity.css";
import { MessageImages } from "./MessageImages";
import { toolOutputImages } from "../lib/toolOutputImages";
import { MessageCard } from "./MessageCard";
import type { TechnicalCardHeading } from "./CardPresentation";
import { ReasoningCard, reasoningHeading } from "./ReasoningCard";
import { CompactionCard } from "./CompactionCard";
import { QuestionCard } from "./QuestionCard";
import { UsageCard, usageHeading } from "./UsageCard";
import { AgentCommunicationCard, agentCommunicationHeading, isAgentCommunication } from "./AgentCommunicationCard";
import { DetailRefreshError } from "./DetailRefreshError";
import { isBookkeepingEvent } from "../lib/eventFilter";
import { readingEventKey } from "../lib/readingPosition";

interface EventCardProps {
  question_is_pending?: boolean;
  on_question_accepted?: (session_key: string) => void;
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
  activity_pages?: ReadonlyMap<string, TrajectoryEventPageState>;
  on_activity_load?: (group_key: string) => void;
  on_activity_visibility?: (group_key: string, visible: boolean) => void;
  on_trajectory_retry?: (trajectory_key: string) => void;
  expanded_activity_keys?: Set<string>;
  expanded_activities?: Map<string, ExpandedActivityState>;
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

const toolActions = new Map(Object.entries({
  shell: "Shell", terminal: "Terminal", code_execution: "Code", file_read: "Read", file_write: "Write",
  file_edit: "Edit", search: "Search", web: "Web", task: "Task",
}));

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
  const action = toolActions.get(tool.kind) ?? "Tool";
  switch (tool.kind) {
    case "code_execution":
      return {
        action,
        primary: tool.language?.trim()
          ? `${humanize(tool.language)} code`
          : fallback,
        secondary: tool.provider_tool_name?.trim() || null,
        monospace: false,
      };
    case "shell":
      return {
        action,
        primary: tool.command?.trim() || fallback,
        secondary: tool.cwd,
        monospace: tool.command !== null,
      };
    case "terminal": {
      const session = tool.terminal_session_id?.trim();
      const terminalAction = tool.terminal_action;
      const primary = terminalAction === "wait"
        ? `Wait for ${session ? `terminal ${session}` : "terminal"}`
        : terminalAction === "send"
          ? `Send ${tool.chars_len ?? 0} characters${session ? ` to terminal ${session}` : ""}`
          : fallback;
      return {
        action,
        primary,
        secondary: tool.wait_ms === null || tool.wait_ms === undefined
          ? null
          : `Up to ${formatDuration(tool.wait_ms)}`,
        monospace: false,
      };
    }
    case "file_read":
      return {
        action,
        primary: tool.path?.trim() || fallback,
        secondary: null,
        monospace: tool.path !== null,
      };
    case "file_write":
      return {
        action,
        primary: tool.path?.trim() || fallback,
        secondary: tool.bytes === null ? null : formatBytes(tool.bytes),
        monospace: tool.path !== null,
      };
    case "file_edit":
      return {
        action,
        primary: tool.path?.trim() || fallback,
        secondary: changeSummary(tool),
        monospace: tool.path !== null,
      };
    case "search":
      return {
        action,
        primary: tool.query?.trim() || fallback,
        secondary: null,
        monospace: false,
      };
    case "web":
      return {
        action,
        primary: tool.url?.trim() || fallback,
        secondary: null,
        monospace: tool.url !== null,
      };
    case "task":
      return {
        action,
        primary: tool.task_title?.trim() || fallback,
        secondary: null,
        monospace: false,
      };
    default:
      return {
        action,
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
 * recorded value while displaying whole seconds rather than milliseconds.
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
  ];
  const parts: string[] = [];
  for (const [unitMs, label] of units) {
    const count = remaining / unitMs;
    if (count > 0n) {
      parts.push(`${count.toString()}${label}`);
      remaining %= unitMs;
    }
  }
  return parts.join(" ") || "0s";
}

function trajectoryFacts(trajectory: TrajectoryCardSummary): string {
  const facts = [];
  if (trajectory.tool_count > 0) {
    facts.push(pluralize(trajectory.tool_count, "tool call"));
  }
  if (trajectory.reasoning_count > 0) {
    facts.push(pluralize(trajectory.reasoning_count, "reasoning step"));
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
  return facts.length ? facts.join(" · ") : pluralize(trajectory.event_count, "event");
}

function WorkPreview({ trajectory, id }: { trajectory: TrajectoryCardSummary; id: string }) {
  const previews = trajectory.tool_previews;
  if (!previews?.length) return null;
  const earlier = trajectory.tool_count - previews.length;
  const earlierLabel = earlier > 0 ? pluralize(earlier, "earlier tool call") : "";
  const rows = previews.map(({ kind, label, status }) => ({
    action: toolActions.get(kind) ?? "Tool",
    label,
    status: ["failed", "running", "pending"].includes(status) ? status : "",
  }));
  const descriptions = rows.map((row) => [row.action, row.label, humanize(row.status)].filter(Boolean).join(" "));
  // The recorded window contains at most three operations. Keep a current or
  // failed operation in sight even if a later routine command has completed.
  const recent = [...rows].reverse();
  const active = trajectory.status === "working" && recent.find(row => row.status === "running" || row.status === "pending");
  const current = active || recent.find(row => row.status === "failed") || recent[0];
  return <span className="work-preview">
    <span id={id} className="sr-only">{[earlierLabel, ...descriptions].filter(Boolean).join(". ")}</span>
    {current.status && <span aria-hidden="true" className="work-preview__state" data-status={current.status}>{humanize(current.status)}</span>}
    <span aria-hidden="true" className="work-preview__label" title={current.label}>{current.label}</span>
    {trajectory.tool_count > 1 && <span aria-hidden="true" className="work-preview__count">+{trajectory.tool_count - 1}</span>}
    {trajectory.error_count > 0 && current.status !== "failed" && <span aria-hidden="true" className="work-preview__state" data-status="failed">{pluralize(trajectory.error_count, "error")}</span>}
  </span>;
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
  const start = timestampMs(trajectory?.started_at ?? null);
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

/** Keep the ticking clock local so that open tool rows and logs do not rerender each second. */
function WorkHeader({ event, button_id, expanded, on_select, on_toggle }: {
  event: EventSummary; button_id: string; expanded: boolean;
  on_select: (key: string) => void; on_toggle: (key: string) => void;
}) {
  const heading = useTrajectoryHeading(event);
  const factsId = heading.secondary ? `${button_id}-facts` : undefined;
  const previewId = !expanded && !event.is_hidden && event.trajectory?.tool_previews?.length ? `${button_id}-preview` : undefined;
  const failures = event.trajectory?.error_count ?? 0;
  return <div className="trajectory-section__header">
    <button type="button" className="trajectory-section__toggle" aria-label={heading.primary}
      aria-expanded={expanded} aria-controls={`${button_id}-details`}
      aria-describedby={[factsId, previewId].filter(Boolean).join(" ") || undefined}
      onClick={() => on_toggle(event.event_key)}>
      {event.trajectory?.status === "working" && <span aria-hidden="true" className="inline-spinner" />}
      <span className="trajectory-section__label" id={`${button_id}-label`}>{heading.primary}</span>
      <ChevronIcon className={expanded ? "chevron chevron--open" : "chevron"} />
      {heading.secondary && <span id={factsId} className={previewId || expanded || failures ? "sr-only" : "trajectory-section__facts"}>{heading.secondary}</span>}
      {previewId && event.trajectory && <WorkPreview trajectory={event.trajectory} id={previewId} />}
      {!previewId && failures > 0 && <span aria-hidden="true" className="work-preview__state" data-status="failed">{pluralize(failures, "error")}</span>}
    </button>
    <button type="button" className="trajectory-section__inspect" id={button_id} aria-label={`Inspect ${heading.primary}`}
      onClick={() => on_select(event.event_key)}><InspectorIcon /></button>
  </div>;
}

function changeSummary(tool: ToolCardSummary): string | null {
  const changes = [
    tool.added == null ? null : `+${tool.added}`,
    tool.removed == null ? null : `−${tool.removed}`,
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
  if (event.type === "error" || event.type === "unknown" || event.is_error
    || event.tool?.status === "failed" || (event.tool?.exit_code ?? 0) !== 0) {
    return <WarningIcon />;
  }
  if (event.type === "agent_activity" || event.tool?.kind === "task") {
    return <BranchIcon />;
  }
  if (event.type === "reasoning") {
    return <ReasoningIcon />;
  }
  if (event.type === "usage") {
    return <UsageIcon />;
  }
  if (event.type === "tool_call") {
    if (event.tool?.kind === "shell" || event.tool?.kind === "terminal" || event.tool?.kind === "code_execution") {
      return <TerminalIcon />;
    }
    if (event.tool?.kind === "search" || event.tool?.kind === "web") return <SearchIcon />;
    if (event.tool?.kind === "file_edit" || event.tool?.kind === "file_write") return <EditFileIcon />;
    if (event.tool?.kind === "file_read") return <ReadFileIcon />;
    return <ToolIcon />;
  }
  return <TechnicalIcon />;
}

function eventTone(event: EventSummary): string {
  if (event.type === "unknown") {
    return "unknown";
  }
  if (event.type === "error" || event.is_error || event.tool?.status === "failed" || (event.tool?.exit_code ?? 0) !== 0) {
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
    || event.type === "question_request" || isAgentCommunication(event);
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
  on_open_related_session,
  on_retry,
  on_retry_child_detail,
  on_select,
  on_toggle,
  on_toggle_child,
  page,
  activity_pages,
  on_activity_load,
  on_activity_visibility,
  selected_event_key,
  expanded_child_detail,
  expanded_child_detail_error,
  expanded_child_detail_loading,
  expanded_child_event_key,
  expanded_activity_keys,
  expanded_activities,
}: {
  session_key?: string;
  hide_lifecycle: boolean;
  button_id: string;
  event: EventSummary;
  is_expanded: boolean;
  is_selected: boolean;
  on_open_related_session?: (target: SessionSummary) => void;
  on_retry?: (trajectory_key: string) => void;
  on_retry_child_detail?: (trajectory_key: string, event_key: string) => void;
  on_select: (event_key: string) => void;
  on_toggle: (event_key: string) => void;
  on_toggle_child?: (trajectory_key: string, event_key: string) => void;
  page: TrajectoryEventPageState | null | undefined;
  activity_pages?: ReadonlyMap<string, TrajectoryEventPageState>;
  on_activity_load?: (group_key: string) => void;
  on_activity_visibility?: (group_key: string, visible: boolean) => void;
  selected_event_key: string | null | undefined;
  expanded_child_detail: EventDetail | null;
  expanded_child_detail_error: string | null;
  expanded_child_detail_loading: boolean;
  expanded_child_event_key: string | null;
  expanded_activity_keys?: Set<string>;
  expanded_activities?: Map<string, ExpandedActivityState>;
}) {
  const regionId = `${button_id}-details`;
  const labelId = `${button_id}-label`;
  // Determine identity and boundaries before filtering. Revealing routine rows
  // must not unload a group's tool output or disturb its original chronology.
  const activityGroups = useMemo(() => {
    const groups = groupActivity(page?.events ?? []);
    return groups.map(group => {
      const substantive = group.filter(child => !isBookkeepingEvent(child));
      const deferred = group[0].type === "activity_group";
      const singleTool = deferred ? !!group[0].single_tool_key
        : substantive.length === 1 && substantive[0].type === "tool_call" && !substantive[0].is_hidden;
      const count = deferred ? group[0].child_keys?.length : group.length;
      // A single short sequence already has a disclosure on the enclosing turn.
      // Keep larger groups and groups separated by agent commentary independently lazy.
      const shortSequence = groups.length === 1 && count !== undefined && count > 0 && count <= 4;
      return {
        group_key: group[0].event_key,
        events: hide_lifecycle ? substantive : group,
        hidden_count: hide_lifecycle ? group.length - substantive.length : 0,
        inline: shortSequence || (event.trajectory?.tool_count === 1 && singleTool),
      };
    });
  }, [page?.events, hide_lifecycle, event.trajectory?.tool_count]);
  const hiddenCount = activityGroups.reduce((count, group) => count + group.hidden_count, 0);
  const visibleCount = (page?.events.length ?? 0) - hiddenCount;
  const renderChild = (childEvent: EventSummary) => (
    <div
      className="trajectory-section__item"
      data-scroll-key={JSON.stringify([readingEventKey(event), readingEventKey(childEvent)])}
      key={childEvent.event_key}
      role="listitem"
    >
      <EventCard
        session_key={session_key}
        hide_lifecycle={hide_lifecycle}
        button_id={eventButtonId(childEvent.event_key)}
        detail={expanded_activities?.get(childEvent.event_key)?.detail ?? (childEvent.event_key === expanded_child_event_key
          ? expanded_child_detail
          : null)}
        detail_error={expanded_activities?.get(childEvent.event_key)?.error ?? (childEvent.event_key === expanded_child_event_key
          ? expanded_child_detail_error
          : null)}
        detail_loading={expanded_activities?.get(childEvent.event_key)?.is_loading
          ?? (childEvent.event_key === expanded_child_event_key && expanded_child_detail_loading)}
        event={childEvent}
        is_expanded={expanded_activity_keys?.has(childEvent.event_key) ?? (childEvent.event_key === expanded_child_event_key)}
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
  );
  return (
    <section className="trajectory-section transcript-work" data-status={event.trajectory?.status} data-selected={is_selected}>
      <WorkHeader event={event} button_id={button_id} expanded={is_expanded} on_toggle={on_toggle} on_select={on_select} />

      {is_expanded || page?.has_loaded || (page?.events.length ?? 0) > 0 ? (
        <div
          hidden={!is_expanded}
          aria-labelledby={labelId}
          className="trajectory-section__body"
          id={regionId}
          role="region"
        >
          {!page || (page.events.length === 0 && (page.is_loading || (!page.has_loaded && !page.error))) ? (
            <div className="trajectory-section__state" role="status">
              <span className="inline-spinner" aria-hidden="true" />
              Loading work groups…
            </div>
          ) : (
            <>
              {page.error ? (
                <div className="trajectory-section__error" role="alert">
                  <span>
                    <strong>Work groups unavailable</strong>
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

              {hiddenCount > 0 ? (
                <p className="event-filter-notice" role="status">
                  {visibleCount === 0 ? "No events match this filter in this turn. " : ""}
                  {hiddenCount} {hiddenCount === 1 ? "event hidden" : "events hidden"} in this turn.
                </p>
              ) : null}

              {visibleCount > 0 ? (
                <div aria-label="Events in this turn" className="trajectory-section__events" role="list">
                  {activityGroups.filter(group => group.events.length > 0).map(({ group_key, events: group, inline }) => group[0].type === "activity_group" ? (
                    <DeferredActivityGroup key={group_key} event={group[0]} page={activity_pages?.get(group_key)}
                      selected_event_key={selected_event_key} on_load={on_activity_load} on_retry={on_retry} parent_visible={is_expanded}
                      inline={inline} reveal={group[0].child_keys?.some((key) => expanded_activity_keys?.has(key) ?? key === expanded_child_event_key)}
                      on_visibility={on_activity_visibility}>
                      {(children) => {
                        const visible = children.filter((child) => !hide_lifecycle || !isBookkeepingEvent(child));
                        return visible.length ? visible.map(renderChild)
                          : <p className="event-filter-notice" role="status">Lifecycle events are hidden.</p>;
                      }}
                    </DeferredActivityGroup>
                  ) : (
                    <ActivityGroup key={group_key} events={group} selected_event_key={selected_event_key} inline={inline}
                      reveal={group.some((child) => expanded_activity_keys?.has(child.event_key)
                        ?? child.event_key === expanded_child_event_key)}>
                      {() => group.map(renderChild)}
                    </ActivityGroup>
                  ))}
                </div>
              ) : page.events.length === 0 && !page.error ? (
                <p className="trajectory-section__empty" role="status">No events in this turn.</p>
              ) : null}


            </>
          )}
        </div>
      ) : null}
    </section>
  );
}

function ToolOutput({
  session_key,
  detail: loaded_detail,
  error,
  event,
  is_loading,
  on_retry,
}: {
  session_key?: string;
  detail: EventDetail | null;
  error: string | null;
  event: EventSummary;
  is_loading: boolean;
  on_retry: () => void;
}) {
  const detail = loaded_detail?.event_key === event.event_key ? loaded_detail : null;
  if (event.is_hidden || detail?.is_hidden) {
    return <p className="tool-output__empty">Tool output is hidden by the provider.</p>;
  }
  const { images, revision } = toolOutputImages(event, detail);
  const imageContent = images.length ? session_key
    ? <MessageImages session_key={session_key} event_key={event.event_key} revision={revision} images={images} aria_label="Tool output images" />
    : <span className="message-image__status">Image unavailable</span> : null;
  if (is_loading && !detail) {
    return (
      <><div className="tool-output__state" role="status">
        <span className="inline-spinner" aria-hidden="true" />
        Loading tool output…
      </div>{imageContent}</>
    );
  }
  if (error && !detail) {
    return (
      <><div className="tool-output__error" role="alert">
        <span>
          <strong>Output unavailable</strong>
          <small>{error}</small>
        </span>
        <button className="text-button" onClick={on_retry} type="button">
          Try again
        </button>
      </div>{imageContent}</>
    );
  }

  const output = detail?.tool_output ?? null;
  const hasOutput = Boolean(output?.sections.length || images.length);
  const isPending = event.tool?.status === "pending" || event.tool?.status === "running"
    || (event.tool?.status === undefined && event.phase !== null && event.phase !== "finished");
  const toolLabel = toolHeading(event).primary;
  const isShell = event.tool?.kind === "shell" || event.tool?.kind === "terminal";
  const input = detail && <ToolInputDetails kind={event.tool?.kind ?? "unknown"} path={event.tool?.path} record={detail.event} has_output={hasOutput} />;
  const sections = output?.sections ?? [];
  const streams = sections.map((section, index) => {
    const isGeneric = !section.label || section.label.toLowerCase() === "output";
    return <ToolTextSection key={`${event.event_key}-${index}`}
      terminal={isShell} text={section.text} format={section.format}
      heading={sections.length === 1 && isShell && isGeneric ? undefined : section.label ?? "Output"}
      label={`${toolLabel} ${isGeneric ? "output" : `${section.label} output`}`} />;
  });
  return (
    <div aria-busy={is_loading} className={isShell ? "tool-output shell-output" : "tool-output"}>
      <DetailRefreshError error={error} on_retry={on_retry} />
      {!isShell && input}
      {isShell && streams.length ? <div className="tool-output__streams">{streams}</div> : streams}
      {imageContent}
      {!hasOutput && <p className="tool-output__empty" role="status">
        {isPending ? "Output is not available yet." : "No output was captured for this tool call."}
      </p>}
      {output?.truncated ? (
        <p className="tool-output__notice" role="status">
          Output preview is truncated
          {output.original_size_bytes > 0 ? ` from ${formatBytes(output.original_size_bytes)}` : ""}.
          Inspect the event for the complete bounded detail.
        </p>
      ) : null}
      {isShell && input}
    </div>
  );
}

export function EventCard({
  question_is_pending = false,
  on_question_accepted,
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
  activity_pages,
  on_activity_load,
  on_activity_visibility,
  on_trajectory_retry,
  expanded_activity_keys,
  expanded_activities,
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
        expanded_activity_keys={expanded_activity_keys}
        expanded_activities={expanded_activities}
        expanded_child_detail={trajectory_expanded_detail ?? null}
        expanded_child_detail_error={trajectory_expanded_detail_error ?? null}
        expanded_child_detail_loading={trajectory_expanded_detail_loading ?? false}
        expanded_child_event_key={trajectory_expanded_event_key ?? null}
        is_expanded={is_expanded}
        is_selected={is_selected}
        on_open_related_session={on_open_related_session}
        on_retry={on_trajectory_retry}
        on_retry_child_detail={on_trajectory_retry_expanded_detail}
        on_select={on_select}
        on_toggle={on_toggle}
        on_toggle_child={on_trajectory_event_toggle}
        page={trajectory_page}
        activity_pages={activity_pages}
        on_activity_load={on_activity_load}
        on_activity_visibility={on_activity_visibility}
        selected_event_key={selected_event_key}
      />
    );
  }

  if (event.type === "model_changed" && event.model_change) {
    const labels = modelChangeLabels(event.model_change.previous, event.model_change.current);
    return <div className="model-change" role="separator" aria-label={`Model changed to ${labels.current}`}>
      <span className="model-change__label" title={`Previously ${labels.previous}`}>
        Switched to {labels.current}
      </span>
    </div>;
  }

  if (isMessage(event) || event.type === "question_reply") {
    return (
      <MessageCard event={event} session_key={session_key} button_id={button_id} is_selected={is_selected} on_select={on_select} />
    );
  }

  const heading = cardHeading(event);
  const title = heading?.primary ?? event.title;
  const regionId = `${button_id}-details`;
  const labelId = `${button_id}-label`;
  const cardIsExpanded = usesControlledExpansion(event) ? is_expanded : isLocallyExpanded;
  const isShell = event.tool?.kind === "shell" || event.tool?.kind === "terminal";
  const status = eventStatus(event) ?? (isShell && cardIsExpanded && event.tool?.exit_code === 0
    ? { label: "exit 0", tone: "success" } : null);
  const secondary = event.tool?.kind === "shell" && !cardIsExpanded ? null : heading?.secondary;
  const isCommunication = isAgentCommunication(event);
  const subagentTarget = event.type === "agent_activity"
    ? (isCommunication ? event.agent_activity?.actor : event.agent_activity?.target) ?? null
    : null;
  return (
    <article
      className="technical-event"
      data-selected={is_selected}
      data-tone={eventTone(event)}
      data-expanded={cardIsExpanded}
      data-tool-kind={event.tool?.kind}
    >
      <div className="technical-event__header">
        <button
          aria-controls={regionId}
          aria-expanded={cardIsExpanded}
          aria-label={heading?.action ? `${heading.action}: ${title}` : title}
          aria-describedby={[status && `${button_id}-status`, secondary && `${button_id}-context`].filter(Boolean).join(" ") || undefined}
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
              {heading?.action && event.tool?.kind !== "shell" ? <span className="tool-action">{heading.action}</span> : null}
              <span
                className="technical-event__title"
                data-monospace={heading?.monospace || undefined}
                id={labelId}
                title={title}
              >
                {title}
              </span>
            </span>
            {secondary ? (
              <span className="technical-event__secondary" title={secondary} id={`${button_id}-context`}>
                {event.tool?.kind === "file_edit" ? <>
                  {event.tool.added != null && <span className="tool-change__added">+{event.tool.added}</span>}
                  {event.tool.added != null && event.tool.removed != null ? " " : null}
                  {event.tool.removed != null && <span className="tool-change__removed">−{event.tool.removed}</span>}
                </> : secondary}
              </span>
            ) : null}
          </span>
          <span className="technical-event__meta">
            {status ? (
              <span className="event-phase" data-tone={status.tone} id={`${button_id}-status`}>{status.label}</span>
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
            {event.summary_truncated && !event.is_hidden ? "Full detail" : <InspectorIcon />}
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
              session_key={session_key}
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
          ) : event.type === "question_request" ? (
            <QuestionCard session_key={session_key} is_pending={question_is_pending} on_accepted={on_question_accepted} event={event} detail={detail} error={detail_error} is_loading={detail_loading} on_retry={on_retry_detail} />
          ) : event.type === "agent_activity" ? (
            <AgentActivityBody activity={event.agent_activity} event={event} />
          ) : (
            <p>{event.is_hidden ? "Hidden extension event" : event.summary || "No summary available."}</p>
          )}
          {event.type !== "tool_call" ? <span className="event-kind-label">{event.type.replace(/_/g, " ")}</span> : null}
        </div>
      ) : null}
    </article>
  );
}
