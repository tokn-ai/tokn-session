import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushReadingPositions, loadReadingWindow, readReadingPosition, saveReadingPosition, type ReadingPosition } from "./readingPosition";
import { RemoteClient, selectMachine } from "./transport";
import type { EventPageResponse, EventSummary } from "./types";

const position: ReadingPosition = {
  anchors: [{ slot_key: "event.v1.1", type: "message", timestamp: null, top: -50 }],
  last_event: "last", at_end: false,
};
const event = (slot_key: string) => ({ event_key: `window.new.${slot_key}`, slot_key, type: "message", timestamp: null } as EventSummary);
const page = (events: EventSummary[], previous_cursor: string | null = null): EventPageResponse => ({
  events, previous_cursor, next_cursor: null, total_events: events.length, history_status: "complete", attention_revision: "4",
});

beforeEach(() => { localStorage.clear(); selectMachine(); });
afterEach(flushReadingPositions);

describe("reading positions", () => {
  it("persists positions separately by machine and session", () => {
    selectMachine(new RemoteClient("https://first.test", "secret"));
    saveReadingPosition("one", position);
    flushReadingPositions();
    expect(readReadingPosition("one")).toEqual(position);
    expect(readReadingPosition("two")).toBeNull();
    selectMachine(new RemoteClient("https://second.test", "secret"));
    expect(readReadingPosition("one")).toBeNull();
    selectMachine(new RemoteClient("https://first.test", "secret"));
    expect(readReadingPosition("one")).toEqual(position);
    selectMachine();
  });

  it("ignores malformed persisted positions", () => {
    localStorage.setItem("tokn.viewer.reading-positions.v1", "null");
    expect(readReadingPosition("one")).toBeNull();
    localStorage.setItem("tokn.viewer.reading-positions.v1", "broken");
    saveReadingPosition("one", position);
    expect(readReadingPosition("one")).toEqual(position);
  });

  it("flushes the latest position to its original machine during scrolling and skips identical writes", () => {
    vi.useFakeTimers();
    const writes = vi.spyOn(Storage.prototype, "setItem");
    const first = new RemoteClient("https://first.test", "secret");
    const second = new RemoteClient("https://second.test", "secret");
    try {
      selectMachine(first);
      saveReadingPosition("one", position);
      const updated = { ...position, anchors: [{ ...position.anchors[0], top: -90 }] };
      saveReadingPosition("one", updated);
      selectMachine(second);
      saveReadingPosition("one", { ...position, at_end: true });
      vi.advanceTimersByTime(250);
      expect(writes).toHaveBeenCalledTimes(1);
      expect(readReadingPosition("one")?.at_end).toBe(true);
      selectMachine(first);
      expect(readReadingPosition("one")).toEqual(updated);
      saveReadingPosition("one", updated);
      vi.advanceTimersByTime(250);
      expect(writes).toHaveBeenCalledTimes(1);
    } finally {
      flushReadingPositions();
      writes.mockRestore();
      vi.useRealTimers();
    }
  });

  it("loads retained older turns until the saved anchor is available across generations", async () => {
    const load = vi.fn().mockResolvedValueOnce(page([event("event.v1.8")], "older"))
      .mockResolvedValueOnce(page([event("event.v1.1"), event("event.v1.8")], "oldest"));
    const response = await loadReadingWindow("one", position, load, () => true);
    expect(response.events[0].slot_key).toBe("event.v1.1");
    expect(load).toHaveBeenCalledTimes(2);
    expect(load).toHaveBeenLastCalledWith({ session_key: "one", window_mode: "earlier", direction: "backward", cursor: "older" });
  });

  it("returns the recent window when a removed bookmark exhausts older history", async () => {
    const latest = page([event("event.v1.8")], "older");
    const load = vi.fn().mockResolvedValueOnce(latest)
      .mockResolvedValueOnce(page([event("event.v1.0"), event("event.v1.8")]));
    expect(await loadReadingWindow("one", position, load, () => true)).toBe(latest);
  });

  it("uses a surviving secondary anchor when the primary was removed", async () => {
    const saved = { ...position, anchors: [...position.anchors,
      { slot_key: "event.v1.2", type: "message", timestamp: null, top: 100 }] };
    const older = page([event("event.v1.2"), event("event.v1.8")]);
    const load = vi.fn().mockResolvedValueOnce(page([event("event.v1.8")], "older"))
      .mockResolvedValueOnce(older);
    expect(await loadReadingWindow("one", saved, load, () => true)).toBe(older);
  });

  it("stops restoring when selection changes or the server repeats a cursor", async () => {
    const load = vi.fn().mockResolvedValue(page([event("event.v1.8")], "older"));
    await loadReadingWindow("one", position, load, () => false);
    expect(load).toHaveBeenCalledTimes(1);
    load.mockClear();
    await loadReadingWindow("one", position, load, () => true);
    expect(load).toHaveBeenCalledTimes(2);
  });
});
