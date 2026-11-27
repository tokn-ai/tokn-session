import { describe, expect, it } from "vitest";
import type { EventSummary } from "./types";
import { isBookkeepingEvent } from "./eventFilter";

function event(overrides: Partial<EventSummary>): EventSummary {
  return {
    event_key: "event", type: "lifecycle", provider: "codex", timestamp: null,
    phase: null, role: null, title: "Turn started", summary: "Turn started",
    summary_truncated: false, is_hidden: false, is_error: false,
    tool: null, usage: null, reasoning: null, ...overrides,
  };
}

describe("bookkeeping filter classification", () => {
  it.each(["session_started", "provider_changed", "session_settings_applied", "lifecycle", "metadata", "usage", "activity_group"])(
    "requires explicit backend classification for %s", (type) => {
      expect(isBookkeepingEvent(event({ type, is_bookkeeping: true }))).toBe(true);
      expect(isBookkeepingEvent(event({ type, is_bookkeeping: false }))).toBe(false);
      expect(isBookkeepingEvent(event({ type }))).toBe(false);
    },
  );

  it.each(["error", "unknown", "message", "reasoning", "tool_call", "agent_activity", "compaction", "goal_updated", "trajectory", "future_provider_event"])(
    "always preserves %s even when a server sets the routine flag", (type) => {
      expect(isBookkeepingEvent(event({ type, is_bookkeeping: true }))).toBe(false);
    },
  );

  it.each(["lifecycle", "metadata", "usage", "activity_group"])("preserves reported errors in %s", (type) => {
    expect(isBookkeepingEvent(event({ type, is_bookkeeping: true, is_error: true }))).toBe(false);
  });
});
