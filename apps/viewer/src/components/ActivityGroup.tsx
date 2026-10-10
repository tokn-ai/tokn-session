import { useEffect, useId, useState, type ReactNode } from "react";
import type { EventSummary } from "../lib/types";
import { ChevronIcon, SearchIcon } from "./Icons";

/** Messages divide a turn into activity blocks; never reorder captured events. */
export function groupActivity(events: EventSummary[]): EventSummary[][] {
  const groups: EventSummary[][] = [];
  for (const event of events) {
    const previous = groups[groups.length - 1];
    if (event.type !== "message" && event.type !== "activity_group" && previous && previous[0].type !== "message" && previous[0].type !== "activity_group") previous.push(event);
    else groups.push([event]);
  }
  return groups;
}

export function activitySummary(events: EventSummary[]): string {
  const counts = new Map<string, number>();
  for (const event of events) {
    const kind = event.type === "tool_call" ? event.tool?.kind : event.type;
    const category = kind === "shell" ? "command"
      : kind === "terminal" ? "terminal"
      : kind === "code_execution" ? "code"
      : kind === "file_read" ? "read"
      : kind === "file_edit" ? "edit"
      : kind === "file_write" ? "write"
      : kind === "search" || kind === "web" ? "search"
      : kind === "reasoning" ? "reasoning"
      : kind === "agent_activity" || kind === "task" ? "agent"
      : "other";
    counts.set(category, (counts.get(category) ?? 0) + 1);
  }
  const descriptions: [string, string, string, string][] = [
    ["command", "Ran", "command", "commands"],
    ["terminal", "performed", "terminal interaction", "terminal interactions"],
    ["code", "ran", "code block", "code blocks"],
    ["read", "read", "file", "files"],
    ["edit", "edited", "file", "files"],
    ["write", "wrote", "file", "files"],
    ["search", "performed", "search", "searches"],
    ["reasoning", "reasoned", "time", "times"],
    ["agent", "recorded", "agent activity", "agent activities"],
    ["other", "recorded", "event", "events"],
  ];
  const summary = descriptions.flatMap(([category, verb, singular, plural]) => {
    const count = counts.get(category) ?? 0;
    return count ? [`${verb} ${count} ${count === 1 ? singular : plural}`] : [];
  }).join(", ");
  return summary.charAt(0).toUpperCase() + summary.slice(1);
}

export function ActivityGroup({ events, selected_event_key, reveal = false, children }: {
  events: EventSummary[];
  selected_event_key?: string | null;
  reveal?: boolean;
  children: ReactNode | (() => ReactNode);
}) {
  const regionId = useId();
  const needsAttention = events.some((event) => event.is_error || event.type === "error"
    || event.tool?.status === "running" || event.tool?.status === "pending"
    || event.tool?.status === "failed" || (event.tool?.exit_code != null && event.tool.exit_code !== 0));
  const [expanded, setExpanded] = useState(needsAttention || reveal);
  const [loaded, setLoaded] = useState(needsAttention || reveal);
  const hasSelection = events.some((event) => event.event_key === selected_event_key);
  useEffect(() => { if (hasSelection || needsAttention || reveal) { setExpanded(true); setLoaded(true); } }, [hasSelection, needsAttention, reveal, selected_event_key]);
  if (events[0]?.type === "message") return typeof children === "function" ? children() : children;
  const summary = activitySummary(events);
  const failures = events.filter((event) => event.is_error || event.type === "error"
    || event.tool?.status === "failed" || (event.tool?.exit_code != null && event.tool.exit_code !== 0)).length;
  return (
    <div className="activity-group" role="listitem">
      <button className="activity-group__toggle" aria-expanded={expanded} aria-controls={regionId}
        onClick={() => { setLoaded(true); setExpanded((current) => !current); }} type="button">
        <SearchIcon />
        <span>{summary}{failures ? <span className="activity-group__failures"> · {failures} {failures === 1 ? "failure" : "failures"}</span> : null}</span>
        <ChevronIcon className={expanded ? "chevron chevron--open" : "chevron"} />
      </button>
      <div id={regionId} hidden={!expanded} className="activity-group__events" role="list" aria-label={summary}>
        {loaded ? (typeof children === "function" ? children() : children) : null}
      </div>
    </div>
  );
}
