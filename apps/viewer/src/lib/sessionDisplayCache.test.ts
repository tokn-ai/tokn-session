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

  it("loads complete work membership locally and updates children without replacing unchanged rows", () => {
    const cache = new SessionDisplayCache();
    const initial = snapshot(cache, "one", Array.from({ length: 125 }, (_, i) => `child-${i}`));
    const group = { ...summary("work"), type: "trajectory", child_keys: initial.items.map((item) => item.item_id) };
    initial.groups = [{ item_id: "work", kind: "work_summary", level: "steps", summary: group }];
    initial.item_order = ["work"];
    cache.apply(initial);
    const first = cache.trajectoryGroup("one", "work")!;
    expect(first.events).toHaveLength(125);
    expect(first.total_events).toBe(125);
    expect(first.previous_cursor).toBeNull();
    expect(first.next_cursor).toBeNull();
    cache.apply({ ...initial, snapshot: false, revision: "2", base_revision: "1", item_order: null, groups: [],
      items: [{ ...initial.items[0], summary: summary("child-0", "finished") }] });
    const updated = cache.trajectoryGroup("one", "work")!;
    expect(updated.events[0].summary).toBe("finished");
    expect(updated.events[1]).toBe(first.events[1]);
    cache.apply({ ...initial, snapshot: false, revision: "3", base_revision: "2", item_order: null,
      items: [{ item_id: "child-125", kind: "tool_summary", level: "steps", summary: summary("child-125") }],
      groups: [{ ...initial.groups[0], summary: { ...group, child_keys: [...group.child_keys, "child-125"] } }] });
    const appended = cache.trajectoryGroup("one", "work")!;
    expect(appended.events).toHaveLength(126);
    expect(appended.events[125].event_key).toBe("child-125");
    expect(appended.events[1]).toBe(first.events[1]);
    cache.apply({ ...initial, generation: "replacement", revision: "4" });
    expect(cache.trajectoryGroup("one", "work")?.events).toHaveLength(125);
  });

  it("falls back for legacy groups and requires recovery for incomplete membership", () => {
    const cache = new SessionDisplayCache();
    const initial = snapshot(cache, "one"); cache.apply(initial);
    expect(cache.trajectoryGroup("one", "a")).toBeNull();
    expect(cache.apply({ ...initial, groups: [{ item_id: "work", kind: "work_summary", level: "steps",
      summary: { ...summary("work"), child_keys: ["missing"] } }], item_order: ["work"] })).toBeNull();
    expect(cache.get("one")).toBeNull();
  });

  it("loads scoped groups atomically and keeps other groups deferred across live appends", () => {
    const cache = new SessionDisplayCache();
    const request = cache.request("one", "steps");
    expect(request.scope).toEqual({ history: "latest_turn", group_keys: [] });
    const a = { ...summary("activity:a"), type: "activity_group", child_keys: ["a", "b"] };
    const c = { ...summary("activity:c"), type: "activity_group", child_keys: ["c"] };
    const work = { ...summary("work"), type: "trajectory", child_keys: [a.event_key, c.event_key] };
    const initial: SessionUpdate = { ...snapshot(cache, "one"), ...request, items: [a, c].map((summary) =>
      ({ item_id: summary.event_key, kind: "work_summary", level: "steps", summary })),
      groups: [{ item_id: "work", kind: "work_summary", level: "steps", summary: work }], item_order: ["work"],
      state: { ...snapshot(cache, "one").state, scope: { history: "latest_turn", turn_key: "user", group_keys: [] } } };
    cache.apply(initial);
    expect(cache.trajectoryGroup("one", "work")?.events).toEqual([a, c]);
    expect(cache.trajectoryGroup("one", a.event_key)).toBeNull();
    cache.includeGroup("one", a.event_key);
    expect(cache.request("one", "steps").scope).toEqual({ history: "latest_turn", turn_key: "user", group_keys: [a.event_key] });
    expect(cache.request("one", "details", ["a"]).scope?.group_keys).toEqual([]);
    const update = { ...initial, snapshot: false, base_revision: "1", revision: "2", groups: [], item_order: null,
      items: ["a", "b"].map((id) => ({ item_id: id, kind: "tool_summary" as const, level: "steps" as const, summary: summary(id) })) };
    cache.apply(update);
    const complete = cache.trajectoryGroup("one", a.event_key)!;
    expect(complete.events.map((event) => event.event_key)).toEqual(["a", "b"]);
    expect(cache.trajectoryGroup("one", c.event_key)).toBeNull();
    cache.apply({ ...update, base_revision: "2", revision: "3", items: [
      { item_id: a.event_key, kind: "work_summary", level: "steps", summary: { ...a, child_keys: ["a", "b", "d"] } },
      { item_id: "d", kind: "tool_summary", level: "steps", summary: summary("d") } ] });
    const appended = cache.trajectoryGroup("one", a.event_key)!;
    expect(appended.events.map((event) => event.event_key)).toEqual(["a", "b", "d"]);
    expect(appended.events[0]).toBe(complete.events[0]);
    cache.includeHistory("one");
    expect(cache.request("one", "steps").scope?.history).toBe("retained");
  });

});
