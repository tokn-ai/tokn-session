import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentActivityCardSummary, EventDetail, EventSummary, SessionSummary } from "../lib/types";
import { EventCard } from "./EventCard";
import { Inspector } from "./Inspector";

afterEach(cleanup);

function communication(overrides: Partial<AgentActivityCardSummary> = {}): EventSummary {
  return {
    event_key: "event.v1.communication", type: "agent_activity", provider: "codex",
    timestamp: null, phase: "finished", role: null, title: "Agent message",
    summary: "routing metadata only", summary_truncated: false, is_hidden: false,
    is_error: false, tool: null, usage: null, reasoning: null,
    agent_activity: {
      kind: "message", event_id: "delivery-1", actor_session_id: null,
      actor_agent_path: "/root/reviewer", actor: null, target_session_id: null,
      target_agent_path: "/root", target: null,
      communication: { has_text: true, has_encrypted_content: false, trigger_turn: true },
      ...overrides,
    },
  };
}

function detail(overrides: Partial<EventDetail> = {}): EventDetail {
  return {
    event_key: "event.v1.communication", is_hidden: false, native: null, tool_output: null,
    event: {
      type: "agent_activity", communication: {
        text: "## Review result\n\nA **complete** answer with `code`.\n\n- First item\n- Second item",
        has_encrypted_content: false, trigger_turn: true,
      },
    },
    ...overrides,
  };
}

function card(props: Partial<React.ComponentProps<typeof EventCard>> = {}) {
  return <EventCard event={communication()} button_id="communication-button"
    detail={null} detail_error={null} detail_loading={false} is_expanded={false}
    is_selected={false} on_retry_detail={vi.fn()} on_select={vi.fn()} on_toggle={vi.fn()}
    {...props} />;
}

describe("agent communication cards", () => {
  it("expands through the shared detail flow and keeps routing metadata out of the body", () => {
    const onToggle = vi.fn();
    const { rerender } = render(card({ on_toggle: onToggle }));
    expect(screen.getByText("Starts a turn")).toBeInTheDocument();
    expect(screen.queryByText("routing metadata only")).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Review result" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Message from /root/reviewer → /root" }));
    expect(onToggle).toHaveBeenCalledWith("event.v1.communication");

    rerender(card({ is_expanded: true, detail: detail() }));
    expect(screen.getByRole("heading", { name: "Review result" })).toBeInTheDocument();
    expect(screen.getByText("complete").tagName).toBe("STRONG");
    expect(screen.getByText("code").tagName).toBe("CODE");
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
  });

  it("shows loading, retry, and the recovered message", () => {
    const onRetry = vi.fn();
    const { rerender } = render(card({ is_expanded: true, detail_loading: true }));
    expect(screen.getByRole("status")).toHaveTextContent("Loading message…");
    rerender(card({ is_expanded: true, detail_error: "Snapshot changed", on_retry_detail: onRetry }));
    expect(screen.getByRole("alert")).toHaveTextContent("Snapshot changed");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledOnce();
    rerender(card({ is_expanded: true, detail: detail() }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Review result" })).toBeInTheDocument();
  });

  it("explains encrypted-only content without requiring native payloads", () => {
    render(card({ is_expanded: true, event: communication({
      communication: { has_text: false, has_encrypted_content: true, trigger_turn: false },
    }) }));
    expect(screen.getByText("Encrypted message body")).toBeInTheDocument();
    expect(screen.getByText("Does not start a turn")).toBeInTheDocument();
    expect(screen.queryByText("Loading message…")).not.toBeInTheDocument();
    expect(screen.queryByText("No readable message body was recorded.")).not.toBeInTheDocument();
  });

  it("keeps the loaded message mounted during refresh and retryable errors", () => {
    const onRetry = vi.fn();
    const { rerender } = render(card({ is_expanded: true, detail: detail() }));
    const heading = screen.getByRole("heading", { name: "Review result" });

    rerender(card({ is_expanded: true, detail: detail(), detail_loading: true }));
    expect(screen.getByRole("heading", { name: "Review result" })).toBe(heading);
    expect(heading.closest(".communication-card")).toHaveAttribute("aria-busy", "true");
    expect(screen.queryByText("Loading message…")).not.toBeInTheDocument();

    rerender(card({ is_expanded: true, detail: detail(), detail_error: "Connection lost",
      on_retry_detail: onRetry }));
    expect(screen.getByRole("heading", { name: "Review result" })).toBe(heading);
    expect(screen.getByRole("alert")).toHaveTextContent("Could not refresh: Connection lost");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledOnce();

    rerender(card({ is_expanded: true, detail: detail(), detail_loading: true }));
    expect(screen.getByRole("heading", { name: "Review result" })).toBe(heading);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("does not retain another event's body when a detail request fails", () => {
    render(card({ is_expanded: true, detail: detail({ event_key: "event.v1.previous" }),
      detail_error: "Connection lost" }));
    expect(screen.queryByRole("heading", { name: "Review result" })).not.toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("Message unavailable");
    expect(screen.queryByText(/Could not refresh/)).not.toBeInTheDocument();
  });

  it("renders readable content and an encrypted notice for mixed content", () => {
    render(card({ is_expanded: true, event: communication({
      communication: { has_text: true, has_encrypted_content: true, trigger_turn: null },
    }), detail: detail({ native: { encrypted_content: "ciphertext-must-stay-out-of-card" } }) }));
    expect(screen.getByRole("heading", { name: "Review result" })).toBeInTheDocument();
    expect(screen.getByText("Encrypted message body")).toBeInTheDocument();
    expect(screen.queryByText(/ciphertext-must-stay-out-of-card/)).not.toBeInTheDocument();
    expect(screen.queryByText(/Starts a turn|Does not start a turn/)).not.toBeInTheDocument();
  });

  it.each(["summary", "detail"])("does not render %s-hidden content", (hiddenSource) => {
    render(card({ is_expanded: true,
      event: { ...communication(), is_hidden: hiddenSource === "summary" },
      detail: detail({ is_hidden: hiddenSource === "detail" }),
    }));
    expect(screen.getByText("Message content is hidden by the provider.")).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Review result" })).not.toBeInTheDocument();
  });

  it("does not render another event's stale detail", () => {
    render(card({ is_expanded: true, detail: detail({ event_key: "event.v1.previous" }) }));
    expect(screen.queryByRole("heading", { name: "Review result" })).not.toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Loading message…");
  });

  it("does not confuse a routing header with a readable body", () => {
    render(card({ is_expanded: true, event: communication({
      communication: { has_text: false, has_encrypted_content: false, trigger_turn: null },
    }), detail: detail({ event: { type: "agent_activity", communication: { text: " \n " } } }) }));
    expect(screen.getByText("No readable message body was recorded.")).toBeInTheDocument();
    expect(screen.queryByText("routing metadata only")).not.toBeInTheDocument();
  });

  it("explains when the readable message exceeds the bounded detail size", () => {
    render(card({ is_expanded: true, detail: detail({ event: {
      truncated: true, original_size_bytes: 600000, representation: "normalized_event",
    } }) }));
    expect(screen.getByRole("status")).toHaveTextContent("Message content exceeds the viewer’s detail size limit.");
    expect(screen.queryByText("No readable message body was recorded.")).not.toBeInTheDocument();
  });

  it("opens only the verified sender, even when a different recipient is resolved", () => {
    const actor: SessionSummary = {
      session_key: "codex:sender", session_id: "sender", parent_session_id: "root",
      is_subagent: true, provider: "codex", title: "Review task", preview: null,
      project: null, cwd: null, updated_at_ms: null, timestamp: null,
      agent_path: "/root/reviewer", agent_nickname: null, agent_role: null,
      child_count: 0, message_count: null, event_count: null, history_status: null,
      has_unread: false,
    };
    const onOpen = vi.fn();
    const { rerender } = render(card({ event: communication({ actor,
      target: { ...actor, session_key: "codex:other-child", title: "Other task" },
    }), on_open_related_session: onOpen }));
    fireEvent.click(screen.getByRole("button", { name: "Open sender Review task" }));
    expect(onOpen).toHaveBeenCalledWith(actor);
    expect(screen.queryByRole("button", { name: "Open subagent Other task" })).not.toBeInTheDocument();
    rerender(card({ event: communication({ actor: null, target: actor }), on_open_related_session: onOpen }));
    expect(screen.queryByRole("button", { name: /Open/ })).not.toBeInTheDocument();
  });

  it("uses IDs or explicit unknown labels when paths are unavailable", () => {
    const { rerender } = render(card({ event: communication({
      actor_agent_path: null, actor_session_id: "sender-123", target_agent_path: null,
    }) }));
    expect(screen.getByRole("button", { name: "Message from sender-123 → Unknown recipient" })).toBeInTheDocument();
    rerender(card({ event: communication({ actor_agent_path: null, target_agent_path: null }) }));
    expect(screen.getByRole("button", { name: "Message from Unknown sender → Unknown recipient" })).toBeInTheDocument();
  });

  it("renders loaded nested communication and routes its expansion through the trajectory", () => {
    const onToggle = vi.fn();
    render(card({
      event: { ...communication(), event_key: "trajectory.v1.turn", type: "trajectory",
        agent_activity: null, trajectory: {
          event_count: 1, tool_count: 0, reasoning_count: 0, agent_activity_count: 1,
          error_count: 0, unknown_count: 0, started_at: null, ended_at: null, duration_ms: null,
        } },
      is_expanded: true,
      trajectory_page: { events: [communication()], total_events: 1, has_loaded: true, is_loading: false, error: null },
      trajectory_expanded_event_key: "event.v1.communication",
      trajectory_expanded_detail: detail(), on_trajectory_event_toggle: onToggle,
    }));
    expect(screen.getByRole("heading", { name: "Review result" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Message from /root/reviewer → /root" }));
    expect(onToggle).toHaveBeenCalledWith("trajectory.v1.turn", "event.v1.communication");
  });

  it("renders communication body Markdown in Inspector without native data", () => {
    render(<Inspector detail={detail()} error={null} event={communication()} is_loading={false}
      is_open on_close={vi.fn()} on_retry={vi.fn()} />);
    expect(screen.getByRole("tab", { name: "Content" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("heading", { name: "Review result" })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Native" })).toBeDisabled();
  });
});
