import { describe, expect, it } from "vitest";
import type { SessionSummary } from "./types";
import {
  findKnownSession,
  groupSessions,
  knownSessionAncestors,
  preserveSessionSelection,
  providerLabel,
  sessionDisplayTitle,
  shortSessionId,
} from "./state";

function session(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    session_key: "key",
    session_id: "01991dce-7f6a-7000-8000-000000000001",
    parent_session_id: null,
    is_subagent: false,
    provider: "codex",
    title: null,
    preview: null,
    project: null,
    cwd: null,
    updated_at_ms: null,
    timestamp: null,
    agent_path: null,
    agent_nickname: null,
    agent_role: null,
    child_count: 0,
    message_count: null,
    event_count: null,
    history_status: null,
    has_unread: false,
    ...overrides,
  };
}

describe("session identity", () => {
  it.each(["project_key", "cwd"] as const)("preserves significant trailing spaces in %s when grouping", (field) => {
    const plain = session({ session_key: "plain", project: "  Repo  ", [field]: "/repo" });
    const spaced = session({ session_key: "spaced", project: "  Repo  ", [field]: "/repo " });
    expect(groupSessions([plain, spaced])).toEqual([
      { key: "/repo", project: "Repo", sessions: [plain] },
      { key: "/repo ", project: "Repo", sessions: [spaced] },
    ]);
  });

  it("ignores blank project identities while preserving the exact cwd fallback", () => {
    const entry = session({ project_key: " \t", cwd: "/repo ", project: "Repo" });
    expect(groupSessions([entry])).toEqual([{ key: "/repo ", project: "Repo", sessions: [entry] }]);
  });

  it("labels WorkBuddy sessions by their product name", () => {
    expect(providerLabel("workbuddy")).toBe("WorkBuddy");
  });

  it("prefers a native title over the first-message preview", () => {
    expect(sessionDisplayTitle(session({ title: "Named task", preview: "Raw prompt" })))
      .toBe("Named task");
  });

  it("uses a meaningful preview and then a neutral fallback", () => {
    expect(sessionDisplayTitle(session({ preview: "  Build the viewer  " })))
      .toBe("Build the viewer");
    expect(sessionDisplayTitle(session({ title: "  ", preview: "\n\t" })))
      .toBe("Untitled session");
  });

  it("uses an agent label for an otherwise untitled child", () => {
    expect(sessionDisplayTitle(session({
      parent_session_id: "parent",
      is_subagent: true,
      agent_nickname: "Hubble",
    }))).toBe("Hubble");
  });

  it("keeps short ids intact and abbreviates long ids", () => {
    expect(shortSessionId("session-12")).toBe("session-12");
    expect(shortSessionId("01991dce-7f6a-7000-8000-000000000001"))
      .toBe("01991dce…");
  });

  it("finds a loaded child and its ancestor path without relying on raw IDs", () => {
    const root = session({ session_key: "codex:root", session_id: "same-id", child_count: 1 });
    const child = session({
      session_key: "codex:child",
      session_id: "same-id",
      parent_session_id: root.session_id,
      is_subagent: true,
      child_count: 1,
    });
    const grandchild = session({
      session_key: "codex:grandchild",
      session_id: "grandchild",
      parent_session_id: child.session_id,
      is_subagent: true,
    });
    const children = new Map([
      [root.session_key, {
        sessions: [child],
        next_cursor: null,
        is_loading: false,
        is_loading_more: false,
        error: null,
      }],
      [child.session_key, {
        sessions: [grandchild],
        next_cursor: null,
        is_loading: false,
        is_loading_more: false,
        error: null,
      }],
    ]);

    expect(findKnownSession([root], children, grandchild.session_key)).toBe(grandchild);
    expect(knownSessionAncestors([root], children, grandchild.session_key))
      .toEqual([root.session_key, child.session_key]);
  });

  it("keeps an explicit selection when a root catalog refresh omits it", () => {
    expect(preserveSessionSelection("codex:child")).toBe("codex:child");
    expect(preserveSessionSelection(null)).toBeNull();
  });
});
