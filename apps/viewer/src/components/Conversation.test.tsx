import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Conversation } from "./Conversation";
import { event, props, SESSION } from "./conversationTestFixtures";

vi.mock("./SessionComposer", () => ({ SessionComposer: () => null }));

afterEach(cleanup);
beforeEach(() => localStorage.clear());

function toggleFilter() {
  const details = screen.getByRole("button", { name: /^Session details for / });
  if (details.getAttribute("aria-expanded") !== "true") fireEvent.click(details);
  fireEvent.click(screen.getByRole("checkbox", { name: "Show technical events" }));
}

it("jumps to a loaded prompt without opening its inspector or exposing hidden messages", () => {
  const view = props({ events: [
    event("first", { type: "message", role: "user", summary: "Inspect the parser", is_bookkeeping: false }),
    event("private", { type: "message", role: "user", summary: "Hidden extension text", is_hidden: true }),
    event("second", { type: "message", role: "user", summary: "Review the tests", is_bookkeeping: false }),
  ], has_older: true });
  const rendered = render(<Conversation {...view} />);
  fireEvent.click(screen.getByRole("button", { name: "Jump to prompt" }));
  const picker = screen.getByRole("region", { name: "Prompts in this session" });
  expect(picker).not.toHaveTextContent("Hidden extension text");
  fireEvent.click(screen.getByRole("button", { name: "Inspect the parser" }));
  expect(rendered.container.querySelector('[data-reading-slot="first"]')).toHaveFocus();
  expect(screen.queryByRole("region", { name: "Prompts in this session" })).toBeNull();
  expect(view.on_event_select).not.toHaveBeenCalled();
  expect(view.on_follow_change).toHaveBeenCalledWith(false);
  fireEvent.click(screen.getByRole("button", { name: "Jump to prompt" }));
  fireEvent.click(screen.getByRole("button", { name: "Load earlier history" }));
  expect(view.on_load_older).toHaveBeenCalledOnce();
});

it("searches loaded prompts from the keyboard and returns to the same reading line after a live redraw", () => {
  const prompts = ["Inspect the parser", "Review the tests", "Write a follow-up"];
  function generation(revision: number) {
    return prompts.map((summary, i) => event(`generation-${revision}-${i}`, {
      slot_key: String(i), summary, type: "message", role: "user", is_bookkeeping: false,
    }));
  }
  const view = props({ events: generation(1), has_older: true, on_show_live_activity: vi.fn() });
  const rendered = render(<Conversation {...view} />);
  const viewport = screen.getByRole("region", { name: "Conversation history" });
  Object.defineProperties(viewport, { clientHeight: { value: 180 }, scrollHeight: { value: 1200 } });
  vi.spyOn(viewport, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 40, 360, 180));
  let prepend = 0;
  const measure = vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
    if (!this.dataset.readingSlot) return new DOMRect();
    return new DOMRect(0, 40 + Number(this.dataset.readingSlot) * 200 + prepend - viewport.scrollTop, 360, 200);
  });
  try {
    fireEvent.wheel(viewport, { deltaY: -120 });
    viewport.scrollTop = 260;
    fireEvent.scroll(viewport);
    fireEvent.click(screen.getByRole("button", { name: "Jump to prompt" }));
    const panel = within(screen.getByRole("region", { name: "Prompts in this session" }));
    const input = panel.getByRole("searchbox", { name: "Search loaded prompts" });
    expect(input).toHaveFocus();
    expect(panel.getByRole("button", { name: /Review the tests/ })).toHaveAttribute("aria-current", "location");
    viewport.scrollTop = 420;
    fireEvent.scroll(viewport);
    expect(panel.getByRole("button", { name: /Write a follow-up/ })).toHaveAttribute("aria-current", "location");
    viewport.scrollTop = 260;
    fireEvent.scroll(viewport);
    fireEvent.change(input, { target: { value: "no match" } });
    expect(panel.getByRole("status")).toHaveTextContent("No matching prompts");
    expect(panel.getByRole("button", { name: "Load earlier history" })).toBeEnabled();
    fireEvent.change(input, { target: { value: "PARSer" } });
    expect(panel.queryByRole("button", { name: /Review the tests/ })).toBeNull();
    fireEvent.keyDown(input, { key: "ArrowDown" });
    const match = panel.getByRole("button", { name: "Inspect the parser" });
    expect(match).toHaveFocus();
    fireEvent.keyDown(match, { key: "ArrowUp" });
    expect(input).toHaveFocus();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(viewport.scrollTop).toBe(0);
    expect(rendered.container.querySelector('[data-reading-slot="0"]')).toHaveFocus();

    // The provider replaced every event key and inserted history before the saved line.
    prepend = 300;
    rendered.rerender(<Conversation {...view} events={generation(2)} />);
    expect(viewport.scrollTop).toBe(300);
    fireEvent.click(screen.getByRole("button", { name: "Back to reading position" }));
    expect(viewport.scrollTop).toBe(560);
    expect(viewport).toHaveFocus();
    expect(screen.queryByRole("button", { name: "Back to reading position" })).toBeNull();
    expect(view.on_show_live_activity).not.toHaveBeenCalled();
  } finally {
    measure.mockRestore();
  }
});

it("offers a labeled jump to latest from a partial window and returns keyboard focus to history", () => {
  const view = props({ events: [event("Reply", { type: "message", role: "assistant", is_bookkeeping: false })],
    has_newer: true, on_show_live_activity: vi.fn() });
  render(<Conversation {...view} />);
  const viewport = screen.getByRole("region", { name: "Conversation history" });
  const jump = screen.getByRole("button", { name: "Jump to latest" });
  expect(jump).toHaveTextContent("Jump to latest");
  fireEvent.keyDown(viewport, { key: "End" });
  expect(view.on_show_live_activity).toHaveBeenCalledOnce();
  expect(viewport).toHaveFocus();
});

it("steps across prompts at the bottom of the viewport and preserves the original reading line", () => {
  const messages = ["Trace startup", "Inspect the cache", "Try a retry", "Review limits", "Summarize"];
  const events = messages.map((summary, i) => event(String(i), { type: "message", role: "user", summary }));
  events.splice(2, 0, event("hidden", { type: "message", role: "user", summary: "Hidden message", is_hidden: true }));
  const conversation = props({ events });
  const view = render(<Conversation {...conversation} />);
  const viewport = screen.getByRole("region", { name: "Conversation history" });
  let top = 660;
  let prepend = 0;
  Object.defineProperties(viewport, {
    clientHeight: { value: 200 }, scrollHeight: { get: () => 860 + prepend },
    scrollTop: { get: () => top, set: (value: number) => { top = Math.min(660 + prepend, Math.max(0, value)); } },
  });
  vi.spyOn(viewport, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 0, 360, 200));
  const offsets = [30, 300, 740, 780, 800];
  const rows = [...view.container.querySelectorAll<HTMLElement>("[data-reading-prompt]")];
  rows.forEach((row, i) => vi.spyOn(row, "getBoundingClientRect")
    .mockImplementation(() => new DOMRect(0, offsets[i] + prepend - top, 360, 40)));
  fireEvent.wheel(viewport, { deltaY: -80 });
  viewport.scrollTop = 320;
  fireEvent.scroll(viewport);

  const previous = screen.getByRole("button", { name: "Previous prompt" });
  const next = screen.getByRole("button", { name: "Next prompt" });
  const inspect = within(rows[1]).getByRole("button", { name: "Inspect user message" });
  inspect.focus();
  expect(fireEvent.keyDown(inspect, { altKey: true, key: "ArrowDown" })).toBe(true);
  expect(inspect).toHaveFocus();
  expect(top).toBe(320);
  fireEvent.click(previous);
  expect(rows[1]).toHaveFocus();
  fireEvent.keyDown(rows[1], { altKey: true, key: "ArrowUp" });
  expect(rows[0]).toHaveFocus();
  expect(previous).toBeDisabled();
  fireEvent.keyDown(rows[0], { altKey: true, key: "ArrowDown" });
  expect(rows[1]).toHaveFocus();
  // All three final prompts fit below the maximum scrollTop.
  for (const row of rows.slice(2)) {
    fireEvent.click(next);
    expect(row).toHaveFocus();
    expect(top).toBe(660);
  }
  expect(next).toBeDisabled();
  prepend = 100;
  view.rerender(<Conversation {...conversation} events={[...events]} />);
  expect(top).toBe(760);
  fireEvent.click(screen.getByRole("button", { name: "Jump to prompt" }));
  const picker = within(screen.getByRole("region", { name: "Prompts in this session" }));
  expect(picker.getByRole("button", { name: /Summarize/ })).toHaveAttribute("aria-current", "location");
  const search = picker.getByRole("searchbox");
  fireEvent.keyDown(search, { altKey: true, key: "ArrowDown" });
  expect(search).toHaveFocus();
  fireEvent.keyDown(search, { key: "Escape" });
  fireEvent.click(previous);
  expect(rows[3]).toHaveFocus();
  fireEvent.click(screen.getByRole("button", { name: "Back to reading position" }));
  expect(top).toBe(420);
  expect(viewport).toHaveFocus();
  fireEvent.click(screen.getByRole("button", { name: "Jump to latest" }));
  fireEvent.click(previous);
  expect(rows[1]).toHaveFocus();
  fireEvent.click(previous);
  expect(rows[0]).toHaveFocus();
  expect(screen.queryByRole("button", { name: "Back to reading position" })).toBeNull();
  expect(screen.getByRole("button", { name: "Jump to latest" })).toBeVisible();
});

it("holds the reading line when keyboard activation opens a long prompt at the live end", () => {
  const view = props({ events: [event("prompt", {
    type: "message", role: "user", is_bookkeeping: false,
    summary: Array.from({ length: 120 }, (_, i) => `Line ${i + 1} of the prompt`).join("\n"),
  })] });
  const rendered = render(<Conversation {...view} />);
  const viewport = screen.getByRole("region", { name: "Conversation history" });
  const row = rendered.container.querySelector<HTMLElement>("[data-reading-slot]")!;
  const button = screen.getByRole("button", { name: "Read full prompt" });
  const extent = () => button.getAttribute("aria-expanded") === "true" ? 2100 : 700;
  Object.defineProperties(viewport, { clientHeight: { value: 240 }, scrollHeight: { get: extent } });
  vi.spyOn(viewport, "getBoundingClientRect").mockReturnValue(new DOMRect(0, 70, 480, 240));
  vi.spyOn(row, "getBoundingClientRect").mockImplementation(() => new DOMRect(0, 70 - viewport.scrollTop, 480, extent()));
  rendered.rerender(<Conversation {...view} />);
  expect(viewport.scrollTop).toBe(460);
  fireEvent.click(screen.getByRole("button", { name: "Inspect user message" }));
  expect(view.on_follow_change).not.toHaveBeenCalled();
  // Keyboard and assistive-technology activation dispatch a click with no preceding pointer event.
  button.focus();
  fireEvent.click(button, { detail: 0 });
  expect(button).toHaveAttribute("aria-expanded", "true");
  rendered.rerender(<Conversation {...view} />);
  expect(viewport.scrollTop).toBe(460);
  expect(view.on_follow_change).toHaveBeenLastCalledWith(false);
  fireEvent.click(screen.getByRole("button", { name: "Jump to latest" }));
  expect(viewport.scrollTop).toBe(1860);
});

describe("Conversation quick filter", () => {
  it("keeps confirmed model separators visible while lifecycle is hidden", () => {
    const model = (model_id: string) => ({ model_provider: "openai", model_id, display_name: model_id });
    render(<Conversation {...props({ events: [event("change", { type: "model_changed", model_change: { previous: model("old"), current: model("new") } })] })} />);
    expect(screen.getByRole("separator", { name: "Model changed to new" })).toBeVisible();
    expect(screen.getByText("Switched to new")).toBeVisible();
    expect(screen.queryByText(/event hidden/)).not.toBeInTheDocument();
  });
  it("distinguishes a provider switch when both providers use the same model ID", () => {
    render(<Conversation {...props({ events: [event("change", { type: "model_changed", model_change: {
      previous: { model_provider: "before", model_id: "shared", display_name: null },
      current: { model_provider: "after", model_id: "shared", display_name: null },
    } })] })} />);
    expect(screen.getByRole("separator", { name: "Model changed to after/shared" })).toBeVisible();
    expect(screen.getByText("Switched to after/shared")).toHaveAttribute("title", "Previously before/shared");
  });
  it("keeps session metadata behind a disclosure and closes it when switching sessions", () => {
    const view = props({ session: { ...SESSION, project: "Viewer", cwd: "/work/viewer" } });
    const { rerender } = render(<Conversation {...view} />);
    const details = screen.getByRole("button", { name: "Session details for Session title" });
    expect(details).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(details);
    expect(screen.getByRole("region", { name: "Session details" })).toHaveFocus();
    expect(screen.getByText("/work/viewer")).toBeVisible();
    expect(screen.getByText("root")).toBeVisible();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(details).toHaveFocus();
    fireEvent.click(details);
    rerender(<Conversation {...view} session={{ ...SESSION, session_key: "codex:next", project: "Next project" }} />);
    expect(screen.getByRole("button", { name: /^Session details for / })).toHaveAttribute("aria-expanded", "false");
  });
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
  it("scrolls only the history viewport when opening an unanswered question", () => {
    const viewProps = props({ events: [event("question", { type: "question_request", is_bookkeeping: false })] });
    const view = render(<div className="viewer-shell"><Conversation {...viewProps} /></div>);
    const timeline = view.container.querySelector<HTMLDivElement>(".conversation__timeline")!;
    const shell = view.container.querySelector<HTMLDivElement>(".viewer-shell")!;
    const button = screen.getByRole("button", { name: "Questions: question detail" });
    timeline.scrollTop = 120;
    shell.scrollTop = 15;
    Object.defineProperty(timeline, "clientTop", { value: 2 });
    Object.defineProperty(timeline, "clientHeight", { value: 400 });
    Object.defineProperty(timeline, "scrollHeight", { value: 1000 });
    vi.spyOn(timeline, "getBoundingClientRect").mockReturnValue({ top: 100, bottom: 500, height: 400 } as DOMRect);
    vi.spyOn(button, "getBoundingClientRect").mockImplementation(() => ({
      top: 380 - timeline.scrollTop, bottom: 420 - timeline.scrollTop, height: 40,
    } as DOMRect));
    const scrollIntoView = vi.fn(() => { shell.scrollTop = 100; });
    Object.defineProperty(button, "scrollIntoView", { configurable: true, value: scrollIntoView });
    view.rerender(<div className="viewer-shell"><Conversation {...viewProps}
      question_navigation={{ event_key: "question", revision: 1 }} /></div>);
    expect(timeline.scrollTop).toBe(278);
    expect(shell.scrollTop).toBe(15);
    expect(scrollIntoView).not.toHaveBeenCalled();
    expect(button).toHaveFocus();
  });

  it("hides token usage and routine lifecycle while showing errors, unknowns, and meaningful outcomes", () => {
    render(<Conversation {...props({ events: [
      event("Turn started"), event("Context settings", { type: "metadata" }),
      event("Mid-turn usage", { type: "usage" }), event("Final usage", { type: "usage" }),
      event("Unknown provider event", { type: "unknown" }),
      event("Turn failed", { is_error: true }), event("Task outcome", { is_bookkeeping: false }),
    ] })} />);
    fireEvent.click(screen.getByRole("button", { name: /^Session details for / }));
    const technical = screen.getByRole("checkbox", { name: "Show technical events" });
    expect(technical).not.toBeChecked();
    expect(technical).toHaveAccessibleDescription("Includes token usage and routine lifecycle events.");
    expect(screen.queryByRole("button", { name: "Turn started" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Context settings" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Mid-turn usage" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Final usage" })).not.toBeInTheDocument();
    for (const title of ["Unknown provider event", "Turn failed", "Task outcome"]) {
      expect(screen.getByRole("button", { name: title })).toBeInTheDocument();
    }
    expect(screen.getByText("4 events hidden in this loaded range.")).toBeInTheDocument();
    expect(screen.getByText(/10 events/)).toBeInTheDocument();
    toggleFilter();
    expect(technical).toBeChecked();
    expect(screen.getByRole("button", { name: "Turn started" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Mid-turn usage" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Final usage" })).toBeInTheDocument();
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
    expect(screen.getByRole("checkbox", { name: "Show technical events", hidden: true })).toBeChecked();
    expect(screen.getByRole("button", { name: "Second turn" })).toBeInTheDocument();
    toggleFilter();
    expect(screen.queryByRole("button", { name: "Second turn" })).not.toBeInTheDocument();
  });

  it("applies the same toggle to loaded trajectory children without replacing the turn", () => {
    const turn = event("Turn", { type: "trajectory", trajectory: {
      event_count: 20, tool_count: 0, reasoning_count: 0, agent_activity_count: 0,
      error_count: 0, unknown_count: 0, started_at: null, ended_at: null, duration_ms: "1000",
    } });
    const view = props({ events: [turn], expanded_event_key: turn.event_key,
      trajectory_pages: new Map([[SESSION.session_key, new Map([[turn.event_key, {
        events: [event("Nested lifecycle"), event("Nested mid-turn usage", { type: "usage" }),
          event("Nested final usage", { type: "usage" }), event("Nested failure", { type: "error", is_error: true })],
        total_events: 4, has_loaded: true,
        is_loading: false,
        error: null,
      }]])]]) });
    render(<Conversation {...view} />);
    const turnButton = screen.getByRole("button", { name: "Worked for 1s" });
    expect(screen.queryByRole("button", { name: "Recorded 1 event" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Worked for 1s" })).toBe(turnButton);
    expect(turnButton).toHaveAttribute("aria-expanded", "true");
    expect(screen.queryByRole("button", { name: "Nested lifecycle" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Nested mid-turn usage" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Nested final usage" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Nested failure" })).toBeInTheDocument();
    expect(screen.getByText("3 events hidden in this turn.")).toBeInTheDocument();
    expect(screen.queryByText(/Loaded .* of .* events/)).not.toBeInTheDocument();
    expect(view.on_trajectory_event_toggle).not.toHaveBeenCalled();
    expect(view.on_follow_change).not.toHaveBeenCalled();
    toggleFilter();
    expect(screen.queryByRole("button", { name: "Recorded 4 events" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", {name: "Nested lifecycle"})).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Nested final usage" })).toBeInTheDocument();
    expect(screen.getAllByRole("button", {name: /^Worked/})).toHaveLength(1);
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

  it("labels last-good history as stale while its live reader retries", () => {
    const events = [event("Previous event", { type: "message", role: "user", is_bookkeeping: false })];
    const view = render(<Conversation {...props({ events, follow_error: "source read failed" })} />);
    expect(screen.getByRole("alert")).toHaveTextContent("Showing last loaded history while live updates retry: source read failed");
    expect(screen.getByText("Previous event detail")).toBeInTheDocument();
    view.rerender(<Conversation {...props({ events, follow_error: null })} />);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });
});
