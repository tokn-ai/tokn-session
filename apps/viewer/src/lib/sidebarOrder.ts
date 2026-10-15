import type { SessionOrder, SessionSummary } from "./types";
import { groupSessions } from "./state";
import { readViewerPreferences, updateViewerPreferences } from "./viewerPreferences";

export const RECENT_WINDOW_MS = 60 * 60 * 1000;
const DAY_MS = 24 * RECENT_WINDOW_MS;

export interface SessionGroup {
  key: string;
  project: string;
  sessions: SessionSummary[];
}

export interface RecentEntry {
  session_key: string;
  activity_ms: number;
  /** The timestamp when this session took its place; live updates don't move it. */
  entered_at_ms: number;
}

export function activityTime(session: SessionSummary): number | null {
  if (session.updated_at_ms !== null && Number.isFinite(session.updated_at_ms)) return session.updated_at_ms;
  const timestamp = session.timestamp?.trim();
  if (!timestamp) return null;
  const numeric = Number(timestamp);
  const parsed = Number.isFinite(numeric)
    ? numeric * (Math.abs(numeric) < 100_000_000_000 ? 1000 : 1)
    : Date.parse(timestamp);
  return Number.isFinite(parsed) ? parsed : null;
}

export function compareActivity(left: SessionSummary, right: SessionSummary): number {
  return (activityTime(right) ?? -Infinity) - (activityTime(left) ?? -Infinity)
    || left.session_key.localeCompare(right.session_key);
}

export function projectKey(session: SessionSummary): string {
  return session.project_key?.trim() || session.cwd?.trim() || session.project?.trim() || "";
}

export function compareProjects(left: SessionSummary, right: SessionSummary): number {
  const left_key = projectKey(left);
  const right_key = projectKey(right);
  return Number(!left_key) - Number(!right_key)
    || (right.project_order_ms ?? -Infinity) - (left.project_order_ms ?? -Infinity)
    || left_key.localeCompare(right_key)
    || compareActivity(left, right);
}

/** Freeze known recent rows, prepend activity arrivals, and append older page loads. */
export function reconcileRecent(previous: RecentEntry[], sessions: SessionSummary[], now: number): RecentEntry[] {
  const incoming = new Map(sessions.map((session) => [session.session_key, activityTime(session)]));
  const cutoff = now - RECENT_WINDOW_MS;
  const retained = previous.map((entry) => incoming.has(entry.session_key)
    ? { ...entry, activity_ms: incoming.get(entry.session_key) ?? -Infinity }
    : entry).filter((entry) => entry.activity_ms > cutoff);
  const known = new Set(retained.map((entry) => entry.session_key));
  const added = sessions.filter((session) => !known.has(session.session_key) && (activityTime(session) ?? -Infinity) > cutoff)
    .sort(compareActivity).map((session) => ({
      session_key: session.session_key,
      activity_ms: activityTime(session)!,
      entered_at_ms: activityTime(session)!,
    }));
  const oldest_known = retained.reduce((oldest, entry) => Math.min(oldest, entry.entered_at_ms), Infinity);
  return [
    ...added.filter((entry) => entry.activity_ms >= oldest_known),
    ...retained,
    ...added.filter((entry) => entry.activity_ms < oldest_known),
  ];
}

export function sidebarGroups(sessions: SessionSummary[], order: SessionOrder, recent: RecentEntry[], now: number): SessionGroup[] {
  if (order === "project") return groupSessions([...sessions].sort(compareProjects));
  const today = new Date(now);
  today.setHours(0, 0, 0, 0);
  const yesterday = new Date(today);
  yesterday.setDate(yesterday.getDate() - 1);
  const groups: SessionGroup[] = [
    { key: "recent", project: "Recent · last hour", sessions: [] },
    { key: "today", project: "Today", sessions: [] },
    { key: "yesterday", project: "Yesterday", sessions: [] },
    { key: "week", project: "Past 7 days", sessions: [] },
    { key: "older", project: "Older", sessions: [] },
    { key: "unknown", project: "Unknown time", sessions: [] },
  ];
  const ranks = new Map(recent.map((entry, index) => [entry.session_key, index]));
  for (const session of [...sessions].sort(compareActivity)) {
    const time = activityTime(session);
    const bucket = time === null ? 5
      : time > now - RECENT_WINDOW_MS ? 0
        : time >= today.getTime() ? 1
          : time >= yesterday.getTime() ? 2
            : time >= now - 7 * DAY_MS ? 3 : 4;
    groups[bucket].sessions.push(session);
  }
  groups[0].sessions.sort((left, right) => (ranks.get(left.session_key) ?? Infinity) - (ranks.get(right.session_key) ?? Infinity) || compareActivity(left, right));
  return groups.filter((group) => group.sessions.length > 0);
}

export function readSessionOrder(): SessionOrder {
  return readViewerPreferences().sidebar_order;
}

export function saveSessionOrder(order: SessionOrder) {
  updateViewerPreferences({ sidebar_order: order });
}
