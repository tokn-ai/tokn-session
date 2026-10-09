import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { EventDetail, EventSummary } from "../lib/types";
import { EventCard } from "./EventCard";

afterEach(cleanup);

const event: EventSummary = {
  event_key: "event.v1.question", type: "question_request", provider: "codex",
  timestamp: null, phase: "finished", role: null, title: "Questions",
  summary: "Choose storage", summary_truncated: false,
  is_hidden: false, is_error: null, tool: null, usage: null, reasoning: null,
};

function detail(overrides: Partial<EventDetail> = {}): EventDetail {
  return {
    event_key: event.event_key, native: null, tool_output: null, is_hidden: false,
    event: {
      type: "question_request", is_blocking: false,
      text: "Fallback text should not duplicate the structured questions",
      questions: [
        { header: "Storage", question: "Choose **storage**", options: [
          { label: "SQLite (Recommended)", description: "Keep it local" },
          { label: "Postgres", description: null },
        ], allows_free_text: true, is_secret: false },
        { header: null, question: "Any constraints?", options: null, allows_free_text: true, is_secret: true },
      ],
    },
    ...overrides,
  };
}

function card(props: Partial<React.ComponentProps<typeof EventCard>> = {}) {
  return <EventCard event={event} button_id="question-button" is_selected={false}
    is_expanded={true} detail={detail()} detail_error={null} detail_loading={false}
    on_toggle={vi.fn()} on_select={vi.fn()} on_retry_detail={vi.fn()} {...props} />;
}

describe("historical question cards", () => {
  it("renders parsed answers alongside their question without inferring a selected option", () => {
    const reply = { ...event, type: "question_reply", title: "Question answers", summary: "SQLite", role: "user" };
    render(card({ event: reply, detail: detail({ event: {
      type: "question_reply", request_id: "call-1", replies: [
        { question_id: "storage", header: "Storage", question: "Which **storage engine**?", answers: ["SQLite", "Keep it **local**."] },
        { question_id: "extra", header: null, question: null, answers: [] },
      ],
    } }) }));
    expect(screen.getByText("Recorded user answers")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Storage" })).toBeInTheDocument();
    expect(screen.getByText("storage engine").tagName).toBe("STRONG");
    expect(screen.getByText("SQLite", { selector: "p" })).toBeInTheDocument();
    expect(screen.getByText("local").tagName).toBe("STRONG");
    expect(screen.getByRole("heading", { name: "extra" })).toBeInTheDocument();
    expect(screen.getByText("No answer recorded")).toBeInTheDocument();
    expect(screen.queryByRole("radio")).not.toBeInTheDocument();
  });

  it("shows empty response maps without claiming a choice was made", () => {
    render(card({ event: { ...event, type: "question_reply" }, detail: detail({ event: { type: "question_reply", replies: [] } }) }));
    expect(screen.getByText("No answers recorded")).toBeInTheDocument();
  });
  it("shows questions and choices as recorded content without answer controls or completion status", () => {
    render(card());
    expect(screen.getByText("Recorded asynchronous question request")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Storage" })).toBeInTheDocument();
    expect(screen.getByText("storage").tagName).toBe("STRONG");
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
    expect(screen.getByText("SQLite (Recommended)")).toBeInTheDocument();
    expect(screen.getByText(/Keep it local/)).toBeInTheDocument();
    expect(screen.getByText("Any constraints?")).toBeInTheDocument();
    expect(screen.getByText("Secret answer requested")).toBeInTheDocument();
    expect(screen.queryByRole("radio")).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    expect(screen.queryByText("finished")).not.toBeInTheDocument();
    expect(screen.queryByText(/Fallback text/)).not.toBeInTheDocument();
  });

  it("uses shared expansion and ignores detail from another event", () => {
    const on_toggle = vi.fn();
    const { rerender } = render(card({ is_expanded: false, on_toggle }));
    fireEvent.click(screen.getByRole("button", { name: "Questions: Choose storage" }));
    expect(on_toggle).toHaveBeenCalledWith(event.event_key);
    rerender(card({ detail: detail({ event_key: "event.v1.other" }), detail_loading: true }));
    expect(screen.getByRole("status")).toHaveTextContent("Loading questions");
    expect(screen.queryByText("SQLite (Recommended)")).not.toBeInTheDocument();
  });

  it("shows retry, refresh errors, and detail truncation", () => {
    const on_retry_detail = vi.fn();
    const { rerender } = render(card({ detail: null, detail_error: "Snapshot changed", on_retry_detail }));
    expect(screen.getByRole("alert")).toHaveTextContent("Snapshot changed");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(on_retry_detail).toHaveBeenCalledOnce();
    rerender(card({ detail_error: "Refresh failed" }));
    expect(screen.getByText("SQLite (Recommended)")).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("Refresh failed");
    rerender(card({ detail: detail({ event: { truncated: true } }) }));
    expect(screen.getByRole("status")).toHaveTextContent("detail size limit");
  });

  it("honors hidden content", () => {
    render(card({ detail: detail({ is_hidden: true }) }));
    expect(screen.getByText("Questions are hidden by the provider.")).toBeInTheDocument();
    expect(screen.queryByText("SQLite (Recommended)")).not.toBeInTheDocument();
  });

  it("does not invent a blocking mode when only a historical tool invocation was recorded", () => {
    render(card({ detail: detail({ event: {
      type: "question_request", is_blocking: null, questions: [{ question: "Choose storage", options: null }],
    } }) }));
    expect(screen.queryByText("Recorded blocking question request")).not.toBeInTheDocument();
    expect(screen.queryByText("Recorded asynchronous question request")).not.toBeInTheDocument();
    expect(screen.getByText("Choose storage", { selector: "p" })).toBeInTheDocument();
  });
});
