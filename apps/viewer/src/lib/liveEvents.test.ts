import { describe, expect, it, vi } from "vitest";
import { refreshEventWindow, loadCompleteTrajectory } from "./liveEvents";
import type { EventPageResponse, EventSummary, TrajectoryEventPageResponse } from "./types";

const event = (event_key: string) => ({ event_key, summary: event_key }) as EventSummary;
const page = (keys: string[], previous_cursor: string | null = null, next_cursor: string | null = null): TrajectoryEventPageResponse => ({
  events: keys.map(event), previous_cursor, next_cursor, total_events: 6,
});
const request = { session_key: "s", trajectory_key: "t", direction: "backward" as const, limit: 40 };

describe("complete work loading", () => {
  it("refreshes the retained history independently of child loading", async () => {
    const response: EventPageResponse = { ...page(["a"]), history_status: "complete" };
    const load = vi.fn().mockResolvedValue(response);
    expect(await refreshEventWindow("session", load)).toBe(response);
    expect(load).toHaveBeenCalledExactlyOnceWith({ session_key: "session", window_mode: "retained", direction: "backward" });
  });

  it("assembles every transport page before resolving, including both sides", async () => {
    const load = vi.fn().mockResolvedValueOnce(page(["c", "d"], "older", "newer"))
      .mockResolvedValueOnce(page(["a", "b"]))
      .mockResolvedValueOnce(page(["e", "f"]));
    const result = await loadCompleteTrajectory(request, load, () => true);
    expect(result.events.map((event) => event.event_key)).toEqual(["a", "b", "c", "d", "e", "f"]);
    expect(result.previous_cursor).toBeNull();
    expect(result.next_cursor).toBeNull();
    expect(load).toHaveBeenNthCalledWith(2, { ...request, cursor: "older", direction: "backward" });
    expect(load).toHaveBeenNthCalledWith(3, { ...request, cursor: "newer", direction: "forward" });
  });

  it("rejects failed, repeated, or overlapping pages rather than publishing a partial group", async () => {
    const failed = vi.fn().mockResolvedValueOnce(page(["b"], "older")).mockRejectedValueOnce(new Error("unavailable"));
    await expect(loadCompleteTrajectory(request, failed, () => true)).rejects.toThrow("unavailable");
    const repeated = vi.fn().mockResolvedValue(page(["b"], "older"));
    await expect(loadCompleteTrajectory(request, repeated, () => true)).rejects.toThrow("repeated cursor");
    const overlap = vi.fn().mockResolvedValueOnce(page(["b"], "older")).mockResolvedValueOnce(page(["b"]));
    await expect(loadCompleteTrajectory(request, overlap, () => true)).rejects.toThrow("changed while loading");
  });

  it("stops obsolete loads without treating them as complete", async () => {
    const load = vi.fn().mockResolvedValue(page(["b"], "older"));
    await expect(loadCompleteTrajectory(request, load, () => false)).rejects.toThrow("superseded");
    expect(load).toHaveBeenCalledOnce();
  });
});
