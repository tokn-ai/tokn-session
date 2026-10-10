import type { EventSummary, EventPageResponse, LoadEventPageRequest, LoadTrajectoryEventPageRequest, TrajectoryEventPageResponse } from "./types";

/** The backend owns the retained turn window, including earlier loaded turns.
 * A refresh returns that whole window from one snapshot in a single request.
 */
export function refreshEventWindow(
  session_key: string,
  load: (request: LoadEventPageRequest) => Promise<EventPageResponse>,
): Promise<EventPageResponse> {
  return load({ session_key, window_mode: "retained", direction: "backward" });
}

/** Legacy transport pages are assembled atomically. A page boundary is never a
 * display boundary: commentary and complete inner groups must arrive together.
 */
export async function loadCompleteTrajectory(
  request: LoadTrajectoryEventPageRequest,
  load: (request: LoadTrajectoryEventPageRequest) => Promise<TrajectoryEventPageResponse>,
  current: () => boolean,
): Promise<TrajectoryEventPageResponse> {
  const initial = await load(request);
  const expected_total = initial.total_events;
  const before: EventSummary[][] = [];
  const after: EventSummary[][] = [];
  const cursors = new Set<string>();
  for (const direction of ["backward", "forward"] as const) {
    let cursor = direction === "backward" ? initial.previous_cursor : initial.next_cursor;
    while (current() && cursor) {
      if (cursors.has(cursor)) throw new Error("Work group load returned a repeated cursor");
      cursors.add(cursor);
      const more = await load({ ...request, cursor, direction });
      if (more.total_events !== expected_total) throw new Error("Work group changed while loading; try again");
      (direction === "backward" ? before : after).push(more.events);
      cursor = direction === "backward" ? more.previous_cursor : more.next_cursor;
    }
  }
  if (!current()) throw new Error("Work group load was superseded");
  // Flatten once, rather than repeatedly copying all previously loaded rows.
  const events = [...before.reverse().flat(), ...initial.events, ...after.flat()];
  const keys = new Set(events.map((event) => event.event_key));
  if (keys.size !== events.length || events.length !== expected_total) {
    throw new Error("Work group changed while loading; try again");
  }
  return { events, total_events: events.length, previous_cursor: null, next_cursor: null };
}
