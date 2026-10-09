import { useEffect, useId, useState, type ReactNode } from "react";
import type { EventSummary } from "../lib/types";
import { ChevronIcon, SearchIcon } from "./Icons";

function isExploration(event: EventSummary): boolean {
  return event.type === "tool_call" && !event.is_error
    && (event.tool?.exit_code === null || event.tool?.exit_code === undefined || event.tool.exit_code === 0)
    && event.tool?.status !== "running" && event.tool?.status !== "pending"
    && (event.tool?.kind === "file_read" || event.tool?.kind === "search");
}

/** Only adjacent exploration is folded; never reorder work or hide failures. */
export function groupActivity(events: EventSummary[]): EventSummary[][] {
  const groups: EventSummary[][] = [];
  for (const event of events) {
    const previous = groups[groups.length - 1];
    if (isExploration(event) && previous && isExploration(previous[0])) previous.push(event);
    else groups.push([event]);
  }
  return groups;
}

export function ActivityGroup({ events, selected_event_key, children }: {
  events: EventSummary[];
  selected_event_key?: string | null;
  children: ReactNode;
}) {
  const regionId = useId();
  const [expanded, setExpanded] = useState(false);
  const hasSelection = events.some((event) => event.event_key === selected_event_key);
  useEffect(() => { if (hasSelection) setExpanded(true); }, [hasSelection, selected_event_key]);
  if (events.length < 2) return children;
  const reads = events.filter((event) => event.tool?.kind === "file_read").length;
  const searches = events.length - reads;
  const facts = [reads ? `${reads} file${reads === 1 ? "" : "s"}` : null,
    searches ? `${searches} search${searches === 1 ? "" : "es"}` : null].filter(Boolean).join(", ");
  return (
    <div className="activity-group" role="listitem">
      <button className="activity-group__toggle" aria-expanded={expanded} aria-controls={regionId}
        onClick={() => setExpanded((current) => !current)} type="button">
        <SearchIcon />
        <span>Explored {facts}</span>
        <ChevronIcon className={expanded ? "chevron chevron--open" : "chevron"} />
      </button>
      <div id={regionId} hidden={!expanded} className="activity-group__events" role="list" aria-label={`Explored ${facts}`}>
        {children}
      </div>
    </div>
  );
}
