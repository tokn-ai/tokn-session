import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { EventSummary, SessionSummary } from "../lib/types";
import { Conversation } from "./Conversation";

vi.mock("./SessionComposer", () => ({ SessionComposer: () => null }));

afterEach(cleanup);

const SESSION: SessionSummary = {
  session_key: "codex:root", session_id: "root", parent_session_id: null,
  is_subagent: false, provider: "codex", title: "Session title", preview: null,
  project: null, cwd: null, updated_at_ms: null, timestamp: null,
  agent_path: null, agent_nickname: null, agent_role: null, child_count: 0,
  message_count: null, event_count: null, history_status: null, has_unread: false,
};

function event(key: string, overrides: Partial<EventSummary> = {}): EventSummary {
  return {
    event_key: key, type: "lifecycle", provider: "codex", timestamp: null,
    phase: null, role: null, title: key, summary: `${key} detail`,
    summary_truncated: false, is_hidden: false, is_error: false,
    tool: null, usage: null, reasoning: null, is_bookkeeping: true,
    ...overrides,
  };
}

function props(overrides: Partial<React.ComponentProps<typeof Conversation>> = {}): React.ComponentProps<typeof Conversation> {
  return {
    session: SESSION, events: [], selected_event_key: null, expanded_event_key: null,
    expanded_detail: null, expanded_detail_error: null, expanded_detail_loading: false,
    initial_page_loaded: true, is_loading: false, is_loading_older: false,
    is_loading_newer: false, error: null, has_older: false, has_newer: false,
    total_events: 10, history_status: null, inspector_open: false,
    on_sidebar_open: vi.fn(), on_inspector_toggle: vi.fn(), on_event_select: vi.fn(),
    on_event_toggle: vi.fn(), trajectory_pages: new Map(), trajectory_expanded_key: null,
    trajectory_expanded_event_key: null, trajectory_expanded_detail: null,
    trajectory_expanded_detail_error: null, trajectory_expanded_detail_loading: false,
    on_trajectory_load_older: vi.fn(), on_trajectory_load_newer: vi.fn(),
    on_trajectory_retry: vi.fn(), on_trajectory_event_toggle: vi.fn(),
    on_trajectory_retry_expanded_detail: vi.fn(), on_open_related_session: vi.fn(),
    on_load_older: vi.fn(), on_load_newer: vi.fn(), on_retry: vi.fn(),
    on_retry_expanded_detail: vi.fn(), on_follow_change: vi.fn(),
    ...overrides,
  };
}

function toggleFilter() {
  fireEvent.click(screen.getByRole("button", { name: /^(Hide|Show) lifecycle$/ }));
}

describe("Conversation quick filter", () => {
  it("shows separate blocking and available question notices and opens their cards", () => {
    const on_question_open = vi.fn();
    const view = render(<Conversation {...props({ on_question_open, outstanding_questions: [
      { event_key: "blocking", requires_input: true, unanswered_count: 1 },
      { event_key: "async", requires_input: false, unanswered_count: 2 },
    ] })} />);
    fireEvent.click(screen.getByRole("button", { name: /Input required/ }));
    expect(on_question_open).toHaveBeenCalledWith("blocking");
    fireEvent.click(screen.getByRole("button", { name: /Question available/ }));
    expect(on_question_open).toHaveBeenCalledWith("async");
    view.rerender(<Conversation {...props({ outstanding_questions: [] })} />);
    expect(screen.queryByRole("status", { name: "Unanswered questions" })).not.toBeInTheDocument();
  });

  it("focuses the outstanding question when navigation arrives", () => {
    render(<Conversation {...props({ events: [event("question", { type: "question_request", is_bookkeeping: false })],
      question_navigation: { event_key: "question", revision: 1 },
    })} />);
    expect(screen.getByRole("button", { name: "Questions: question detail" })).toHaveFocus();
  });
  it("hides intermediate usage and routine lifecycle while keeping final usage, errors, unknowns, and meaningful outcomes", () => {
    render(<Conversation {...props({ events: [
      event("Turn started"), event("Context settings", { type: "metadata" }),
      event("Mid-turn usage", { type: "usage" }), event("Final usage", { type: "usage", is_bookkeeping: false }),
      event("Unknown provider event", { type: "unknown" }),
      event("Turn failed", { is_error: true }), event("Task outcome", { is_bookkeeping: false }),
    ] })} />);
    expect(screen.getByRole("button", { name: "Hide lifecycle" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "Turn started" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Mid-turn usage" })).toBeInTheDocument();
    toggleFilter();
    expect(screen.getByRole("button", { name: "Show lifecycle" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.queryByRole("button", { name: "Turn started" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Context settings" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Mid-turn usage" })).not.toBeInTheDocument();
    for (const title of ["Final usage", "Unknown provider event", "Turn failed", "Task outcome"]) {
      expect(screen.getByRole("button", { name: title })).toBeInTheDocument();
    }
    expect(screen.getByText("3 events hidden in this loaded range.")).toBeInTheDocument();
    expect(screen.getByText(/10 events/)).toBeInTheDocument();
    toggleFilter();
    expect(screen.getByRole("button", { name: "Hide lifecycle" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "Turn started" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Mid-turn usage" })).toBeInTheDocument();
    expect(screen.queryByText(/events hidden/)).not.toBeInTheDocument();
  });

  it("retains unclassified older-server records and mounted response content", () => {
    const view = props({ events: [event("Old lifecycle", { is_bookkeeping: undefined }),
      event("Response", { type: "message", role: "assistant", summary: "A **meaningful** response" })] });
    render(<Conversation {...view} />);
    const response = screen.getByText("meaningful");
    toggleFilter();
    expect(screen.getByRole("button", { name: "Old lifecycle" })).toBeInTheDocument();
    expect(screen.getByText("meaningful")).toBe(response);
    expect(view.on_follow_change).not.toHaveBeenCalled();
  });

  it("keeps paging and Inspector controls when every loaded event is filtered", () => {
    const view = props({ events: [event("Turn started"), event("Turn finished")],
      total_events: 250, has_older: true, has_newer: true, selected_event_key: "Turn started",
      inspector_open: true });
    render(<Conversation {...view} />);
    toggleFilter();
    expect(screen.getByText(/No events match this filter in the loaded range/)).toHaveTextContent("2 events hidden");
    expect(screen.queryByText("No events in this session")).not.toBeInTheDocument();
    expect(screen.getByText(/250 events/)).toBeInTheDocument();
    expect(view.on_event_select).not.toHaveBeenCalled();
    expect(view.on_event_toggle).not.toHaveBeenCalled();
    expect(view.on_follow_change).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Close event inspector" }));
    expect(view.on_inspector_toggle).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Load earlier turns" }));
    fireEvent.click(screen.getByRole("button", { name: "Load newer events" }));
    expect(view.on_load_older).toHaveBeenCalledOnce();
    expect(view.on_load_newer).toHaveBeenCalledOnce();
  });

  it("keeps its selection across sessions and filters newly loaded activity", () => {
    const { rerender } = render(<Conversation {...props({ events: [event("First turn")] })} />);
    toggleFilter();
    rerender(<Conversation {...props({ session: { ...SESSION, session_key: "codex:next", session_id: "next" },
      events: [event("Second turn")] })} />);
    expect(screen.getByRole("button", { name: "Show lifecycle" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.queryByRole("button", { name: "Second turn" })).not.toBeInTheDocument();
    expect(screen.getByText(/1 event hidden in this loaded range/)).toBeInTheDocument();
  });

  it("applies the same toggle to loaded trajectory children without replacing the turn", () => {
    const turn = event("Turn", { type: "trajectory", trajectory: {
      event_count: 20, tool_count: 0, reasoning_count: 0, agent_activity_count: 0,
      error_count: 0, unknown_count: 0, started_at: null, ended_at: null, duration_ms: "1000",
    } });
    const view = props({ events: [turn], expanded_event_key: turn.event_key,
      trajectory_pages: new Map([[SESSION.session_key, new Map([[turn.event_key, {
        events: [event("Nested lifecycle"), event("Nested mid-turn usage", { type: "usage" }),
          event("Nested final usage", { type: "usage", is_bookkeeping: false })],
        next_cursor: null, previous_cursor: "earlier", total_events: 20, has_loaded: true,
        is_loading: false, is_loading_older: false, is_loading_newer: false,
        error: null, error_direction: null, error_cursor: null,
      }]])]]) });
    render(<Conversation {...view} />);
    const turnButton = screen.getByRole("button", { name: "Worked for 1s" });
    expect(screen.getByRole("button", { name: "Nested lifecycle" })).toBeInTheDocument();
    toggleFilter();
    expect(screen.getByRole("button", { name: "Worked for 1s" })).toBe(turnButton);
    expect(turnButton).toHaveAttribute("aria-expanded", "true");
    expect(screen.queryByRole("button", { name: "Nested lifecycle" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Nested mid-turn usage" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Nested final usage" })).toBeInTheDocument();
    expect(screen.getByText("2 events hidden in this loaded turn range.")).toBeInTheDocument();
    expect(screen.getByText("Loaded 3 of 20 events.")).toBeInTheDocument();
    expect(view.on_trajectory_event_toggle).not.toHaveBeenCalled();
    expect(view.on_trajectory_load_older).not.toHaveBeenCalled();
    expect(view.on_follow_change).not.toHaveBeenCalled();
  });

  it("preserves actual empty, loading, and error states", () => {
    const { rerender } = render(<Conversation {...props({ is_loading: true, initial_page_loaded: false })} />);
    toggleFilter();
    expect(screen.queryByText(/No events match/)).not.toBeInTheDocument();
    rerender(<Conversation {...props({ error: "Session unavailable" })} />);
    expect(screen.getByText("Conversation unavailable")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Try again" })).toBeInTheDocument();
    rerender(<Conversation {...props()} />);
    expect(screen.getByText("No events in this session")).toBeInTheDocument();
    expect(screen.queryByText(/No events match/)).not.toBeInTheDocument();
  });
});
