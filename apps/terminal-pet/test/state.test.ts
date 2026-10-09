import { describe, expect, test } from "bun:test";

import { PetStore } from "../src/state";
import { relayEvent } from "./fixtures";

const policy = {
  ready_debounce_ms: 10,
  ready_hold_ms: 50,
  recent_completion_ms: 500,
  error_grace_ms: 20,
  running_lease_ms: 100,
  open_tool_lease_ms: 200,
  blocked_lease_ms: 300,
  input_lease_ms: 400
};

describe("PetStore", () => {
  test("normalized blocking questions retain input attention while async questions do not imply completion", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({ type: "question_request", request_id: "call-1", is_blocking: false, phase: "finished" }), 0);
    expect(store.snapshot(20).state).toBe("running");
    store.ingest(relayEvent({ type: "question_request", request_id: "call-2", is_blocking: true, phase: "finished" }), 30);
    expect(store.snapshot(40).state).toBe("needs_input");
    store.ingest(relayEvent({ type: "tool_call", tool_call_id: "call-2", phase: "finished", output: { answers: {} } }), 50);
    expect(store.snapshot(60).state).toBe("running");
    store.ingest(relayEvent({ type: "question_request", request_id: "call-3", is_blocking: null, phase: "finished" }), 70);
    expect(store.snapshot(80).state).toBe("needs_input");
  });
  test("passive records and hidden messages do not change activity or renew leases", () => {
    const store = new PetStore(policy);
    const passive = [
      { type: "metadata", native_type: "compacted", summary: "context summary" },
      { type: "compaction", state: "started" },
      { type: "compaction", state: "completed", summary: "context summary" },
      { type: "compaction", state: "failed" },
      { type: "usage", kind: "session_snapshot", input_tokens: 100 },
      { type: "message", role: "user", phase: "finished", text: "hidden", provenance: { display: false } },
      { type: "unknown", provider: "pi", native: { type: "custom_message", display: false, content: "hidden" } }
    ];
    for (const event of passive) {
      store.ingest(relayEvent(event), 0);
    }
    expect(store.snapshot(0).sessions).toHaveLength(0);

    store.ingest(relayEvent({ type: "message", role: "user", phase: "finished", text: "real work" }), 0);
    const before = store.snapshot(1);
    for (const event of passive) {
      store.ingest(relayEvent(event), 50);
    }
    expect(store.snapshot(50)).toEqual(before);
    expect(store.snapshot(101).state).toBe("idle");
  });

  test("moves from user work to debounced ready and then idle", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "message",
      role: "user",
      phase: "finished",
      text: "build it"
    }), 0);
    expect(store.snapshot(1).state).toBe("running");

    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "done"
    }), 20);
    expect(store.snapshot(29).state).toBe("running");
    expect(store.snapshot(30).state).toBe("ready");
    const completed = store.snapshot(80);
    expect(completed.state).toBe("idle");
    expect(completed.sessions[0]).toMatchObject({
      completed_at: 30,
      recently_completed: true
    });

    store.ingest(relayEvent({
      type: "message",
      role: "user",
      phase: "finished",
      text: "one more thing"
    }), 90);
    expect(store.snapshot(91).sessions[0]).toMatchObject({
      state: "running",
      recently_completed: false
    });
    expect(store.snapshot(91).sessions[0]?.completed_at).toBeUndefined();
  });

  test("treats finished commentary as progress rather than completion", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      delivery: "commentary",
      phase: "finished",
      text: "still working"
    }), 0);

    expect(store.snapshot(20).state).toBe("running");
  });

  test("does not report ready while a tool remains open", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "tool_call",
      tool_call_id: "call-1",
      tool_name: "exec_command",
      phase: "started",
      input: { cmd: "cargo test" }
    }), 0);
    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "premature"
    }), 10);
    expect(store.snapshot(30).state).toBe("running");

    store.ingest(relayEvent({
      type: "tool_call",
      tool_call_id: "call-1",
      tool_name: "exec_command",
      phase: "finished",
      output: { exit_code: 0 }
    }), 40);
    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "done"
    }), 50);
    expect(store.snapshot(60).state).toBe("ready");
  });

  test("retains structured details for the focused activity", () => {
    const store = new PetStore(policy);
    const tool = relayEvent({
      type: "tool_call",
      tool_call_id: "call-1",
      tool_name: "exec_command",
      phase: "started",
      input: { cmd: "cargo test" },
      summary: { command: "cargo test" }
    });
    tool.session.cwd = "/tmp/tokn-agent";
    store.ingest(tool, 0);

    expect(store.snapshot(1).focus).toMatchObject({
      cwd: "/tmp/tokn-agent",
      current_activity: {
        kind: "tool",
        label: "exec_command: cargo test",
        detail: "cargo test",
        at: 0
      }
    });

    store.ingest(relayEvent({
      type: "unknown",
      native_type: "event_msg.request_user_input",
      native: {
        id: "question-1",
        prompt: "Approve cargo test?"
      }
    }), 10);

    expect(store.snapshot(11).focus).toMatchObject({
      state: "needs_input",
      current_activity: {
        kind: "input",
        detail: "Approve cargo test?",
        at: 10
      }
    });
  });

  test("does not let an older assistant finish override newer progress", () => {
    const store = new PetStore(policy);
    const progress = relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "newer work",
      occurred_at_ms: 100
    });
    store.ingest(progress, 100);
    const staleFinish = relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "older finish",
      occurred_at_ms: 50
    });
    staleFinish.session.agent_nickname = "Merged metadata";
    store.ingest(staleFinish, 110);

    expect(store.snapshot(150).focus).toMatchObject({
      state: "running",
      label: "Thinking",
      agent: "Merged metadata",
      last_event_at: 100
    });
  });

  test("turns a stable error into blocked but cancels it on progress", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "error",
      message: "stream failed"
    }), 0);
    expect(store.snapshot(19).state).toBe("idle");
    expect(store.snapshot(20).state).toBe("blocked");

    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "recovering"
    }), 30);
    expect(store.snapshot(31).state).toBe("running");
  });

  test("treats Codex turn interruption as a recent outcome, not blocked", () => {
    const store = new PetStore(policy);
    const rootInterrupted = relayEvent({
      type: "error",
      provider: "codex",
      message: "interrupted"
    }, "codex.root");
    rootInterrupted.session.session_id = "root";
    store.ingest(rootInterrupted, 10);

    const childInterrupted = relayEvent({
      type: "error",
      provider: "codex",
      message: "interrupted"
    }, "codex.child");
    childInterrupted.session.session_id = "child";
    childInterrupted.session.parent_session_id = "root";
    childInterrupted.session.agent_path = "/root/reviewer";
    store.ingest(childInterrupted, 20);

    const snapshot = store.snapshot(40);
    expect(snapshot.active_sessions).toBe(0);
    expect(snapshot.sessions).toHaveLength(2);
    expect(snapshot.sessions).toEqual([
      expect.objectContaining({
        topic: "codex.root",
        state: "idle",
        family_state: "idle",
        outcome: "interrupted",
        recently_completed: true
      }),
      expect.objectContaining({
        topic: "codex.child",
        state: "idle",
        outcome: "interrupted",
        recently_completed: true
      })
    ]);
  });

  test("prioritizes needs input and clears it when work resumes", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "unknown",
      native_type: "event_msg.request_user_input",
      native: {
        id: "question-1"
      }
    }), 0);
    expect(store.snapshot(1).state).toBe("needs_input");

    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "background progress"
    }), 5);
    expect(store.snapshot(6).state).toBe("needs_input");

    store.acknowledge("codex.session-1");
    expect(store.snapshot(7).state).toBe("needs_input");

    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "still waiting"
    }), 8);
    expect(store.snapshot(9).state).toBe("needs_input");

    store.ingest(relayEvent({
      type: "message",
      role: "user",
      phase: "finished",
      text: "yes"
    }), 10);
    expect(store.snapshot(11).state).toBe("running");
  });

  test("uses official state priority across sessions", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta"
    }, "codex.running"), 0);
    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "ready"
    }, "codex.ready"), 0);
    store.ingest(relayEvent({
      type: "unknown",
      native_type: "event_msg.exec_approval_request",
      native: {
        approval_id: "approval-1"
      }
    }, "codex.waiting"), 0);

    const snapshot = store.snapshot(10);
    expect(snapshot.state).toBe("needs_input");
    expect(snapshot.focus?.topic).toBe("codex.waiting");
    expect(snapshot.active_sessions).toBe(3);
    expect(snapshot.sessions.map((session) => session.topic)).toEqual([
      "codex.waiting",
      "codex.ready",
      "codex.running"
    ]);
  });

  test("creates a provisional child and keeps agent activity off the parent", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "spawn-1",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 0
    }), 0);

    const snapshot = store.snapshot(1);
    expect(snapshot.active_sessions).toBe(1);
    expect(snapshot.focus).toMatchObject({
      topic: "codex.child",
      state: "running",
      parent_topic: "codex.root",
      root_topic: "codex.root",
      depth: 1,
      is_provisional: true
    });
    expect(snapshot.sessions.map((session) => session.topic)).toEqual([
      "codex.root",
      "codex.child"
    ]);
    expect(snapshot.sessions[0]).toMatchObject({
      state: "idle",
      family_state: "running",
      descendant_count: 1,
      active_descendant_count: 1
    });
  });

  test("keeps inferred children inside the emitting root family", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta"
    }, "codex.root-a"), 0);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta"
    }, "codex.root-b"), 0);
    store.ingest(agentActivity("root-b", {
      event_id: "spawn-b",
      kind: "started",
      target_session_id: "child-b",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 10
    }), 10);

    expect(store.snapshot(11).sessions.find(
      (session) => session.topic === "codex.child-b"
    )).toMatchObject({
      parent_topic: "codex.root-b",
      root_topic: "codex.root-b"
    });
  });

  test("does not infer a child under a different provider root", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta"
    }, "pi.root-pi"), 0);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta"
    }, "codex.root-codex"), 0);
    store.ingest(agentActivity("root-codex", {
      event_id: "spawn-codex",
      kind: "started",
      target_session_id: "child-codex",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 10
    }), 10);

    expect(store.snapshot(11).sessions.find(
      (session) => session.topic === "codex.child-codex"
    )).toMatchObject({
      parent_topic: "codex.root-codex",
      root_topic: "codex.root-codex"
    });
  });

  test("reconciles a provisional child with its own Relay session", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "spawn-1",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 0
    }), 0);
    const child = relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "checking"
    }, "codex.child");
    child.session.parent_session_id = "root";
    child.session.agent_path = "/root/researcher";
    child.session.agent_nickname = "Avicenna";
    store.ingest(child, 10);

    const snapshot = store.snapshot(11);
    expect(snapshot.total_sessions).toBe(2);
    expect(snapshot.focus).toMatchObject({
      topic: "codex.child",
      agent: "Avicenna",
      is_provisional: false,
      parent_topic: "codex.root",
      root_topic: "codex.root",
      depth: 1
    });
  });

  test("treats interaction as a target annotation without extending work", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "spawn-1",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 0
    }), 0);
    store.ingest(agentActivity("root", {
      event_id: "interaction-1",
      kind: "interacted",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 90
    }), 90);

    expect(store.snapshot(95).focus).toMatchObject({
      topic: "codex.child",
      state: "running",
      label: "Agent interaction"
    });
    const expired = store.snapshot(101);
    expect(expired.active_sessions).toBe(0);
    expect(expired.sessions).toEqual([]);
    expect(expired.total_sessions).toBe(2);
  });

  test("routes target-less agent messages by an exact known agent path", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "spawn-1",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 0
    }), 0);
    store.ingest(agentActivity("root", {
      event_id: "message-1",
      kind: "messaged",
      actor_agent_path: "/root",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 90
    }), 90);

    expect(store.snapshot(95).focus).toMatchObject({
      topic: "codex.child",
      state: "running",
      label: "Agent messaged",
      last_event_at: 90
    });
    expect(store.snapshot(101).sessions).toEqual([]);
  });

  test("routes target-less activity only within the source family", () => {
    const store = new PetStore(policy);
    const childA = relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "family a"
    }, "codex.child-a");
    childA.session.parent_session_id = "root-a";
    childA.session.agent_path = "/root/researcher";
    store.ingest(childA, 0);
    const childB = relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "family b"
    }, "codex.child-b");
    childB.session.parent_session_id = "root-b";
    childB.session.agent_path = "/root/researcher";
    store.ingest(childB, 0);
    store.ingest(agentActivity("root-b", {
      event_id: "interaction-b",
      kind: "interacted",
      target_agent_path: "/root/researcher",
      actor_agent_path: "/root",
      occurred_at_ms: 10
    }), 10);

    const sessions = store.snapshot(11).sessions;
    expect(sessions.find((session) => session.topic === "codex.child-a")?.label)
      .toBe("Thinking");
    expect(sessions.find((session) => session.topic === "codex.child-b")?.label)
      .toBe("Agent interaction");
  });

  test("routes target-less messages to their source only when the actor matches", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "root work",
      occurred_at_ms: 0
    }, "codex.root"), 0);
    store.ingest(agentActivity("root", {
      event_id: "root-message",
      kind: "messaged",
      actor_agent_path: "/root",
      target_agent_path: "/root/not-known",
      occurred_at_ms: 90
    }), 90);

    expect(store.snapshot(95).focus).toMatchObject({
      topic: "codex.root",
      label: "Agent messaged",
      last_event_at: 90
    });
    expect(store.snapshot(101).sessions).toEqual([]);
  });

  test("ignores copied target-less messages whose actor is not the source", () => {
    const store = new PetStore(policy);
    const childProgress = relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "child work",
      occurred_at_ms: 0
    }, "codex.child");
    childProgress.session.parent_session_id = "root";
    childProgress.session.agent_path = "/root/researcher";
    store.ingest(childProgress, 0);
    const copiedParentMessage = agentActivity("child", {
      event_id: "copied-message",
      kind: "messaged",
      actor_agent_path: "/root",
      target_agent_path: "/root/not-known",
      occurred_at_ms: 50
    });
    copiedParentMessage.session.parent_session_id = "root";
    copiedParentMessage.session.agent_path = "/root/researcher";
    store.ingest(copiedParentMessage, 50);

    expect(store.snapshot(51).focus).toMatchObject({
      topic: "codex.child",
      label: "Thinking",
      last_event_at: 0
    });
    expect(store.snapshot(101).sessions).toEqual([]);
  });

  test("keeps a detached provisional node's parent fields consistent", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "detached-interrupt",
      kind: "interrupted",
      target_session_id: "detached",
      target_agent_path: "/detached/agent",
      occurred_at_ms: 10
    }), 10);

    const detached = store.sessions.get("codex.detached");
    expect(detached?.parent_topic).toBeUndefined();
    expect(detached?.session.parent_session_id).toBeUndefined();
    expect(store.snapshot(11).focus).toMatchObject({
      topic: "codex.detached",
      root_topic: "codex.detached",
      depth: 0,
      outcome: "interrupted"
    });
  });

  test("shows interruption as recent child work without claiming blocked", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "spawn-1",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 0
    }), 0);
    store.ingest(agentActivity("root", {
      event_id: "interrupt-1",
      kind: "interrupted",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 50
    }), 50);

    const snapshot = store.snapshot(51);
    expect(snapshot.active_sessions).toBe(0);
    expect(snapshot.focus).toMatchObject({
      topic: "codex.child",
      state: "idle",
      label: "Interrupted",
      outcome: "interrupted",
      completed_at: 50,
      recently_completed: true
    });
    expect(snapshot.sessions[0]).toMatchObject({
      topic: "codex.root",
      family_state: "idle",
      recent_descendant_count: 1
    });
  });

  test("keeps nested agents in family preorder and bubbles urgency", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "spawn-child",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 0
    }), 0);
    const childStart = relayEvent({
      type: "agent_activity",
      provider: "codex",
      event_id: "spawn-grandchild",
      kind: "started",
      target_session_id: "grandchild",
      target_agent_path: "/root/researcher/reviewer",
      occurred_at_ms: 10
    }, "codex.child");
    childStart.session.parent_session_id = "root";
    childStart.session.agent_path = "/root/researcher";
    store.ingest(childStart, 10);
    const needsInput = relayEvent({
      type: "unknown",
      native_type: "event_msg.request_user_input",
      native: { id: "question-1" }
    }, "codex.grandchild");
    needsInput.session.parent_session_id = "child";
    needsInput.session.agent_path = "/root/researcher/reviewer";
    store.ingest(needsInput, 20);

    const snapshot = store.snapshot(21);
    expect(snapshot.sessions.map((session) => [
      session.topic,
      session.depth
    ])).toEqual([
      ["codex.root", 0],
      ["codex.child", 1],
      ["codex.grandchild", 2]
    ]);
    expect(snapshot.focus).toMatchObject({
      topic: "codex.grandchild",
      state: "needs_input",
      root_topic: "codex.root"
    });
    expect(snapshot.sessions[0]).toMatchObject({
      state: "idle",
      family_state: "needs_input",
      descendant_count: 2,
      active_descendant_count: 2,
      urgent_descendant_count: 1
    });
  });

  test("orders an urgent nested branch before a running sibling", () => {
    const store = new PetStore(policy);
    const urgentBranch = relayEvent({
      type: "session_started"
    }, "codex.urgent-branch");
    urgentBranch.session.parent_session_id = "root";
    urgentBranch.session.agent_path = "/root/urgent";
    store.ingest(urgentBranch, 0);
    const urgentLeaf = relayEvent({
      type: "unknown",
      native_type: "event_msg.request_user_input",
      native: { id: "question-1" }
    }, "codex.urgent-leaf");
    urgentLeaf.session.parent_session_id = "urgent-branch";
    urgentLeaf.session.agent_path = "/root/urgent/reviewer";
    store.ingest(urgentLeaf, 10);
    const runningSibling = relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "working"
    }, "codex.running-sibling");
    runningSibling.session.parent_session_id = "root";
    runningSibling.session.agent_path = "/root/running";
    store.ingest(runningSibling, 10);

    expect(store.snapshot(11).sessions.map((session) => session.topic)).toEqual([
      "codex.root",
      "codex.urgent-branch",
      "codex.urgent-leaf",
      "codex.running-sibling"
    ]);
  });

  test("deduplicates copied agent activity by provider and event id", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "spawn-1",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 0
    }), 0);
    store.ingest(agentActivity("root", {
      event_id: "spawn-1",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 90
    }), 90);

    expect(store.snapshot(101).sessions).toEqual([]);
    expect(store.snapshot(101).total_sessions).toBe(2);
  });

  test("uses provider event times so replayed starts do not look live", () => {
    const store = new PetStore(policy);
    store.ingest(agentActivity("root", {
      event_id: "old-spawn",
      kind: "started",
      target_session_id: "child",
      target_agent_path: "/root/researcher",
      occurred_at_ms: 100
    }), 1_000);
    store.ingest(agentActivity("root", {
      event_id: "old-spawn-timestamp",
      kind: "started",
      target_session_id: "child-from-timestamp",
      target_agent_path: "/root/reviewer",
      timestamp: "1970-01-01T00:00:00.100Z"
    }), 1_000);

    const snapshot = store.snapshot(1_000);
    expect(snapshot.active_sessions).toBe(0);
    expect(snapshot.sessions).toEqual([]);
    expect(snapshot.total_sessions).toBe(3);
  });

  test("prefers explicit project, folder, repository, then legacy names", () => {
    const store = new PetStore(policy);
    const cases = [
      {
        topic: "codex.project",
        project: {
          name: "tokn",
          project_name: "llm-router_2",
          folder: "/worktrees/59e1/llm-router",
          folder_name: "llm-router",
          repository_name: "tokn"
        },
        expected: "llm-router_2"
      },
      {
        topic: "codex.folder",
        project: {
          name: "legacy",
          project_name: null,
          folder: "/worktrees/59e1/llm-router",
          folder_name: "llm-router",
          repository_name: "tokn"
        },
        expected: "llm-router"
      },
      {
        topic: "codex.repository",
        project: {
          name: "legacy",
          project_name: null,
          folder: "/worktrees/59e1/llm-router",
          folder_name: null,
          repository_name: "tokn"
        },
        expected: "tokn"
      },
      {
        topic: "codex.legacy",
        project: {
          name: "legacy-project",
          folder: "/work/legacy-project"
        },
        expected: "legacy-project"
      }
    ] as const;

    for (const item of cases) {
      const relay = relayEvent({
        type: "reasoning",
        phase: "delta"
      }, item.topic);
      relay.session.project = item.project;
      store.ingest(relay, 0);
    }

    const names = new Map(
      store.snapshot(1).sessions.map((session) => [
        session.topic,
        session.project_label
      ])
    );
    for (const item of cases) {
      expect(names.get(item.topic)).toBe(item.expected);
    }
  });

  test("keeps recoverable tool errors in running", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "tool_call",
      tool_call_id: "call-1",
      tool_name: "exec_command",
      phase: "finished",
      output: { exit_code: 1 },
      is_error: true
    }), 0);
    expect(store.snapshot(1).state).toBe("running");
  });

  test("lists active sessions before retained recent completions", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "done"
    }, "codex.completed"), 0);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta",
      text: "working"
    }, "codex.running"), 20);
    store.ingest(relayEvent({
      type: "session_started"
    }, "codex.inactive"), 30);

    const withActive = store.snapshot(70);
    expect(withActive.sessions.map((session) => session.topic)).toEqual([
      "codex.running",
      "codex.completed"
    ]);
    expect(withActive.sessions[1]).toMatchObject({
      state: "idle",
      completed_at: 10,
      recently_completed: true
    });
    expect(withActive.focus?.topic).toBe("codex.running");
    expect(withActive.active_sessions).toBe(1);
    expect(withActive.total_sessions).toBe(3);

    const onlyRecent = store.snapshot(150);
    expect(onlyRecent.sessions.map((session) => session.topic)).toEqual([
      "codex.completed"
    ]);
    expect(onlyRecent.focus?.topic).toBe("codex.completed");
    expect(onlyRecent.state).toBe("idle");

    const expired = store.snapshot(510);
    expect(expired.sessions).toEqual([]);
    expect(expired.focus).toBeUndefined();
    expect(expired.total_sessions).toBe(3);
  });

  test("sorts active and recent sessions deterministically", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta"
    }, "codex.running-z"), 0);
    store.ingest(relayEvent({
      type: "reasoning",
      phase: "delta"
    }, "codex.running-a"), 0);
    store.ingest(relayEvent({
      type: "goal_updated",
      goal: { status: "blocked" }
    }, "codex.blocked"), 0);
    store.ingest(relayEvent({
      type: "unknown",
      native_type: "event_msg.request_user_input",
      native: { id: "question-1" }
    }, "codex.waiting"), 0);

    expect(store.snapshot(1).sessions.map((session) => session.topic)).toEqual([
      "codex.waiting",
      "codex.blocked",
      "codex.running-a",
      "codex.running-z"
    ]);

    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "older"
    }, "codex.completed-old"), 10);
    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "newer"
    }, "codex.completed-new"), 20);

    expect(store.snapshot(450).sessions.map((session) => session.topic)).toEqual([
      "codex.completed-new",
      "codex.completed-old"
    ]);
  });

  test("acknowledging a completion removes it from the recent roster", () => {
    const store = new PetStore(policy);
    store.ingest(relayEvent({
      type: "message",
      role: "assistant",
      phase: "finished",
      text: "done"
    }), 0);
    expect(store.snapshot(20).sessions).toHaveLength(1);

    store.acknowledge("codex.session-1");

    const snapshot = store.snapshot(20);
    expect(snapshot.sessions).toEqual([]);
    expect(snapshot.focus).toBeUndefined();
    expect(snapshot.total_sessions).toBe(1);
  });
});

function agentActivity(
  sourceSessionId: string,
  event: {
    event_id: string;
    kind: "started" | "interacted" | "interrupted" | "messaged";
    target_session_id?: string;
    target_agent_path?: string;
    actor_agent_path?: string;
    occurred_at_ms?: number;
    timestamp?: string;
  }
) {
  return relayEvent({
    type: "agent_activity",
    provider: "codex",
    ...event
  }, `codex.${sourceSessionId}`);
}
