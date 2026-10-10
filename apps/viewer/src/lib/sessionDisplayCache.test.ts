import { describe, expect, it } from "vitest";
import { SessionDisplayCache } from "./sessionDisplayCache";
import type { EventSummary, SessionUpdate } from "./types";

function summary(id: string, text = id): EventSummary {
  return { event_key: id, type: "message", provider: "codex", timestamp: null, phase: "finished", role: "assistant", title: "Assistant", summary: text, summary_truncated: false, is_hidden: false, is_bookkeeping: false, is_error: null, tool: null, usage: null, reasoning: null, trajectory: null, agent_activity: null, compaction: null };
}
function snapshot(cache: SessionDisplayCache, key: string, ids = ["a"]): SessionUpdate {
  const request = cache.request(key);
  return { ...request, generation: "source", revision: "1", base_revision: null, snapshot: true,
    items: ids.map((id) => ({ item_id: id, kind: "assistant_message", level: "final", summary: summary(id) })), groups: [], item_order: ids,
    removed_items: [], state: { total_events: ids.length, history_status: "complete", previous_cursor: null, next_cursor: null, outstanding_questions: [] } };
}

describe("session display replicas", () => {
  it("applies changed items while retaining unchanged references", () => {
    const cache = new SessionDisplayCache(); const initial = snapshot(cache, "one", ["a", "b"]);
    const first = cache.apply(initial)!;
    const next = cache.apply({ ...initial, snapshot: false, base_revision: "1", revision: "2", item_order: null,
      items: [{ ...initial.items[1], summary: summary("b", "updated") }] })!;
    expect(next.events[0]).toBe(first.events[0]); expect(next.events[1].summary).toBe("updated");
  });

  it("requires a snapshot after a revision gap and ignores foreign subscriptions", () => {
    const cache = new SessionDisplayCache(); const initial = snapshot(cache, "one"); cache.apply(initial);
    expect(cache.apply({ ...initial, subscription_id: "other" })).toBeNull();
    expect(cache.get("one")).not.toBeNull();
    expect(cache.apply({ ...initial, snapshot: false, base_revision: "missing", revision: "3" })).toBeNull();
    expect(cache.get("one")).toBeNull(); expect(cache.apply({ ...initial, revision: "4" })).not.toBeNull();
  });

  it("buffers updates that arrive before the initial snapshot response", () => {
    const cache = new SessionDisplayCache(); const initial = snapshot(cache, "one");
    cache.apply({ ...initial, snapshot: false, base_revision: "1", revision: "2", item_order: null,
      items: [{ ...initial.items[0], summary: summary("a", "newest") }] });
    expect(cache.apply(initial)?.events[0].summary).toBe("newest");
  });

  it("does not regress a newer push when an older request completes", () => {
    const cache = new SessionDisplayCache(); const initial = snapshot(cache, "one"); cache.apply(initial);
    cache.apply({ ...initial, snapshot: false, base_revision: "1", revision: "2", item_order: null,
      items: [{ ...initial.items[0], summary: summary("a", "newest") }] });
    expect(cache.apply(initial)?.events[0].summary).toBe("newest");
  });

  it("keeps final coverage separate from all coverage", () => {
    const cache = new SessionDisplayCache(); const initial = snapshot(cache, "one"); cache.apply(initial);
    const final = cache.request("one", "final");
    cache.apply({ ...initial, ...final, revision: "9", items: [], item_order: [] });
    expect(cache.request("one").cursor).toBe("1"); expect(cache.request("one", "final").cursor).toBe("9");
  });

  it("preserves references when a catch-up snapshot contains identical messages", () => {
    const cache = new SessionDisplayCache(); const initial = snapshot(cache, "one"); const first = cache.apply(initial)!;
    const next = cache.apply({ ...initial, items: [{ ...initial.items[0], summary: { ...initial.items[0].summary! } }] })!;
    expect(next.events[0]).toBe(first.events[0]);
  });

  it("evicts recent sessions by user access rather than background activity", () => {
    const cache = new SessionDisplayCache(); cache.select("selected"); cache.apply(snapshot(cache, "selected"));
    for (let i = 0; i < 7; i++) cache.apply(snapshot(cache, `session-${i}`));
    const old = snapshot(cache, "session-0"); cache.apply({ ...old, revision: "2" });
    cache.apply(snapshot(cache, "new"));
    expect(cache.get("selected")).not.toBeNull(); expect(cache.get("session-0")).toBeNull(); expect(cache.get("new")).not.toBeNull();
  });

  it("can drop a subscription while retaining display content", () => {
    const cache = new SessionDisplayCache(); const initial = snapshot(cache, "one"); cache.apply(initial);
    const release = cache.release("one", "all"); expect(release?.unsubscribe).toBe(true);
    expect(cache.accepts(initial)).toBe(false); expect(cache.get("one")).not.toBeNull();
    expect(cache.request("one").subscription_id).not.toBe(initial.subscription_id);
  });
  it("compares committed snapshots correctly across coalesced pushes", () => {
    const cache = new SessionDisplayCache(); const initial = snapshot(cache, "one", ["a", "b"]);
    const first = cache.apply(initial)!; cache.commit("one", first);
    const second = cache.apply({ ...initial, snapshot: false, base_revision: "1", revision: "2", item_order: null,
      items: [{ ...initial.items[0], summary: summary("a", "updated a") }] })!;
    const third = cache.apply({ ...initial, snapshot: false, base_revision: "2", revision: "3", item_order: null,
      items: [{ ...initial.items[1], summary: summary("b", "updated b") }] })!;
    expect([...cache.commit("one", second)!]).toEqual(["a"]);
    expect([...cache.commit("one", third)!]).toEqual(["b"]);
  });

  it("retains full source records separately from display groups and invalidates detail-only changes", () => {
    const cache = new SessionDisplayCache();
    const initial = snapshot(cache, "one");
    const detail = { event_key: "a", event: { output: "first" }, native: null, is_hidden: false, tool_output: null };
    initial.items.push({ item_id: "detail:a", kind: "detail", level: "details", event_key: "a", detail },
      { item_id: "event:result", kind: "event", level: "all", event_key: "result", event: { ...detail, event_key: "result", event: { type: "tool_result", text: "full output" } } });
    initial.event_order = ["event:result"];
    const first = cache.apply(initial)!; cache.commit("one", first);
    expect(first.events).toHaveLength(1);
    expect(cache.sourceEvents("one")[0].event).toEqual({ type: "tool_result", text: "full output" });
    const next = cache.apply({ ...initial, snapshot: false, revision: "2", base_revision: "1", item_order: null, event_order: null,
      items: [{ item_id: "detail:a", kind: "detail", level: "details", event_key: "a", detail: { ...detail, event: { output: "updated" } } }] })!;
    expect(next.events[0]).toBe(first.events[0]);
    expect(cache.detail("one", "a")?.event).toEqual({ output: "updated" });
    expect([...cache.commit("one", next)!]).toEqual(["a"]);
    const removed = cache.apply({ ...initial, snapshot: false, revision: "3", base_revision: "2", item_order: null,
      event_order: [], items: [], removed_items: ["event:result"] })!;
    expect(removed.events).toHaveLength(1);
    expect(cache.sourceEvents("one")).toEqual([]);
  });

  it("pages complete work membership locally and updates children without replacing unchanged rows", () => {
    const cache = new SessionDisplayCache();
    const initial = snapshot(cache, "one", Array.from({ length: 125 }, (_, i) => `child-${i}`));
    const group = { ...summary("work"), type: "trajectory", child_keys: initial.items.map((item) => item.item_id) };
    initial.groups = [{ item_id: "work", kind: "work_summary", level: "steps", summary: group }];
    initial.item_order = ["work"];
    cache.apply(initial);
    const request = { session_key: "one", trajectory_key: "work", limit: 40 };
    const first = cache.trajectoryPage(request)!;
    expect(first.events).toHaveLength(40);
    expect(first.total_events).toBe(125);
    const second = cache.trajectoryPage({ ...request, cursor: first.next_cursor! })!;
    expect(second.events[0].event_key).toBe("child-40");
    expect(cache.trajectoryPage({ ...request, cursor: second.previous_cursor!, direction: "backward" })?.events).toEqual(first.events);
    const tail = cache.trajectoryPage({ ...request, direction: "backward" })!;
    expect(tail.events[0].event_key).toBe("child-85");
    cache.apply({ ...initial, snapshot: false, revision: "2", base_revision: "1", item_order: null, groups: [],
      items: [{ ...initial.items[0], summary: summary("child-0", "finished") }] });
    const updated = cache.trajectoryPage(request)!;
    expect(updated.events[0].summary).toBe("finished");
    expect(updated.events[1]).toBe(first.events[1]);
    cache.apply({ ...initial, generation: "replacement", revision: "3" });
    expect(() => cache.trajectoryPage({ ...request, cursor: first.next_cursor! })).toThrow("no longer current");
  });

  it("falls back for legacy groups and requires recovery for incomplete membership", () => {
    const cache = new SessionDisplayCache();
    const initial = snapshot(cache, "one"); cache.apply(initial);
    expect(cache.trajectoryPage({ session_key: "one", trajectory_key: "a" })).toBeNull();
    expect(cache.apply({ ...initial, groups: [{ item_id: "work", kind: "work_summary", level: "steps",
      summary: { ...summary("work"), child_keys: ["missing"] } }], item_order: ["work"] })).toBeNull();
    expect(cache.get("one")).toBeNull();
  });

});
