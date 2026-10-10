import { expect, it } from "vitest";
import { SessionResources } from "./sessionResources";
import type { EventDetail } from "./types";

it("tracks mixed resource coverage and keeps inspection separate from display payloads", () => {
  const cache = new SessionResources();
  const display = { event_key: "tool", event: { output: "display" }, native: null, is_hidden: false, tool_output: null } as EventDetail;
  const inspect = { ...display, native: { original: "source" } };
  expect(cache.coverage("session", "group", "collapsed").status).toBe("missing");
  cache.set("session", "group", "opened", { status: "complete" });
  cache.set("session", "tool", "tool", { status: "loading" });
  expect(cache.coverage("session", "tool", "tool").status).toBe("loading");
  cache.set("session", "tool", "tool", { status: "complete", generation: "source", revision: "3" }, display);
  expect(cache.detail("session", "inspect", "tool")).toBeNull();
  cache.set("session", "inspect", "tool", { status: "complete" }, inspect);
  expect(cache.detail("session", "tool", "tool")?.native).toBeNull();
  expect(cache.detail("session", "inspect", "tool")?.native).toEqual({ original: "source" });
  cache.invalidate("session", "tool");
  expect(cache.detail("session", "tool", "tool")).toBeNull();
  expect(cache.coverage("session", "inspect", "tool").status).toBe("stale");
  expect(cache.coverage("session", "group", "opened").status).toBe("complete");
  cache.set("session", "tool", "tool", { status: "failed", error: "read failed" });
  expect(cache.coverage("session", "tool", "tool").error).toBe("read failed");
  cache.remove("session");
  expect(cache.coverage("session", "group", "opened").status).toBe("missing");
});
