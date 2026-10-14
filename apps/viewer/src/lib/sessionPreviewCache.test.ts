import { describe, expect, it } from "vitest";
import { SessionPreviewCache } from "./sessionPreviewCache";
import type { BackgroundSessionUpdate } from "./types";

function update(revision = "1", session_key = "main"): BackgroundSessionUpdate {
  return { subscription_id: "subscription", generation: "generation", revision, session_key,
    message_revision: `message:${revision}`, latest_assistant_message: { message_id: "message", delivery: "commentary", phase: "running", preview: "Latest line.\nDetails" },
    has_unread: false, unread_final_count: 0, is_running: true, question_attention: { required_count: 0, available_count: 0 } };
}

describe("compact session previews", () => {
  it("bounds first lines independently of rich replicas and keeps child ownership", () => {
    const cache = new SessionPreviewCache(2); cache.begin("subscription");
    cache.apply({ ...update(), latest_assistant_message: { ...update().latest_assistant_message!, preview: "界".repeat(300) } });
    cache.apply({ ...update("2", "child"), latest_assistant_message: { ...update().latest_assistant_message!, preview: "Child output" } });
    expect([...cache.preview("main")!]).toHaveLength(240);
    expect(cache.preview("child")).toBe("Child output");
    cache.apply(update("3", "other"));
    expect(cache.preview("main")).toBeUndefined();
    expect(cache.preview("child")).toBe("Child output");
  });

  it("rejects stale identities, revisions and server generations, releasing activity authority on gaps", () => {
    const cache = new SessionPreviewCache(); cache.begin("subscription");
    expect(cache.apply(update())).toBe(true);
    expect(cache.apply(update())).toBe(false);
    expect(cache.apply({ ...update("2"), subscription_id: "old" })).toBe(false);
    expect(cache.apply({ ...update("2"), generation: "old" })).toBe(false);
    cache.apply(update("3", "other"));
    expect(cache.preview("main")).toBe("Latest line.");
    expect(cache.activity("main")).toBeUndefined();
    expect(cache.preview("other")).toBe("Latest line.");
    cache.begin("new");
    expect(cache.preview("other")).toBe("Latest line.");
    expect(cache.activity("other")).toBeUndefined();
    expect(cache.apply(update("4"))).toBe(false);
  });

  it("preserves an existing body for unknown projection but clears an authoritative null", () => {
    const cache = new SessionPreviewCache(); cache.begin("subscription"); cache.apply(update());
    cache.apply({ ...update("2"), message_revision: null, latest_assistant_message: null, is_running: false });
    expect(cache.preview("main")).toBe("Latest line.");
    expect(cache.activity("main")?.is_running).toBe(false);
    cache.apply({ ...update("3"), latest_assistant_message: null });
    expect(cache.preview("main")).toBeNull();
    cache.apply({ ...update("4"), removed: true });
    expect(cache.preview("main")).toBeNull();
    expect(cache.activity("main")).toBeUndefined();
    expect(cache.isRemoved("main")).toBe(true);
    expect(cache.rememberPage("main", "Stale retained reply", cache.capture("main"))).toBe(false);
    cache.begin("replacement");
    expect(cache.isRemoved("main")).toBe(true);
  });

  it("keeps a live message that overtakes an opening read and does not rewind an early event with its ack", () => {
    const cache = new SessionPreviewCache(); cache.begin("subscription");
    const token = cache.capture("main");
    cache.apply(update());
    cache.acknowledge({ subscription_id: "subscription", generation: "generation", revision: "0", scope: "all", subscribed: true });
    expect(cache.rememberPage("main", "Older page", token)).toBe(false);
    expect(cache.apply(update())).toBe(false);
    expect(cache.preview("main")).toBe("Latest line.");
    const current = cache.capture("main");
    expect(cache.rememberPage("main", "New accepted page", current)).toBe(true);
    cache.begin("new");
    expect(cache.rememberPage("main", "Stale page", current)).toBe(false);
  });
});
