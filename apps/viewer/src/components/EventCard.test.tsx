import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type {
  EventDetail,
  EventSummary,
  ReasoningCardSummary,
  SessionSummary,
  ToolCardSummary,
  TrajectoryCardSummary,
  TrajectoryEventPageState,
  UsageCardSummary,
} from "../lib/types";
import { EventCard, formatTrajectoryDuration } from "./EventCard";

afterEach(cleanup);

function tool(overrides: Partial<ToolCardSummary> = {}): ToolCardSummary {
  return {
    kind: "shell",
    tool_name: "exec_command",
    tool_call_id: "call-1",
    command: "cargo test",
    cwd: "/work/repo",
    path: null,
    query: null,
    url: null,
    task_title: null,
    exit_code: 0,
    bytes: null,
    added: null,
    removed: null,
    ...overrides,
  };
}

function usage(overrides: Partial<UsageCardSummary> = {}): UsageCardSummary {
  return {
    kind: "model_call",
    input_tokens: "33",
    output_tokens: "5",
    total_tokens: "38",
    cache_read_tokens: "20",
    cache_write_tokens: "3",
    reasoning_tokens: "2",
    turn_id: null,
    step_id: null,
    ...overrides,
  };
}

function reasoning(overrides: Partial<ReasoningCardSummary> = {}): ReasoningCardSummary {
  return {
    preview: "Inspect the source",
    has_summary: true,
    has_text: false,
    has_encrypted_content: false,
    is_redacted: false,
    ...overrides,
  };
}

function subagent(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    session_key: "session.v1.child",
    session_id: "child-session-123456",
    parent_session_id: "parent-session",
    is_subagent: true,
    provider: "codex",
    title: null,
    preview: null,
    project: "tokn-agent",
    cwd: "/work/tokn-agent",
    updated_at_ms: 1_788_000_000_000,
    timestamp: "2026-08-31T00:00:00Z",
    agent_path: "/root/reviewer",
    agent_nickname: "Hubble",
    agent_role: "reviewer",
    child_count: 0,
    message_count: null,
    event_count: null,
    history_status: null,
    has_unread: false,
    ...overrides,
  };
}

function event(overrides: Partial<EventSummary> = {}): EventSummary {
  return {
    event_key: "event.v1.1",
    type: "message",
    provider: "codex",
    timestamp: "2026-08-31T00:00:00Z",
    phase: "finished",
    role: "assistant",
    title: "Assistant message",
    summary: "A **formatted** answer",
    summary_truncated: false,
    is_hidden: false,
    is_error: false,
    tool: null,
    usage: null,
    reasoning: null,
    ...overrides,
  };
}

function detail(overrides: Partial<EventDetail> = {}): EventDetail {
  return {
    event_key: "event.v1.1",
    event: { type: "tool_call" },
    native: null,
    is_hidden: false,
    tool_output: null,
    ...overrides,
  };
}

function trajectory(overrides: Partial<TrajectoryCardSummary> = {}): TrajectoryCardSummary {
  return {
    event_count: 4,
    tool_count: 1,
    reasoning_count: 1,
    agent_activity_count: 1,
    error_count: 0,
    unknown_count: 0,
    started_at: "2026-08-31T00:00:00Z",
    ended_at: "2026-08-31T01:00:00Z",
    duration_ms: "3600000",
    ...overrides,
  };
}

function trajectoryPage(
  overrides: Partial<TrajectoryEventPageState> = {},
): TrajectoryEventPageState {
  return {
    events: [],
    next_cursor: null,
    previous_cursor: null,
    total_events: 0,
    has_loaded: true,
    is_loading: false,
    is_loading_older: false,
    is_loading_newer: false,
    error: null,
    error_direction: null,
    error_cursor: null,
    ...overrides,
  };
}

function renderCard(
  cardEvent: EventSummary,
  overrides: Partial<React.ComponentProps<typeof EventCard>> = {},
) {
  return render(
    <EventCard
      button_id="event-button"
      detail={null}
      detail_error={null}
      detail_loading={false}
      event={cardEvent}
      is_expanded={false}
      is_selected={false}
      on_retry_detail={vi.fn()}
      on_select={vi.fn()}
      on_toggle={vi.fn()}
      {...overrides}
    />,
  );
}

describe("EventCard conversation content", () => {
  it.each(["tool_call", "reasoning"] as const)("allows retrying an empty retained %s detail", (type) => {
    const onRetry = vi.fn();
    renderCard(event({
      type,
      role: null,
      tool: type === "tool_call" ? tool() : null,
      reasoning: type === "reasoning" ? reasoning() : null,
    }), {
      is_expanded: true,
      detail: detail({ event: { type } }),
      detail_error: "temporary failure",
      on_retry_detail: onRetry,
    });

    expect(screen.getByRole("alert")).toHaveTextContent("temporary failure");
    expect(screen.getByText(type === "tool_call"
      ? "No output was captured for this tool call."
      : "No readable reasoning was captured for this event.")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });

  it.each(["tool_call", "reasoning", "compaction"] as const)("keeps %s detail mounted during refresh and refresh failure", (type) => {
    const cardEvent = event({
      type,
      role: null,
      title: "Detail card",
      summary: "Preview",
      tool: type === "tool_call" ? tool() : null,
      reasoning: type === "reasoning" ? reasoning() : null,
      compaction: type === "compaction" ? {
        state: "completed", trigger: null, reason: null, has_summary: true, summary_opaque: false, measurements: [],
      } : undefined,
    });
    const existing = detail({
      event: { type, summary: "Stable detail" },
      tool_output: type === "tool_call" ? {
        sections: [{ label: "stdout", text: "Stable detail", format: "text" }],
        truncated: false, original_size_bytes: 13, source_event_key: cardEvent.event_key,
      } : null,
    });
    const props = {
      button_id: "event-button", detail: existing, detail_error: null,
      detail_loading: false, event: cardEvent, is_expanded: true, is_selected: false,
      on_retry_detail: vi.fn(), on_select: vi.fn(), on_toggle: vi.fn(),
    };
    const { rerender } = render(<EventCard {...props} />);
    const content = screen.getByText("Stable detail");
    rerender(<EventCard {...props} detail_loading />);
    expect(screen.getByText("Stable detail")).toBe(content);
    expect(screen.queryByText(/Loading (tool output|reasoning|compaction summary)/)).not.toBeInTheDocument();
    rerender(<EventCard {...props} detail_error="temporary failure" />);
    expect(screen.getByText("Stable detail")).toBe(content);
    expect(screen.getByRole("alert")).toHaveTextContent("temporary failure");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(props.on_retry_detail).toHaveBeenCalledOnce();
  });

  it("expands compaction summaries with precisely scoped measurements", () => {
    const onToggle = vi.fn();
    const compaction = event({
      type: "compaction", role: null, title: "Context compacted", summary: "Context compacted", phase: "completed",
      compaction: { state: "completed", trigger: "auto", reason: null, has_summary: true, summary_opaque: false,
        measurements: [{ scope: "replaced_context", tokens: "9007199254740993", estimated: true }] },
    });
    renderCard(compaction, { on_toggle: onToggle, is_expanded: true,
      detail: detail({ event: { type: "compaction", summary: "## Retained context\nA **decision**" } }),
    });
    expect(screen.getByRole("heading", { name: "Retained context" })).toBeInTheDocument();
    expect(screen.getByText("decision").tagName).toBe("STRONG");
    expect(screen.getByText("Replaced context: 9007199254740993 tokens (estimated)")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { expanded: true }));
    expect(onToggle).toHaveBeenCalledWith("event.v1.1");
  });

  it("explains opaque compaction summaries without inventing readable content", () => {
    renderCard(event({ type: "compaction", title: "Context compacted", role: null,
      compaction: { state: "completed", trigger: null, reason: null, has_summary: false, summary_opaque: true, measurements: [] },
    }), { is_expanded: true });
    expect(screen.getByText("The provider stored an opaque compaction summary.")).toBeInTheDocument();
  });

  it("renders visible conversation Markdown outside the inspect button", () => {
    const onSelect = vi.fn();
    const { container } = renderCard(event(), { on_select: onSelect });

    expect(screen.getByText("formatted").tagName).toBe("STRONG");
    expect(container.querySelector("button strong")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Inspect assistant message" }));
    expect(onSelect).toHaveBeenCalledWith("event.v1.1");
  });

  it("uses the user bubble presentation and keeps the user role label", () => {
    const { container } = renderCard(event({
      role: "user",
      title: "User message",
      summary: "My Request:",
    }));

    expect(container.querySelector(".message-event")).toHaveAttribute(
      "data-presentation",
      "bubble",
    );
    expect(screen.getByText("user")).toBeInTheDocument();
    expect(screen.getByText("My Request:")).toBeInTheDocument();
  });

  it("keeps hidden messages redacted and exposes the full-message action for truncation", () => {
    const { rerender } = renderCard(event({ is_hidden: true, summary: "secret" }));

    expect(screen.getByText("Hidden extension message")).toBeInTheDocument();
    expect(screen.queryByText("secret")).not.toBeInTheDocument();

    rerender(
      <EventCard
        button_id="event-button"
        detail={null}
        detail_error={null}
        detail_loading={false}
        event={event({ summary_truncated: true })}
        is_expanded={false}
        is_selected={false}
        on_retry_detail={vi.fn()}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
      />,
    );
    expect(screen.getByText("View full message")).toBeInTheDocument();
  });

  it("keeps unknown events expanded by default", () => {
    const { unmount } = renderCard(event({
      type: "unknown",
      role: null,
      title: "Mystery",
      summary: "raw shape",
    }));

    expect(screen.getByRole("button", { name: "Mystery" })).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("raw shape")).toBeInTheDocument();
    unmount();
  });
});

describe("EventCard tool headings", () => {
  it("uses compact provider-neutral headings for every known tool family", () => {
    const tools: Array<[string, Partial<ToolCardSummary>, string]> = [
      ["shell", {}, "cargo test"],
      ["file_read", { path: "src/main.rs", command: null, cwd: null, exit_code: null }, "src/main.rs"],
      ["file_write", { path: "out.txt", command: null, cwd: null, exit_code: null, bytes: 1536 }, "out.txt"],
      ["file_edit", { path: "lib.rs", command: null, cwd: null, exit_code: null, added: 8, removed: 3 }, "lib.rs"],
      ["search", { query: "ToolCall", command: null, cwd: null, exit_code: null }, "ToolCall"],
      ["web", { url: "https://example.test/docs", command: null, cwd: null, exit_code: null }, "https://example.test/docs"],
      ["task", { task_title: "Run smoke test", command: null, cwd: null, exit_code: null }, "Run smoke test"],
      ["unknown", { tool_name: "mcp_custom_lookup", command: null, cwd: null, exit_code: null }, "Mcp Custom Lookup"],
    ];

    render(
      <>
        {tools.map(([kind, overrides], index) => (
          <EventCard
            button_id={`event-button-${index}`}
            detail={null}
            detail_error={null}
            detail_loading={false}
            event={event({
              event_key: `event-${index}`,
              type: "tool_call",
              role: null,
              title: "raw_tool_name",
              summary: "raw summary",
              tool: tool({ kind, ...overrides }),
            })}
            is_expanded={false}
            is_selected={false}
            key={kind}
            on_retry_detail={vi.fn()}
            on_select={vi.fn()}
            on_toggle={vi.fn()}
          />
        ))}
      </>,
    );

    for (const [, , primary] of tools) {
      expect(screen.getByText(primary)).toBeInTheDocument();
    }
    expect(screen.getByText("/work/repo")).toBeInTheDocument();
    expect(screen.getByText("1.5 KB")).toBeInTheDocument();
    expect(screen.getByText("+8 −3")).toBeInTheDocument();
  });

  it("reports provider errors as failed even when the exit code is zero", () => {
    renderCard(event({
      type: "tool_call",
      role: null,
      title: "exec_command",
      is_error: true,
      tool: tool({ exit_code: 0 }),
    }));

    expect(screen.getByText("failed")).toHaveAttribute("data-tone", "error");
    expect(screen.queryByText("exit 0")).not.toBeInTheDocument();
  });
});

describe("EventCard subagent activity", () => {
  it("presents a known child and opens it without selecting or expanding the event", () => {
    const onOpenSubagent = vi.fn();
    const onSelect = vi.fn();
    const onToggle = vi.fn();
    const child = subagent();
    renderCard(event({
      type: "agent_activity",
      role: null,
      title: "Agent activity",
      summary: "agent activity completed /root/reviewer",
      agent_activity: {
        kind: "completed",
        event_id: "activity-1",
        target_session_id: child.session_id,
        target_agent_path: child.agent_path,
        target: child,
      },
    }), {
      on_open_related_session: onOpenSubagent,
      on_select: onSelect,
      on_toggle: onToggle,
    });

    expect(screen.getByRole("button", { name: "Subagent: Hubble" })).toHaveAttribute(
      "aria-expanded",
      "false",
    );
    expect(screen.getByText("reviewer · /root/reviewer")).toBeInTheDocument();
    expect(screen.getByText("Completed")).toHaveAttribute("data-tone", "neutral");

    fireEvent.click(screen.getByRole("button", { name: "Open subagent Hubble" }));

    expect(onOpenSubagent).toHaveBeenCalledWith(child);
    expect(onSelect).not.toHaveBeenCalled();
    expect(onToggle).not.toHaveBeenCalled();
  });

  it("keeps an unresolved target inspectable without offering a fabricated navigation action", () => {
    renderCard(event({
      type: "agent_activity",
      role: null,
      title: "Agent activity",
      summary: "agent activity started /root/missing",
      agent_activity: {
        kind: "started",
        event_id: "activity-2",
        target_session_id: "missing-child",
        target_agent_path: "/root/missing",
        target: null,
      },
    }));

    expect(screen.queryByRole("button", { name: /open subagent/i })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Subagent: /root/missing" }));
    expect(screen.getByText(/Child session is not available in this viewer/)).toBeInTheDocument();
    expect(screen.getByText(/Recorded target: missing-child/)).toBeInTheDocument();
  });
});

describe("EventCard whole-turn trajectories", () => {
  it("shows a ticking working duration and stops the timer when unmounted", () => {
    vi.useFakeTimers();
    try {
      vi.setSystemTime(new Date("2026-08-31T00:00:02Z"));
      const { unmount } = renderCard(event({ type: "trajectory", trajectory: trajectory({ status: "working" }) }));
      expect(screen.getByRole("button", { name: "Working for 2s" })).toBeInTheDocument();
      act(() => vi.advanceTimersByTime(1000));
      expect(screen.getByRole("button", { name: "Working for 3s" })).toBeInTheDocument();
      unmount();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not claim unknown historical work is currently running", () => {
    renderCard(event({ type: "trajectory", trajectory: trajectory({ status: "unknown" }) }));
    expect(screen.getByRole("button", { name: "Work for 1h" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Working/ })).not.toBeInTheDocument();
  });

  it("keeps existing child items visible during a live refresh", () => {
    renderCard(event({ type: "trajectory", trajectory: trajectory({ status: "working" }) }), {
      is_expanded: true,
      trajectory_page: trajectoryPage({ is_loading: true, has_loaded: false, events: [event({ summary: "Still visible" })] }),
    });
    expect(screen.getByText("Still visible")).toBeInTheDocument();
    expect(screen.queryByText("Loading turn events…")).not.toBeInTheDocument();
  });

  const trajectoryEvent = event({
    event_key: "trajectory.v1.turn-1",
    type: "trajectory",
    role: null,
    title: "Turn trajectory",
    summary: "Whole turn",
    trajectory: trajectory(),
  });

  it("stays collapsed until explicitly opened, then renders child events as normal timeline cards", () => {
    const onToggle = vi.fn();
    const onToggleChild = vi.fn();
    const onSelect = vi.fn();
    const onOpenSubagent = vi.fn();
    const child = subagent();
    const message = event({
      event_key: "event.v1.turn-message",
      role: "assistant",
      summary: "I checked the source files.",
    });
    const delegation = event({
      event_key: "event.v1.turn-agent",
      type: "agent_activity",
      role: null,
      title: "Agent activity",
      summary: "agent activity interacted /root/reviewer",
      agent_activity: {
        kind: "interacted",
        event_id: "activity-turn-1",
        target_session_id: child.session_id,
        target_agent_path: child.agent_path,
        target: child,
      },
    });
    const nestedTool = event({
      event_key: "event.v1.turn-tool",
      type: "tool_call",
      role: null,
      title: "exec_command",
      summary: "shell exit 0 cargo test",
      tool: tool(),
    });
    const { container, rerender } = renderCard(trajectoryEvent, {
      on_open_related_session: onOpenSubagent,
      on_select: onSelect,
      on_toggle: onToggle,
    });

    const toggle = screen.getByRole("button", { name: "Worked for 1h" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText(message.summary)).not.toBeInTheDocument();

    fireEvent.click(toggle);
    expect(onToggle).toHaveBeenCalledWith(trajectoryEvent.event_key);
    expect(onSelect).not.toHaveBeenCalled();

    rerender(
      <EventCard
        button_id="event-button"
        detail={null}
        detail_error={null}
        detail_loading={false}
        event={trajectoryEvent}
        is_expanded
        is_selected={false}
        on_open_related_session={onOpenSubagent}
        on_retry_detail={vi.fn()}
        on_select={onSelect}
        on_toggle={onToggle}
        on_trajectory_event_toggle={onToggleChild}
        selected_event_key={message.event_key}
        trajectory_page={trajectoryPage({
          events: [message, nestedTool, delegation],
          next_cursor: "next-page",
          previous_cursor: "previous-page",
          total_events: 4,
        })}
      />,
    );

    expect(screen.getByRole("region", { name: "Worked for 1h" })).toBeInTheDocument();
    expect(screen.getByRole("list", { name: "Events in this turn" })).toBeInTheDocument();
    expect(screen.getByText(message.summary)).toBeInTheDocument();
    expect(container.querySelector(".trajectory-section")).toBeInTheDocument();
    expect(container.querySelector(".trajectory-section .message-event")).toBeInTheDocument();
    expect(container.querySelector(".trajectory-section .technical-event")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Ran 1 command, recorded 1 agent activity" }));
    expect(screen.getByRole("button", { name: "Shell: cargo test" })).toBeInTheDocument();
    expect(screen.getByText("Showing 3 of 4 events.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Load earlier events" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Load more events" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Inspect assistant message" }));
    expect(onSelect).toHaveBeenCalledWith(message.event_key);

    fireEvent.click(screen.getByRole("button", { name: "Shell: cargo test" }));
    expect(onToggleChild).toHaveBeenCalledWith(trajectoryEvent.event_key, nestedTool.event_key);

    fireEvent.click(screen.getByRole("button", { name: "Open subagent Hubble" }));
    expect(onOpenSubagent).toHaveBeenCalledWith(child);
  });

  it("retains exploration disclosure state while folding the enclosing work", () => {
    const children = [event({event_key: "read", type: "tool_call", tool: tool({kind: "file_read", path: "a.rs"})}),
      event({event_key: "search", type: "tool_call", tool: tool({kind: "search", query: "state"})})];
    const props = {button_id: "work", event: trajectoryEvent, is_selected: false, detail: null,
      detail_error: null, detail_loading: false, on_retry_detail: vi.fn(), on_select: vi.fn(), on_toggle: vi.fn(),
      trajectory_page: trajectoryPage({events: children})};
    const {rerender} = render(<EventCard {...props} is_expanded />);
    fireEvent.click(screen.getByRole("button", {name: "Read 1 file, performed 1 search"}));
    expect(screen.getByRole("button", {name: "Read: a.rs"})).toBeVisible();
    rerender(<EventCard {...props} is_expanded={false} />);
    expect(screen.queryByRole("button", {name: "Read: a.rs"})).not.toBeInTheDocument();
    rerender(<EventCard {...props} is_expanded />);
    expect(screen.getByRole("button", {name: "Read: a.rs"})).toBeVisible();
  });

  it("retains pagination, errors, and raw counts when every loaded child is filtered", () => {
    const onOlder = vi.fn();
    const onNewer = vi.fn();
    const onRetry = vi.fn();
    const routine = event({ event_key: "event.routine", type: "lifecycle", title: "Turn started",
      role: null, is_bookkeeping: true });
    const page = trajectoryPage({ events: [routine], previous_cursor: "previous-page",
      next_cursor: "next-page", total_events: 50, error: "Child page unavailable" });
    const { rerender } = renderCard(trajectoryEvent, {
      is_expanded: true, hide_lifecycle: true, trajectory_page: page,
      on_trajectory_load_older: onOlder, on_trajectory_load_newer: onNewer,
      on_trajectory_retry: onRetry,
    });
    expect(screen.getByText(/No events match this filter in the loaded turn range/)).toHaveTextContent("1 event hidden");
    expect(screen.queryByText("No events in this turn.")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Turn started" })).not.toBeInTheDocument();
    expect(screen.getByText("Loaded 1 of 50 events.")).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("Child page unavailable");
    fireEvent.click(screen.getByRole("button", { name: "Load earlier events" }));
    fireEvent.click(screen.getByRole("button", { name: "Load more events" }));
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onOlder).toHaveBeenCalledWith(trajectoryEvent.event_key);
    expect(onNewer).toHaveBeenCalledWith(trajectoryEvent.event_key);
    expect(onRetry).toHaveBeenCalledWith(trajectoryEvent.event_key);
    expect(page.events).toEqual([routine]);

    rerender(<EventCard button_id="event-button" event={trajectoryEvent} is_expanded is_selected={false}
      hide_lifecycle detail={null} detail_error={null} detail_loading={false}
      on_retry_detail={vi.fn()} on_select={vi.fn()} on_toggle={vi.fn()}
      on_trajectory_load_older={onOlder} on_trajectory_load_newer={onNewer}
      trajectory_page={{ ...page, error: null, is_loading_older: true }} />);
    expect(screen.getByRole("button", { name: "Loading earlier events…" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Load more events" })).toBeDisabled();
    expect(screen.getByText(/1 event hidden in this loaded turn range/)).toBeInTheDocument();
  });

  it("shows loading and retryable child-page failures without using event detail", () => {
    const onRetry = vi.fn();
    const { rerender } = renderCard(trajectoryEvent, {
      is_expanded: true,
      trajectory_page: null,
    });

    expect(screen.getByRole("status")).toHaveTextContent("Loading turn events");

    rerender(
      <EventCard
        button_id="event-button"
        detail={null}
        detail_error={null}
        detail_loading={false}
        event={trajectoryEvent}
        is_expanded
        is_selected={false}
        on_retry_detail={vi.fn()}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
        on_trajectory_retry={onRetry}
        trajectory_page={trajectoryPage({
          error: "trajectory record was unavailable",
          error_direction: "initial",
        })}
      />,
    );

    expect(screen.getByRole("alert")).toHaveTextContent("trajectory record was unavailable");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledWith(trajectoryEvent.event_key);
  });

  it("formats whole-second durations without Number precision loss", () => {
    expect(formatTrajectoryDuration("9007199254740993")).toBe("104249991d 8h 59m");
    expect(formatTrajectoryDuration("0")).toBe("0s");
    expect(formatTrajectoryDuration("999")).toBe("0s");
    expect(formatTrajectoryDuration("1999")).toBe("1s");
    expect(formatTrajectoryDuration("61234")).toBe("1m 1s");
    expect(formatTrajectoryDuration("not-a-duration")).toBeNull();
  });
});

describe("EventCard tool output", () => {
  const toolEvent = event({
    type: "tool_call",
    role: null,
    title: "exec_command",
    summary: "shell exit 0 cargo test",
    tool: tool(),
  });

  it("expands independently from Inspector and exposes an explicitly labelled region", () => {
    const onSelect = vi.fn();
    const onToggle = vi.fn();
    const { rerender } = renderCard(toolEvent, { on_select: onSelect, on_toggle: onToggle });
    const toggle = screen.getByRole("button", { name: "Shell: cargo test" });

    expect(toggle).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(toggle);
    expect(onToggle).toHaveBeenCalledWith("event.v1.1");
    expect(onSelect).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Inspect cargo test" }));
    expect(onSelect).toHaveBeenCalledWith("event.v1.1");

    rerender(
      <EventCard
        button_id="event-button"
        detail={detail()}
        detail_error={null}
        detail_loading={false}
        event={toolEvent}
        is_expanded
        is_selected={false}
        on_retry_detail={vi.fn()}
        on_select={onSelect}
        on_toggle={onToggle}
      />,
    );
    const region = screen.getByRole("region", { name: "cargo test" });
    expect(toggle.id).toBe("");
    expect(screen.getByRole("button", { name: "Shell: cargo test" })).toHaveAttribute(
      "aria-controls",
      region.id,
    );
    expect(region).toHaveAttribute("aria-labelledby", "event-button-label");
  });

  it("renders output as literal selectable preformatted text, never Markdown or HTML", () => {
    const unsafe = '<img src=x onerror="globalThis.pwned=true">\n**not bold**';
    const { container } = renderCard(toolEvent, {
      detail: detail({
        tool_output: {
          sections: [
            { label: "stdout", text: unsafe, format: "text" },
            { label: "metadata", text: '{"ok":true}', format: "json" },
          ],
          truncated: true,
          original_size_bytes: 70_000,
          source_event_key: "event.v1.2",
        },
      }),
      is_expanded: true,
    });

    const blocks = container.querySelectorAll(".tool-output pre");
    expect(blocks).toHaveLength(2);
    expect(blocks[0]?.textContent).toBe(unsafe);
    expect(blocks[0]).toHaveAttribute("tabindex", "0");
    expect(blocks[0]).toHaveAccessibleName("cargo test stdout output");
    expect(blocks[1]).toHaveAttribute("data-format", "json");
    expect(blocks[1]).toHaveAccessibleName("cargo test metadata output");
    expect(container.querySelector(".tool-output img")).not.toBeInTheDocument();
    expect(container.querySelector(".tool-output strong")).not.toBeInTheDocument();
    expect(screen.getByText(/truncated from 70 KB/i)).toBeInTheDocument();
    expect(screen.getByText(/Inspect the event for the complete bounded detail/i)).toBeInTheDocument();
    expect(screen.queryByText(/related result event/i)).not.toBeInTheDocument();
  });

  it("shows loading, retryable error, pending, and finished empty-output states", () => {
    const onRetry = vi.fn();
    const { rerender } = renderCard(toolEvent, {
      detail_loading: true,
      is_expanded: true,
      on_retry_detail: onRetry,
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading tool output");

    rerender(
      <EventCard
        button_id="event-button"
        detail={null}
        detail_error="bridge disconnected"
        detail_loading={false}
        event={toolEvent}
        is_expanded
        is_selected={false}
        on_retry_detail={onRetry}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
      />,
    );
    expect(screen.getByRole("alert")).toHaveTextContent("bridge disconnected");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledOnce();

    rerender(
      <EventCard
        button_id="event-button"
        detail={detail()}
        detail_error={null}
        detail_loading={false}
        event={{ ...toolEvent, phase: "started" }}
        is_expanded
        is_selected={false}
        on_retry_detail={onRetry}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
      />,
    );
    expect(screen.getByText("Output is not available yet.")).toBeInTheDocument();

    rerender(
      <EventCard
        button_id="event-button"
        detail={detail()}
        detail_error={null}
        detail_loading={false}
        event={toolEvent}
        is_expanded
        is_selected={false}
        on_retry_detail={onRetry}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
      />,
    );
    expect(screen.getByText("No output was captured for this tool call.")).toBeInTheDocument();
  });

  it("does not reveal provider-hidden tool output", () => {
    renderCard({ ...toolEvent, is_hidden: true }, {
      detail: detail({
        tool_output: {
          sections: [{ label: null, text: "secret output", format: "text" }],
          truncated: false,
          original_size_bytes: 13,
          source_event_key: "event.v1.1",
        },
      }),
      is_expanded: true,
    });

    expect(screen.getByText("Tool output is hidden by the provider.")).toBeInTheDocument();
    expect(screen.queryByText("secret output")).not.toBeInTheDocument();
  });
});

describe("EventCard semantic tool operations", () => {
  it("renders Code Execution and Terminal operation headings", () => {
    render(
      <>
        <EventCard
          button_id="code-button"
          detail={null}
          detail_error={null}
          detail_loading={false}
          event={event({
            event_key: "event.code",
            type: "tool_call",
            role: null,
            title: "exec",
            tool: tool({
              kind: "code_execution",
              tool_name: "exec",
              provider_tool_name: "exec",
              language: "javascript",
              command: null,
              cwd: null,
              exit_code: null,
            }),
          })}
          is_expanded={false}
          is_selected={false}
          on_retry_detail={vi.fn()}
          on_select={vi.fn()}
          on_toggle={vi.fn()}
        />
        <EventCard
          button_id="terminal-button"
          detail={null}
          detail_error={null}
          detail_loading={false}
          event={event({
            event_key: "event.terminal",
            type: "tool_call",
            role: null,
            title: "write_stdin",
            tool: tool({
              kind: "terminal",
              tool_name: "write_stdin",
              command: null,
              cwd: null,
              exit_code: null,
              terminal_action: "wait",
              terminal_session_id: "90855",
              wait_ms: 30_000,
              status: "completed",
            }),
          })}
          is_expanded={false}
          is_selected={false}
          on_retry_detail={vi.fn()}
          on_select={vi.fn()}
          on_toggle={vi.fn()}
        />
      </>,
    );

    expect(screen.getByRole("button", { name: "Code: Javascript code" })).toBeInTheDocument();
    expect(screen.getByText("exec")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Terminal: Wait for terminal 90855" }))
      .toBeInTheDocument();
    expect(screen.getByText("Up to 30 s")).toBeInTheDocument();
  });

  it("uses the derived operation status instead of the source record phase", () => {
    const { rerender } = renderCard(event({
      type: "tool_call",
      role: null,
      title: "write_stdin",
      phase: "finished",
      tool: tool({
        kind: "terminal",
        tool_name: "write_stdin",
        command: null,
        cwd: null,
        exit_code: null,
        status: "running",
        terminal_action: "send",
        terminal_session_id: "90855",
        chars_len: 14,
      }),
    }), {
      detail: detail(),
      is_expanded: true,
    });

    expect(screen.getByRole("button", {
      name: "Terminal: Send 14 characters to terminal 90855",
    })).toBeInTheDocument();
    expect(screen.getByText("running")).toHaveAttribute("data-tone", "neutral");
    expect(screen.getByText("Output is not available yet.")).toBeInTheDocument();

    rerender(
      <EventCard
        button_id="event-button"
        detail={detail()}
        detail_error={null}
        detail_loading={false}
        event={event({
          type: "tool_call",
          role: null,
          title: "write_stdin",
          phase: "started",
          tool: tool({
            kind: "terminal",
            tool_name: "write_stdin",
            command: null,
            cwd: null,
            exit_code: null,
            status: "completed",
            terminal_action: "send",
            terminal_session_id: "90855",
            chars_len: 14,
          }),
        })}
        is_expanded
        is_selected={false}
        on_retry_detail={vi.fn()}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
      />,
    );

    expect(screen.queryByText("running")).not.toBeInTheDocument();
    expect(screen.getByText("No output was captured for this tool call.")).toBeInTheDocument();
  });

  it("labels a failed logical operation as failed without relying on an exit code", () => {
    renderCard(event({
      type: "tool_call",
      role: null,
      title: "exec",
      tool: tool({
        kind: "code_execution",
        tool_name: "exec",
        command: null,
        cwd: null,
        exit_code: null,
        status: "failed",
      }),
    }));

    expect(screen.getByText("failed")).toHaveAttribute("data-tone", "error");
  });
});

describe("EventCard usage", () => {
  const usageEvent = event({
    type: "usage",
    role: null,
    title: "Usage",
    summary: "[usage] input=33 output=5 total=38",
    usage: usage(),
  });

  it("shows scope-aware, exact token data without treating subsets as totals", () => {
    renderCard(usageEvent);

    expect(screen.getByRole("button", { name: "Usage: 38 tokens" })).toHaveAttribute(
      "aria-expanded",
      "false",
    );
    expect(screen.getByText("Model call · 33 input · 5 output")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Usage: 38 tokens" }));
    expect(screen.getByRole("region", { name: "38 tokens" })).toBeInTheDocument();
    expect(screen.getByText("Reported total")).toBeInTheDocument();
    expect(screen.getAllByText("38")).toHaveLength(1);
    expect(screen.getByText(/Cache counts are already included in input/i)).toBeInTheDocument();
  });

  it("retains zero values, exact large counters, and the snapshot warning", () => {
    renderCard({
      ...usageEvent,
      usage: usage({
        kind: "session_snapshot",
        input_tokens: "18446744073709551615",
        output_tokens: "0",
        total_tokens: null,
        cache_read_tokens: "0",
        cache_write_tokens: null,
        reasoning_tokens: "0",
      }),
    });

    const button = screen.getByRole("button", {
      name: "Usage: 18,446,744,073,709,551,615 input · 0 output",
    });
    fireEvent.click(button);
    expect(screen.getByText("18,446,744,073,709,551,615")).toBeInTheDocument();
    expect(screen.getAllByText("0")).toHaveLength(3);
    expect(screen.queryByText("Reported total")).not.toBeInTheDocument();
    expect(screen.getByText(/replaces earlier snapshots/i)).toBeInTheDocument();
  });
});

describe("EventCard reasoning", () => {
  const summary = "## Approach\n\n- inspect the source\n- verify the **result**";
  const detailed = "I checked the implementation details.";
  const reasoningEvent = event({
    type: "reasoning",
    role: null,
    title: "Reasoning",
    summary,
    reasoning: reasoning({ preview: "Approach", has_text: true }),
  });

  it("loads reasoning through the controlled detail path and keeps Inspect independent", () => {
    const onToggle = vi.fn();
    const onSelect = vi.fn();
    const { rerender } = renderCard(reasoningEvent, { on_select: onSelect, on_toggle: onToggle });
    const toggle = screen.getByRole("button", { name: "Reasoning: Approach" });

    fireEvent.click(toggle);
    expect(onToggle).toHaveBeenCalledWith("event.v1.1");
    expect(onSelect).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Inspect Approach" }));
    expect(onSelect).toHaveBeenCalledWith("event.v1.1");

    rerender(
      <EventCard
        button_id="event-button"
        detail={detail({
          event: { type: "reasoning", summary, text: detailed },
        })}
        detail_error={null}
        detail_loading={false}
        event={reasoningEvent}
        is_expanded
        is_selected={false}
        on_retry_detail={vi.fn()}
        on_select={onSelect}
        on_toggle={onToggle}
      />,
    );

    expect(screen.getByRole("heading", { name: "Approach" })).toBeInTheDocument();
    expect(screen.getByText("result").tagName).toBe("STRONG");
    expect(screen.getByText(detailed).closest("details")).not.toHaveAttribute("open");
    fireEvent.click(screen.getByText("Detailed reasoning"));
    expect(screen.getByText(detailed).closest("details")).toHaveAttribute("open");
  });

  it("shows deliberate opaque and redacted reasoning states without requesting content", () => {
    const { rerender } = renderCard({
      ...reasoningEvent,
      reasoning: reasoning({
        preview: null,
        has_summary: false,
        has_text: false,
        has_encrypted_content: true,
      }),
    }, { is_expanded: true });
    expect(screen.getByText(/encrypted and cannot be shown inline/i)).toBeInTheDocument();
    expect(screen.queryByText(summary)).not.toBeInTheDocument();

    rerender(
      <EventCard
        button_id="event-button"
        detail={null}
        detail_error={null}
        detail_loading={false}
        event={{
          ...reasoningEvent,
          reasoning: reasoning({
            preview: null,
            has_summary: false,
            has_text: false,
            is_redacted: true,
          }),
        }}
        is_expanded
        is_selected={false}
        on_retry_detail={vi.fn()}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
      />,
    );
    expect(screen.getByText(/redacted by the provider/i)).toBeInTheDocument();
  });

  it("shows loading and a retryable detail failure", () => {
    const onRetry = vi.fn();
    const { rerender } = renderCard(reasoningEvent, {
      detail_loading: true,
      is_expanded: true,
      on_retry_detail: onRetry,
    });
    expect(screen.getByRole("status")).toHaveTextContent("Loading reasoning");

    rerender(
      <EventCard
        button_id="event-button"
        detail={null}
        detail_error="session file is unavailable"
        detail_loading={false}
        event={reasoningEvent}
        is_expanded
        is_selected={false}
        on_retry_detail={onRetry}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
      />,
    );
    expect(screen.getByRole("alert")).toHaveTextContent("session file is unavailable");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });
});
