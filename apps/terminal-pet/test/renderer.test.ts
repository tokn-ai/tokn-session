import { describe, expect, test } from "bun:test";

import { loadPetArt } from "../src/art";
import { focusSnapshot } from "../src/navigation";
import { renderScreen } from "../src/renderer";
import type { PetFocus, PetSnapshot } from "../src/state";

describe("renderer", () => {
  test("renders a deterministic monochrome session roster", async () => {
    const art = await loadPetArt();
    const running = petFocus({
      topic: "codex.session-1",
      label: "exec_command: cargo test",
      project_label: "llm-router_2",
      title: "A title that must not hide the GUI project"
    });
    const snapshot = petSnapshot([running]);
    const screen = renderScreen(snapshot, art.running.ansi, {
      source_label: "test"
    }, {
      columns: 100,
      rows: 24,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });
    const output = screen.lines.join("\n");

    expect(output).toContain("H A C H I W A R E");
    expect(output).toContain("SESSION ROSTER");
    expect(output).toContain("ACTIVE 1");
    expect(output).toContain("● codex");
    const runningRow = screen.lines.find((line) => (
      line.includes("● codex") && line.includes("llm-router_2")
    )) ?? "";
    expect(runningRow).not.toContain("Running");
    expect(output).toContain("codex · llm-router_2");
    expect(output).toContain("llm-router_2");
    expect(output).toContain("exec_command: cargo test");
    expect(output).not.toContain("\u001b");

    const titled = petFocus({
      project_label: undefined,
      title: "Investigate Relay",
      agent: "Zeno"
    });
    const titledOutput = renderScreen(petSnapshot([titled]), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 100,
      rows: 24,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    }).lines.join("\n");
    expect(titledOutput).toContain("Investigate Relay");
  });

  test("shows concurrent sessions and recently ready work", async () => {
    const art = await loadPetArt();
    const waiting = petFocus({
      topic: "codex.waiting",
      state: "needs_input",
      label: "Approval required",
      session_id: "waiting"
    });
    const running = petFocus({
      topic: "pi.running",
      label: "Running provider tests",
      provider: "pi",
      session_id: "running",
      agent: "worker"
    });
    const recent = petFocus({
      topic: "codex.recent",
      state: "idle",
      label: "Updated Relay docs",
      session_id: "recent",
      agent: "docs",
      completed_at: 30_000,
      recently_completed: true
    });
    const snapshot = petSnapshot([waiting, running, recent], 3);
    const screen = renderScreen(snapshot, art.waiting.ansi, {
      source_label: "test"
    }, {
      columns: 100,
      rows: 24,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 90_000
    });
    const output = screen.lines.join("\n");

    expect(output).toContain("ACTIVE 2");
    expect(output).toContain("RECENT 1");
    expect(output).toContain("? codex");
    expect(output).toContain("● pi");
    expect(output).toContain("Running provider tests");
    expect(output).toContain("✓ codex");
    expect(output).not.toContain("✓ Ready");
    expect(output).toContain("Updated Relay docs");
    expect(output).toContain("1 recent");
  });

  test("renders focused activity details in the wide layout", async () => {
    const art = await loadPetArt();
    const focused = petFocus({
      cwd: "/workspace/demo-project",
      current_activity: {
        kind: "tool",
        label: "exec_command: cargo test",
        detail: "cargo test --workspace",
        at: 88_000
      },
      last_event_at: 88_000,
      family_last_event_at: 88_000
    });
    const output = renderScreen(petSnapshot([focused]), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 100,
      rows: 24,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 100_000
    }).lines.join("\n");

    expect(output).toContain("FOCUS");
    expect(output).toContain("Current activity: cargo test --workspace");
    expect(output).toContain("12s ago");
    expect(output).toContain("cwd /workspace/demo-project");
    expect(output).toContain("SESSION ROSTER");
  });

  test("aligns roster columns across provider and text widths", async () => {
    const art = await loadPetArt();
    const root = petFocus({
      topic: "codex.root",
      session_id: "root",
      state: "idle",
      family_state: "needs_input",
      root_topic: "codex.root",
      descendant_count: 5,
      active_descendant_count: 3,
      urgent_descendant_count: 2,
      recent_descendant_count: 2
    });
    const children = [
      petFocus({
        topic: "codex.runner",
        session_id: "runner",
        root_topic: root.topic,
        parent_topic: root.topic,
        depth: 1,
        agent: "Runner",
        label: "Running task",
        last_event_at: 10_000
      }),
      petFocus({
        topic: "codex.approver",
        session_id: "approver",
        root_topic: root.topic,
        parent_topic: root.topic,
        depth: 1,
        state: "needs_input",
        family_state: "needs_input",
        agent: "Approver",
        label: "Approval required",
        last_event_at: 1_000
      }),
      petFocus({
        topic: "codex.blocker",
        session_id: "blocker",
        root_topic: root.topic,
        parent_topic: root.topic,
        depth: 1,
        state: "blocked",
        family_state: "blocked",
        agent: "Blocker",
        label: "Blocked on dependency",
        last_event_at: -5_000
      }),
      petFocus({
        topic: "codex.writer",
        session_id: "writer",
        root_topic: root.topic,
        parent_topic: root.topic,
        depth: 1,
        state: "idle",
        family_state: "idle",
        agent: "作家",
        label: "Release notes written",
        completed_at: 0,
        recently_completed: true,
        outcome: "completed"
      }),
      petFocus({
        topic: "codex.reviewer",
        session_id: "reviewer",
        root_topic: root.topic,
        parent_topic: root.topic,
        depth: 1,
        state: "idle",
        family_state: "idle",
        agent: "Reviewer",
        label: "Review stopped",
        completed_at: -110_000,
        recently_completed: true,
        outcome: "interrupted"
      })
    ];
    const snapshot = petSnapshot([root, ...children]);
    snapshot.focus = children[0];
    const screen = renderScreen(snapshot, art.running.ansi, {
      source_label: "test"
    }, {
      columns: 120,
      rows: 24,
      color: true,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 10_000
    });

    const identityColumns = [
      "tokn-agent · root",
      "↳ Runner",
      "↳ Approver",
      "↳ Blocker",
      "↳ 作家",
      "↳ Reviewer"
    ].map((identity) => screenColumn(screen.lines, identity));
    const rosterRows = [
      ["tokn-agent · root", "5 agents · 2 urgent"],
      ["↳ Runner", "Running task"],
      ["↳ Approver", "Approval required"],
      ["↳ Blocker", "Blocked on dependency"],
      ["↳ 作家", "Release notes written"],
      ["↳ Reviewer", "Review stopped"]
    ] as const;
    const activityColumns = rosterRows.map(([identity, activity]) => (
      screenColumnForRow(screen.lines, identity, activity)
    ));
    const rowEndColumns = [
      "tokn-agent · root",
      "↳ Runner",
      "↳ Approver",
      "↳ Blocker",
      "↳ 作家",
      "↳ Reviewer"
    ].map((identity) => screenEndColumn(screen.lines, identity));

    expect(new Set(identityColumns).size).toBe(1);
    expect(new Set(activityColumns).size).toBe(1);
    expect(new Set(rowEndColumns).size).toBe(1);
    expect(screen.lines.join("\n")).toContain("\u001b");
    expect(screen.lines.every((line) => Bun.stringWidth(line) <= 120)).toBe(true);
  });

  test("renders each root as a project family with indented agents", async () => {
    const art = await loadPetArt();
    const root = petFocus({
      topic: "codex.root",
      session_id: "root",
      state: "idle",
      family_state: "running",
      family_last_event_at: 9_000,
      root_topic: "codex.root",
      project_label: "llm-router_2",
      agent: "root",
      label: "Waiting for agents",
      descendant_count: 2,
      active_descendant_count: 1,
      recent_descendant_count: 1
    });
    const worker = petFocus({
      topic: "codex.worker",
      session_id: "worker",
      root_topic: root.topic,
      parent_topic: root.topic,
      depth: 1,
      agent: "Zeno",
      label: "Reviewing the protocol"
    });
    const recent = petFocus({
      topic: "codex.recent-child",
      session_id: "recent",
      root_topic: root.topic,
      parent_topic: root.topic,
      depth: 1,
      state: "idle",
      family_state: "idle",
      agent: "/root/docs",
      label: "Updated documentation",
      completed_at: 5_000,
      recently_completed: true,
      outcome: "completed"
    });
    const snapshot = petSnapshot([root, worker, recent]);
    snapshot.focus = worker;
    snapshot.state = worker.state;

    const output = renderScreen(snapshot, art.running.ansi, {
      source_label: "test"
    }, {
      columns: 100,
      rows: 24,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 10_000
    }).lines.join("\n");

    expect(output).toContain("ACTIVE 1");
    expect(output).toContain("llm-router_2 · root");
    expect(output).toContain("2 agents · 1 active");
    expect(screenLine(output, "2 agents · 1 active")).toContain("· now");
    expect(output).toContain("↳ Zeno · worker");
    expect(output).toContain("↳ root/docs · recent");
    expect(output).not.toContain("llm-router_2/Zeno");
  });

  test("keeps focus on an urgent child while surfacing urgency on its root", async () => {
    const art = await loadPetArt();
    const root = petFocus({
      topic: "codex.root",
      session_id: "root",
      state: "idle",
      family_state: "needs_input",
      root_topic: "codex.root",
      descendant_count: 1,
      active_descendant_count: 1,
      urgent_descendant_count: 1
    });
    const child = petFocus({
      topic: "codex.reviewer",
      session_id: "reviewer",
      root_topic: root.topic,
      parent_topic: root.topic,
      depth: 1,
      state: "needs_input",
      family_state: "needs_input",
      agent: "Reviewer",
      label: "Approval required"
    });
    const snapshot = petSnapshot([root, child]);
    snapshot.focus = child;
    snapshot.state = child.state;

    const screen = renderScreen(snapshot, art.waiting.ansi, {
      source_label: "test"
    }, {
      columns: 100,
      rows: 24,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });
    const output = screen.lines.join("\n");
    const childLine = screen.lines.find((line) => (
      line.includes("Approval required")
      && line.includes("› ? codex")
    )) ?? "";
    const rootLine = lastScreenLine(screen.lines, "1 urgent");

    expect(output).toContain("ACTIVE 1");
    expect(rootLine).toContain("? codex · tokn-agent");
    expect(rootLine).not.toContain("› ? codex");
    expect(childLine).toContain("› ? codex · ↳ Reviewer");
  });

  test("uses family state and timing when a root is manually focused", async () => {
    const art = await loadPetArt();
    const root = petFocus({
      topic: "codex.root",
      session_id: "root",
      state: "idle",
      family_state: "needs_input",
      family_last_event_at: 5_000,
      root_topic: "codex.root",
      label: "Waiting for agents",
      descendant_count: 1,
      active_descendant_count: 1,
      urgent_descendant_count: 1
    });
    const child = petFocus({
      topic: "codex.reviewer",
      session_id: "reviewer",
      root_topic: root.topic,
      parent_topic: root.topic,
      depth: 1,
      state: "needs_input",
      family_state: "needs_input",
      last_event_at: 5_000,
      family_last_event_at: 5_000,
      agent: "Reviewer",
      label: "Approval required"
    });
    const snapshot = focusSnapshot(petSnapshot([root, child]), root.topic);
    const screen = renderScreen(snapshot, art.waiting.ansi, {
      source_label: "test",
      focus_mode: "manual"
    }, {
      columns: 100,
      rows: 24,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 10_000
    });
    const output = screen.lines.join("\n");
    const rootLine = lastScreenLine(screen.lines, "1 urgent");
    const childLine = lastScreenLine(screen.lines, "Approval required");

    expect(snapshot.state).toBe("needs_input");
    expect(snapshot.state_changed_at).toBe(5_000);
    expect(output).toContain("? Needs input");
    expect(rootLine).toContain("› ? codex · tokn-agent");
    expect(rootLine).toContain("· 5s");
    expect(childLine).not.toContain("› ? Needs input");
  });

  test("keeps completed and interrupted children in their root family", async () => {
    const art = await loadPetArt();
    const root = petFocus({
      topic: "codex.root",
      session_id: "root",
      state: "idle",
      family_state: "idle",
      root_topic: "codex.root",
      descendant_count: 2,
      recent_descendant_count: 2
    });
    const completed = petFocus({
      topic: "codex.completed",
      session_id: "complete",
      root_topic: root.topic,
      parent_topic: root.topic,
      depth: 1,
      state: "idle",
      family_state: "idle",
      agent: "Writer",
      label: "Finished release notes",
      completed_at: 8_000,
      recently_completed: true,
      outcome: "completed"
    });
    const interrupted = petFocus({
      topic: "codex.interrupted",
      session_id: "stopped",
      root_topic: root.topic,
      parent_topic: root.topic,
      depth: 1,
      state: "idle",
      family_state: "idle",
      agent: "Reviewer",
      label: "Interrupted",
      completed_at: 7_000,
      recently_completed: true,
      outcome: "interrupted"
    });

    const snapshot = petSnapshot([root, completed, interrupted]);
    snapshot.focus = interrupted;
    snapshot.state = interrupted.state;
    const output = renderScreen(
      snapshot,
      art.ready.ansi,
      {
        source_label: "test"
      },
      {
        columns: 100,
        rows: 24,
        color: false,
        image_protocol: "ansi",
        name: "Hachiware",
        now_ms: 10_000
      }
    ).lines.join("\n");

    expect(output).toContain("RECENT 2");
    expect(output).toContain("2 agents · 2 recent");
    expect(output).toMatch(/✓ codex\s+· ↳ Writer/);
    expect(output).toContain("× codex · ↳ Reviewer");
    expect(output).toContain("× Interrupted");
    expect(output.match(/× codex/g)).toHaveLength(1);
    expect(output).not.toContain("! Blocked");
    expect(output).not.toContain("✓ Ready recently");
  });

  test("renders interrupted roots and subagents as neutral outcomes", async () => {
    const art = await loadPetArt();
    const root = petFocus({
      topic: "codex.root",
      session_id: "root",
      state: "idle",
      family_state: "idle",
      root_topic: "codex.root",
      label: "Interrupted",
      completed_at: 8_000,
      recently_completed: true,
      outcome: "interrupted",
      descendant_count: 1,
      recent_descendant_count: 1
    });
    const child = petFocus({
      topic: "codex.child",
      session_id: "child",
      root_topic: root.topic,
      parent_topic: root.topic,
      depth: 1,
      state: "idle",
      family_state: "idle",
      agent: "Reviewer",
      label: "Interrupted",
      completed_at: 9_000,
      recently_completed: true,
      outcome: "interrupted"
    });

    const screen = renderScreen(
      petSnapshot([root, child]),
      art.idle.ansi,
      { source_label: "test" },
      {
        columns: 100,
        rows: 24,
        color: true,
        image_protocol: "ansi",
        name: "Hachiware",
        now_ms: 10_000
      }
    );
    const rootLine = lastScreenLine(screen.lines, "tokn-agent");
    const childLine = lastScreenLine(screen.lines, "Reviewer");
    const blockedColor = "\u001b[38;2;239;91;91m";
    const idleColor = "\u001b[38;2;139;148;158m";

    expect(rootLine).toContain("×");
    expect(childLine).toContain("×");
    expect(rootLine).toContain(idleColor);
    expect(childLine).toContain(idleColor);
    expect(rootLine).not.toContain(blockedColor);
    expect(childLine).not.toContain(blockedColor);
  });

  test("uses one image anchor alongside the wide roster", async () => {
    const art = await loadPetArt();
    const running = petFocus();
    const screen = renderScreen(petSnapshot([running]), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 80,
      rows: 20,
      color: false,
      image_protocol: "kitty",
      name: "Hachiware",
      now_ms: 0
    });

    expect(screen.image_anchor).toEqual({
      column: 6,
      row: 3,
      columns: 10,
      rows: 5
    });
    expect(screen.lines.join("\n")).toContain("ACTIVE 1");
  });

  test("strips terminal controls and reports narrow-list overflow", async () => {
    const art = await loadPetArt();
    const sessions = Array.from({ length: 8 }, (_, index) => petFocus({
      topic: `codex.session-${index}`,
      session_id: `session-${index}`,
      label: index === 0
        ? "\u001b]52;c;dGVzdA==\u0007 進捗進捗進捗進捗進捗進捗"
        : `Working on task ${index}`,
      last_event_at: -index * 1_000
    }));
    const screen = renderScreen(petSnapshot(sessions, 8), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 30,
      rows: 8,
      color: false,
      image_protocol: "kitty",
      name: "Hachiware",
      now_ms: 10_000
    });
    const output = screen.lines.join("\n");

    expect(screen.image_anchor).toBeUndefined();
    expect(output).not.toContain("\u001b");
    expect(output).toContain("more");
    expect(output).toContain("10s");
    expect(screen.lines.every((line) => Bun.stringWidth(line) <= 30)).toBe(true);
  });

  test("uses available rows on tall wide and narrow terminals", async () => {
    const art = await loadPetArt();
    const sessions = Array.from({ length: 20 }, (_, index) => petFocus({
      topic: `codex.session-${index}`,
      session_id: `session-${index}`,
      label: `Task ${index}`
    }));
    const wide = renderScreen(petSnapshot(sessions, 20), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 80,
      rows: 40,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });
    const narrow = renderScreen(petSnapshot(sessions.slice(0, 6), 6), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 23,
      rows: 12,
      color: false,
      image_protocol: "kitty",
      name: "Hachiware",
      now_ms: 0
    });

    expect(wide.lines.join("\n")).toContain("Task 19");
    expect(wide.lines.join("\n")).not.toContain("more");
    expect(narrow.image_anchor).toBeUndefined();
    expect(narrow.lines.join("\n")).toContain("ACTIVE 6");
    expect(narrow.lines.join("\n")).toContain("Task 5");
    expect(narrow.lines.every((line) => Bun.stringWidth(line) <= 23)).toBe(true);
  });

  test("prefers a session row over a heading in a two-row roster", async () => {
    const art = await loadPetArt();
    const sessions = Array.from({ length: 6 }, (_, index) => petFocus({
      topic: `codex.session-${index}`,
      session_id: `session-${index}`,
      label: `Task ${index}`
    }));
    const screen = renderScreen(petSnapshot(sessions, 6), art.running.ansi, {
      source_label: "test",
      diagnostic: "Relay warning"
    }, {
      columns: 24,
      rows: 6,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });
    const output = screen.lines.join("\n");

    expect(output).toContain("Task");
    expect(output).toContain("+5 more");
    expect(output).not.toContain("ACTIVE 6");
  });

  test("keeps diagnostics and provider identity in constrained layouts", async () => {
    const art = await loadPetArt();
    const running = petFocus({
      project_label: undefined,
      agent: undefined,
      title: undefined
    });
    const wide = renderScreen(petSnapshot([running]), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 80,
      rows: 20,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });
    const tiny = renderScreen(petSnapshot([running]), art.running.ansi, {
      source_label: "test",
      diagnostic: "ERR"
    }, {
      columns: 7,
      rows: 4,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });

    expect(wide.lines.join("\n")).toContain("codex · session");
    expect(tiny.lines.join("\n")).toContain("ERR");
    expect(tiny.lines.every((line) => Bun.stringWidth(line) <= 7)).toBe(true);
  });

  test("fills tight roster budgets before reporting overflow", async () => {
    const art = await loadPetArt();
    const active = petFocus({
      topic: "codex.active",
      session_id: "active",
      label: "Active task"
    });
    const recent = petFocus({
      topic: "codex.recent",
      state: "idle",
      session_id: "recent",
      label: "Recent task",
      completed_at: 0,
      recently_completed: true
    });
    const twoSessions = renderScreen(petSnapshot([active, recent], 2), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 24,
      rows: 6,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 10_000
    });
    const fourSessions = Array.from({ length: 4 }, (_, index) => petFocus({
      topic: `codex.session-${index}`,
      session_id: `session-${index}`,
      label: `Task ${index}`
    }));
    const fiveRows = renderScreen(petSnapshot(fourSessions, 4), art.running.ansi, {
      source_label: "test"
    }, {
      columns: 40,
      rows: 5,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });

    expect(twoSessions.lines.join("\n")).toContain("Active");
    expect(twoSessions.lines.join("\n")).toContain("Recent");
    expect(twoSessions.lines.join("\n")).not.toContain("more");
    expect(fiveRows.lines.join("\n")).toContain("Task 0");
    expect(fiveRows.lines.join("\n")).toContain("+3 more");
  });

  test("preserves activity age across intermediate narrow widths", async () => {
    const art = await loadPetArt();
    const running = petFocus();

    for (let columns = 8; columns < 32; columns += 1) {
      const screen = renderScreen(petSnapshot([running]), art.running.ansi, {
        source_label: "test"
      }, {
        columns,
        rows: 8,
        color: false,
        image_protocol: "ansi",
        name: "Hachiware",
        now_ms: 10_000
      });
      expect(screen.lines.join("\n")).toContain("10s");
      expect(screen.lines.every((line) => Bun.stringWidth(line) <= columns)).toBe(true);
    }
  });

  test("keeps a manually focused session visible through overflow", async () => {
    const art = await loadPetArt();
    const sessions = Array.from({ length: 8 }, (_, index) => petFocus({
      topic: `codex.session-${index}`,
      session_id: `session-${index}`,
      label: `Task ${index}`,
      state: index === 0 ? "needs_input" : "running"
    }));
    const snapshot = petSnapshot(sessions, 8);
    snapshot.focus = sessions.at(-1);
    snapshot.state = snapshot.focus?.state ?? "idle";
    snapshot.state_changed_at = snapshot.focus?.state_changed_at ?? 0;
    const screen = renderScreen(snapshot, art.running.ansi, {
      source_label: "test",
      focus_mode: "manual"
    }, {
      columns: 30,
      rows: 8,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });
    const tiny = renderScreen(snapshot, art.running.ansi, {
      source_label: "test",
      focus_mode: "manual"
    }, {
      columns: 80,
      rows: 4,
      color: false,
      image_protocol: "ansi",
      name: "Hachiware",
      now_ms: 0
    });
    const output = screen.lines.join("\n");
    const tinyOutput = tiny.lines.join("\n");

    expect(output).toContain("Task 7");
    expect(output).toContain("1 urgent hidden");
    expect(output).toContain("focus MANUAL");
    expect(tinyOutput).toContain("Task 7");
    expect(tinyOutput).toContain("1 urgent hidden");
    expect(tinyOutput).toContain("focus MANUAL");
  });

  test("does not advertise keyboard controls when input is unavailable", async () => {
    const art = await loadPetArt();
    const screen = renderScreen(
      petSnapshot([petFocus()]),
      art.running.ansi,
      {
        source_label: "stdin",
        control_mode: "signal_only"
      },
      {
        columns: 80,
        rows: 20,
        color: false,
        image_protocol: "ansi",
        name: "Hachiware",
        now_ms: 0
      }
    );
    const output = screen.lines.join("\n");

    expect(output).toContain("keyboard controls unavailable");
    expect(output).not.toContain("↑/↓ select");
  });

  test("shows the active input composer and its controls", async () => {
    const art = await loadPetArt();
    const screen = renderScreen(
      petSnapshot([petFocus({ provider: "pi" })]),
      art.running.ansi,
      {
        source_label: "relay",
        control_mode: "relay",
        input_active: true,
        input_line: "continue the task"
      },
      {
        columns: 80,
        rows: 20,
        color: false,
        image_protocol: "ansi",
        name: "Hachiware",
        now_ms: 0
      }
    );
    const output = screen.lines.join("\n");

    expect(output).toContain("> continue the task");
    expect(output).toContain("Enter send · Esc cancel");
  });
});

function petFocus(overrides: Partial<PetFocus> = {}): PetFocus {
  const topic = overrides.topic ?? "codex.session-1";
  const state = overrides.state ?? "running";
  const lastEventAt = overrides.last_event_at ?? 0;
  return {
    topic,
    state,
    state_changed_at: 0,
    last_event_at: lastEventAt,
    label: "Thinking",
    provider: "codex",
    project_label: "tokn-agent",
    session_id: "session-1",
    agent: "root",
    root_topic: overrides.root_topic ?? topic,
    depth: 0,
    is_provisional: false,
    family_state: overrides.family_state ?? state,
    family_last_event_at: overrides.family_last_event_at ?? lastEventAt,
    descendant_count: 0,
    active_descendant_count: 0,
    urgent_descendant_count: 0,
    recent_descendant_count: 0,
    recently_completed: false,
    ...overrides
  };
}

function petSnapshot(sessions: PetFocus[], totalSessions = sessions.length): PetSnapshot {
  const focus = sessions[0];
  return {
    state: focus?.state ?? "idle",
    state_changed_at: focus?.state_changed_at ?? 0,
    active_sessions: sessions.filter((session) => session.state !== "idle").length,
    total_sessions: totalSessions,
    sessions,
    focus
  };
}

function screenLine(output: string, content: string): string {
  return output.split("\n").find((line) => line.includes(content)) ?? "";
}

function lastScreenLine(lines: string[], content: string): string {
  return [...lines].reverse().find((line) => line.includes(content)) ?? "";
}

function screenColumn(lines: string[], content: string): number {
  const line = [...lines]
    .reverse()
    .map((candidate) => Bun.stripANSI(candidate))
    .find((candidate) => candidate.includes(content));
  expect(line).toBeDefined();
  return Bun.stringWidth(line!.slice(0, line!.indexOf(content)));
}

function screenColumnForRow(
  lines: string[],
  identity: string,
  activity: string
): number {
  const line = lines
    .map((candidate) => Bun.stripANSI(candidate))
    .find((candidate) => candidate.includes(identity) && candidate.includes(activity));
  expect(line).toBeDefined();
  return Bun.stringWidth(line!.slice(0, line!.indexOf(activity)));
}

function screenEndColumn(lines: string[], content: string): number {
  const line = [...lines]
    .reverse()
    .map((candidate) => Bun.stripANSI(candidate))
    .find((candidate) => candidate.includes(content));
  expect(line).toBeDefined();
  return Bun.stringWidth(line!.trimEnd());
}
