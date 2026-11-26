import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { EventDetail, EventSummary } from "../lib/types";
import { Inspector } from "./Inspector";
import { ViewerVisibility } from "../lib/viewerVisibility";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

const EVENT: EventSummary = {
  event_key: "event.v1.2",
  type: "message",
  provider: "codex",
  timestamp: "2026-08-31T00:00:00Z",
  phase: "finished",
  role: "assistant",
  title: "Assistant message",
  summary: "preview",
  summary_truncated: true,
  is_hidden: false,
  is_error: false,
  tool: null,
  usage: null,
  reasoning: null,
};

const DETAIL: EventDetail = {
  event_key: EVENT.event_key,
  event: {
    type: "message",
    provider: "codex",
    role: "assistant",
    text: "# Full result\n\nThe **complete** message.",
  },
  native: null,
  is_hidden: false,
  tool_output: null,
};

const base = { detail: DETAIL, event: EVENT, error: null, is_open: true, is_loading: false, on_close: vi.fn(), on_retry: vi.fn() };

it("gates content and copying on the summary revision, then recovers with the matching payload", () => {
  vi.stubGlobal("navigator", { clipboard: { writeText: vi.fn() } });
  const retry = vi.fn();
  const props = { ...base, on_retry: retry, event: { ...EVENT, content_revision: "new" } };
  const detail = { ...DETAIL, native: { original: true }, content_revision: "old" };
  const { rerender } = render(<Inspector {...props} detail={detail} />);
  expect(screen.queryByRole("heading", { name: "Full result" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /Copy (native|normalized) JSON/ })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Try again" }));
  expect(retry).toHaveBeenCalledOnce();
  rerender(<Inspector {...props} detail={{ ...detail, content_revision: "new" }} />);
  expect(screen.getByRole("heading", { name: "Full result" })).toBeVisible();
});

it("compares the same representation across events, resets its scroll, and keeps desktop focus on the timeline", async () => {
  const writeText = vi.fn().mockResolvedValue(undefined);
  vi.stubGlobal("navigator", { clipboard: { writeText } });
  const first = { ...DETAIL, native: { id: "first" } };
  const second = { ...first, event_key: "another-event", native: { id: "second" } };
  const tree = (event: EventSummary, detail: EventDetail | null) => <>
    <button type="button">Inspect next message</button><Inspector {...base} event={event} detail={detail} />
  </>;
  const { rerender } = render(tree(EVENT, first));
  expect(screen.getByRole("button", { name: "Close inspector" })).toHaveFocus();
  fireEvent.keyDown(screen.getByRole("tab", { name: "Content" }), { key: "End" });
  const native = screen.getByRole("tab", { name: "Native" });
  expect(native).toHaveFocus();
  const panel = screen.getByRole("tabpanel", { name: "Native" });
  expect(panel).toHaveAttribute("tabindex", "0");
  panel.scrollTop = 67;
  const trigger = screen.getByRole("button", { name: "Inspect next message" });
  trigger.focus();
  const nextEvent = { ...EVENT, event_key: second.event_key };
  rerender(tree(nextEvent, first));
  expect(screen.queryByRole("button", { name: "Copy native JSON" })).not.toBeInTheDocument();
  expect(panel).not.toHaveTextContent("first");
  rerender(tree(nextEvent, second));
  expect(native).toHaveAttribute("aria-selected", "true");
  expect(panel.scrollTop).toBe(0);
  expect(trigger).toHaveFocus();
  await act(async () => fireEvent.click(screen.getByRole("button", { name: "Copy native JSON" })));
  expect(writeText).toHaveBeenCalledExactlyOnceWith(JSON.stringify(second.native, null, 2));
  rerender(tree({ ...nextEvent, is_hidden: true }, second));
  expect(native).toBeDisabled();
  expect(screen.getByRole("tab", { name: "Normalized" })).toHaveAttribute("aria-selected", "true");
  expect(panel).not.toHaveTextContent('"id": "second"');
  expect(JSON.parse(panel.textContent!)).toEqual({ type: "message", provider: "codex", redacted: true });
});

it("opens a native dialog on phones and releases its modal surface when hidden, dismissed or resized", () => {
  let matches = true;
  let resize = () => {};
  vi.stubGlobal("matchMedia", () => ({
    get matches() { return matches; },
    addEventListener: (_: string, listener: () => void) => { resize = listener; }, removeEventListener: vi.fn(),
  }));
  const close = vi.fn();
  const tree = (is_open: boolean, visible = true) => <ViewerVisibility.Provider value={visible}>
    <Inspector {...base} is_open={is_open} on_close={close} />
  </ViewerVisibility.Provider>;
  const { rerender } = render(tree(false));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  rerender(tree(true));
  const dialog = screen.getByRole("dialog", { name: "Event inspector" });
  expect(dialog).toHaveAttribute("open");
  expect(screen.getByRole("button", { name: "Close inspector" })).toHaveFocus();
  rerender(tree(true, false));
  expect(dialog).not.toHaveAttribute("open");
  rerender(tree(true));
  fireEvent(dialog, new Event("cancel", { cancelable: true }));
  expect(close).toHaveBeenCalledOnce();
  rerender(tree(false));
  expect(dialog).not.toHaveAttribute("open");
  rerender(tree(true));
  act(() => { matches = false; resize(); });
  expect(screen.getByRole("complementary", { name: "Event inspector" })).toBeVisible();
  expect(dialog).not.toHaveAttribute("open");
});

describe("Inspector Markdown routing", () => {
  it("shows the complete native provider error separately from its normalized shape", () => {
    const native = {
      type: "message",
      id: "native-assistant-42",
      parentId: "native-user-41",
      message: {
        role: "assistant",
        stopReason: "error",
        errorMessage: "Connection error: provider request failed",
        providerData: { request_id: "provider-request-7", status: 503 },
      },
    };
    render(
      <Inspector
        detail={{
          event_key: "event.v1.error",
          event: { type: "error", provider: "pi", message: "Connection error" },
          native,
          is_hidden: false,
          tool_output: null,
        }}
        error={null}
        event={{ ...EVENT, event_key: "event.v1.error", type: "error", provider: "pi", role: null,
          title: "Error", summary: "Connection error", summary_truncated: false, is_error: true }}
        is_loading={false}
        is_open
        on_close={vi.fn()}
        on_retry={vi.fn()}
      />,
    );

    const panel = screen.getByRole("tabpanel");
    expect(screen.getByRole("tab", { name: "Native" })).toBeEnabled();
    fireEvent.click(screen.getByRole("tab", { name: "Native" }));
    expect(panel).toHaveTextContent("Connection error: provider request failed");
    expect(panel).toHaveTextContent("native-assistant-42");
    expect(panel).toHaveTextContent("native-user-41");
    expect(panel).toHaveTextContent("provider-request-7");
    fireEvent.click(screen.getByRole("tab", { name: "Normalized" }));
    expect(JSON.parse(panel.textContent!)).toEqual({ type: "error", provider: "pi", message: "Connection error" });
    expect(panel).not.toHaveTextContent("native-assistant-42");
    expect(panel).not.toHaveTextContent("provider-request-7");
  });

  it("keeps visible detail mounted during refresh and refresh failure", () => {
    const props = {
      detail: DETAIL, error: null, event: EVENT, is_loading: false, is_open: true,
      on_close: vi.fn(), on_retry: vi.fn(),
    };
    const { rerender } = render(<Inspector {...props} />);
    const content = screen.getByRole("heading", { name: "Full result" });
    rerender(<Inspector {...props} is_loading />);
    expect(screen.getByRole("heading", { name: "Full result" })).toBe(content);
    expect(screen.queryByText("Loading event detail…")).not.toBeInTheDocument();
    rerender(<Inspector {...props} error="temporary failure" />);
    expect(screen.getByRole("heading", { name: "Full result" })).toBe(content);
    expect(screen.getByRole("alert")).toHaveTextContent("Could not refresh detail");
  });

  it("renders readable message detail as Markdown in the Content tab", () => {
    render(
      <Inspector
        detail={DETAIL}
        error={null}
        event={EVENT}
        is_loading={false}
        is_open
        on_close={vi.fn()}
        on_retry={vi.fn()}
      />,
    );

    expect(screen.getByRole("tab", { name: "Content" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("heading", { name: "Full result" })).toBeInTheDocument();
    expect(screen.getByText("complete").tagName).toBe("STRONG");
  });

  it("does not expose hidden detail as readable Markdown", () => {
    render(
      <Inspector
        detail={{
          event_key: EVENT.event_key,
          event: { type: "message", provider: "pi", redacted: true },
          native: null,
          is_hidden: true,
          tool_output: null,
        }}
        error={null}
        event={{ ...EVENT, provider: "pi", is_hidden: true }}
        is_loading={false}
        is_open
        on_close={vi.fn()}
        on_retry={vi.fn()}
      />,
    );

    expect(screen.getByText("Content hidden by provider")).toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: "Content" })).not.toBeInTheDocument();
  });

  it("keeps reasoning summary and detailed reasoning as separate Markdown sections", () => {
    render(
      <Inspector
        detail={{
          event_key: "event.v1.reasoning",
          event: {
            type: "reasoning",
            summary: "The **short** explanation.",
            text: "The full explanation.",
          },
          native: null,
          is_hidden: false,
          tool_output: null,
        }}
        error={null}
        event={{
          ...EVENT,
          event_key: "event.v1.reasoning",
          type: "reasoning",
          role: null,
          title: "Reasoning",
          summary: "The short explanation.",
          reasoning: {
            preview: "The short explanation.",
            has_summary: true,
            has_text: true,
            has_encrypted_content: false,
            is_redacted: false,
          },
        }}
        is_loading={false}
        is_open
        on_close={vi.fn()}
        on_retry={vi.fn()}
      />,
    );

    expect(screen.getByRole("heading", { name: "Summary", level: 3 })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Reasoning", level: 3 })).toBeInTheDocument();
    expect(screen.getByText("short").tagName).toBe("STRONG");
  });

  it("does not render provider-redacted reasoning in the Content tab", () => {
    render(
      <Inspector
        detail={{
          event_key: "event.v1.reasoning-redacted",
          event: {
            type: "reasoning",
            summary: "provider-withheld summary",
            text: "provider-withheld text",
            redacted: true,
          },
          native: { source: "provider-withheld native" },
          is_hidden: false,
          tool_output: null,
        }}
        error={null}
        event={{
          ...EVENT,
          event_key: "event.v1.reasoning-redacted",
          type: "reasoning",
          role: null,
          title: "Reasoning",
          summary: "Reasoning redacted by provider",
          reasoning: {
            preview: null,
            has_summary: true,
            has_text: true,
            has_encrypted_content: false,
            is_redacted: true,
          },
        }}
        is_loading={false}
        is_open
        on_close={vi.fn()}
        on_retry={vi.fn()}
      />,
    );

    expect(screen.queryByRole("tab", { name: "Content" })).not.toBeInTheDocument();
    expect(screen.getByText("Reasoning redacted by provider")).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Native" })).toBeDisabled();
    expect(screen.queryByText("provider-withheld summary")).not.toBeInTheDocument();
    expect(screen.queryByText("provider-withheld text")).not.toBeInTheDocument();
    expect(screen.queryByText("provider-withheld native")).not.toBeInTheDocument();
  });
});
