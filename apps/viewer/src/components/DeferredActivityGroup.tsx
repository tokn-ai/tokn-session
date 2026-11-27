import { useEffect, useId, useState, type ReactNode } from "react";
import type { EventSummary, TrajectoryEventPageState } from "../lib/types";
import { ActivityIcon, ChevronIcon } from "./Icons";

/** Unloaded groups have summaries, never a partial set of child rows. */
export function DeferredActivityGroup({ event, page, selected_event_key, on_load, on_retry, on_visibility, parent_visible, inline = false, reveal = false, children }: {
  event: EventSummary;
  page?: TrajectoryEventPageState;
  selected_event_key?: string | null;
  on_load?: (group_key: string) => void;
  on_retry?: (group_key: string) => void;
  on_visibility?: (group_key: string, visible: boolean) => void;
  parent_visible: boolean;
  inline?: boolean;
  reveal?: boolean;
  children: (events: EventSummary[]) => ReactNode;
}) {
  const id = useId();
  const running = event.phase === "running";
  const [expanded, setExpanded] = useState(Boolean(event.is_error || running));
  const open = inline || expanded;
  const selected = !!selected_event_key && event.child_keys?.includes(selected_event_key);
  useEffect(() => {
    on_visibility?.(event.event_key, parent_visible && open);
    return () => on_visibility?.(event.event_key, false);
  }, [event.event_key, open, parent_visible, on_visibility]);
  useEffect(() => {
    if (selected || running || event.is_error || reveal) setExpanded(true);
  }, [selected, running, event.is_error, reveal]);
  useEffect(() => {
    if (parent_visible && open && !page?.has_loaded && !page?.is_loading && !page?.error) on_load?.(event.event_key);
  }, [parent_visible, open, page, on_load, event.event_key]);
  const contents = <>
    {page?.events.length ? children(page.events) : open && !page?.error
      ? <p role="status">{page?.has_loaded ? "No activity in this group." : "Loading activity…"}</p> : null}
    {page?.error ? <div role="alert">{page.error}<button type="button" className="text-button"
      onClick={() => on_retry?.(event.event_key)}>Try again</button></div> : null}
  </>;
  if (inline) return contents;
  return <div className="activity-group" role="listitem">
    <button className="activity-group__toggle" aria-expanded={expanded} aria-controls={id}
      onClick={() => setExpanded((current) => !current)} type="button">
      <ActivityIcon /><span>{event.summary}
        {running ? <span className="activity-group__running"> · Running</span> : null}
        {event.is_error ? <span className="activity-group__failures"> · Failed activity</span> : null}
      </span>
      <ChevronIcon className={expanded ? "chevron chevron--open" : "chevron"} />
    </button>
    <div id={id} hidden={!expanded} className="activity-group__events" role="list" aria-label={event.summary}>
      {contents}
    </div>
  </div>;
}
