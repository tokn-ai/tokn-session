import { useEffect, useId, useState, type ReactNode } from "react";
import type { EventSummary, TrajectoryEventPageState } from "../lib/types";
import { ChevronIcon, SearchIcon } from "./Icons";

/** Unloaded groups have summaries, never a partial set of child rows. */
export function DeferredActivityGroup({ event, page, selected_event_key, on_load, on_retry, on_visibility, parent_visible, children }: {
  event: EventSummary;
  page?: TrajectoryEventPageState;
  selected_event_key?: string | null;
  on_load?: (group_key: string) => void;
  on_retry?: (group_key: string) => void;
  on_visibility?: (group_key: string, visible: boolean) => void;
  parent_visible: boolean;
  children: (events: EventSummary[]) => ReactNode;
}) {
  const id = useId();
  const [expanded, setExpanded] = useState(false);
  const selected = !!selected_event_key && event.child_keys?.includes(selected_event_key);
  useEffect(() => {
    on_visibility?.(event.event_key, parent_visible && expanded);
    return () => on_visibility?.(event.event_key, false);
  }, [event.event_key, expanded, parent_visible, on_visibility]);
  useEffect(() => { if (selected) setExpanded(true); }, [selected]);
  useEffect(() => {
    if (parent_visible && expanded && !page?.has_loaded && !page?.is_loading && !page?.error) on_load?.(event.event_key);
  }, [parent_visible, expanded, page, on_load, event.event_key]);
  return <div className="activity-group" role="listitem">
    <button className="activity-group__toggle" aria-expanded={expanded} aria-controls={id}
      onClick={() => setExpanded((current) => !current)} type="button">
      <SearchIcon /><span>{event.summary}{event.is_error ? " · Failed activity" : ""}</span>
      <ChevronIcon className={expanded ? "chevron chevron--open" : "chevron"} />
    </button>
    <div id={id} hidden={!expanded} className="activity-group__events" role="list" aria-label={event.summary}>
      {page?.events.length ? children(page.events) : expanded && !page?.error ? <p role="status">Loading activity…</p> : null}
      {page?.error ? <div role="alert">{page.error}<button type="button" className="text-button"
        onClick={() => on_retry?.(event.event_key)}>Try again</button></div> : null}
    </div>
  </div>;
}
