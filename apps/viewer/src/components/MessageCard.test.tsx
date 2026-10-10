import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { browserTranslationEngine } from "../lib/browserTranslation";
import { cancelTranslation, getTranslationStatus, loadEventDetail, translateText } from "../lib/tauri";
import type { TranslationJob, TranslationProgress } from "../lib/translationEngine";
import { isDesktop } from "../lib/transport";
import type { EventDetail, EventSummary, TranslateTextResponse, TrajectoryEventPageState } from "../lib/types";
import { EventCard } from "./EventCard";
import { MessageCard } from "./MessageCard";
import { TranslationProvider } from "./TranslationProvider";

vi.mock("../lib/tauri", () => ({
  cancelTranslation: vi.fn(),
  getTranslationStatus: vi.fn(),
  loadEventDetail: vi.fn(),
  translateText: vi.fn(),
}));
vi.mock("../lib/transport", () => ({ isDesktop: vi.fn() }));
vi.mock("../lib/browserTranslation", () => ({
  browserTranslationEngine: {
    label: "Browser Translation",
    description: "Translate locally in your browser.",
    getStatus: vi.fn(),
    start: vi.fn(),
  },
}));

const browser_translate = vi.fn<TranslationJob["translate"]>();
const browser_dispose = vi.fn<TranslationJob["dispose"]>();
let browser_progress: (progress: TranslationProgress) => void;

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

function message(overrides: Partial<EventSummary> = {}): EventSummary {
  return {
    event_key: "event.v1.1",
    type: "message",
    provider: "codex",
    timestamp: "2026-09-13T00:00:00Z",
    phase: "finished",
    role: "assistant",
    title: "Assistant message",
    summary: "Original response.",
    summary_truncated: false,
    is_hidden: false,
    is_error: false,
    tool: null,
    usage: null,
    reasoning: null,
    ...overrides,
  };
}

function detail(text = "Original response.", overrides: Partial<EventDetail> = {}): EventDetail {
  return {
    event_key: "event.v1.1",
    event: { type: "message", role: "assistant", text },
    native: null,
    is_hidden: false,
    tool_output: null,
    ...overrides,
  };
}

function card(event: EventSummary, session_key: string | undefined = "session.v1.first") {
  return (
    <TranslationProvider>
      <MessageCard
        button_id="inspect-message"
        event={event}
        is_selected={false}
        on_select={vi.fn()}
        session_key={session_key}
      />
    </TranslationProvider>
  );
}

async function startTranslation() {
  fireEvent.click(await screen.findByRole("button", { name: "Translate → 简体中文" }));
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(isDesktop).mockReturnValue(true);
  vi.mocked(getTranslationStatus).mockResolvedValue({ available: true, reason: null });
  vi.mocked(loadEventDetail).mockResolvedValue(detail());
  vi.mocked(translateText).mockResolvedValue({ texts: ["中文译文。"] });
  vi.mocked(cancelTranslation).mockResolvedValue();
  vi.mocked(browserTranslationEngine.getStatus).mockResolvedValue({ available: true, reason: null });
  browser_translate.mockResolvedValue(["浏览器译文。"]);
  vi.mocked(browserTranslationEngine.start).mockImplementation((on_progress) => {
    browser_progress = on_progress;
    return { translate: browser_translate, dispose: browser_dispose };
  });
});
afterEach(cleanup);

describe("message translation availability", () => {
  it.each([
    { role: "user" },
    { role: "system" },
    { role: "tool" },
    { role: "unknown" },
    { is_hidden: true },
    { summary: " \n\t " },
  ])("excludes messages with %j", async (overrides) => {
    render(card(message(overrides)));
    await act(async () => {});
    expect(screen.queryByRole("button", { name: /Translate/ })).not.toBeInTheDocument();
    expect(loadEventDetail).not.toHaveBeenCalled();
    expect(translateText).not.toHaveBeenCalled();
  });

  it("requires a session identity", async () => {
    render(
      <TranslationProvider>
        <MessageCard button_id="inspect-message" event={message()} is_selected={false} on_select={vi.fn()} />
      </TranslationProvider>,
    );
    await act(async () => {});
    expect(screen.queryByRole("button", { name: /Translate/ })).not.toBeInTheDocument();
  });

  it("explains unavailable desktop translation without starting a request", async () => {
    vi.mocked(getTranslationStatus).mockResolvedValue({ available: false, reason: "Requires macOS 15 or later." });
    render(card(message()));
    const button = await screen.findByRole("button", { name: "Translate → 简体中文" });
    expect(button).toBeDisabled();
    expect(button).toHaveAttribute("title", "Requires macOS 15 or later.");
    fireEvent.click(button);
    expect(loadEventDetail).not.toHaveBeenCalled();
    expect(translateText).not.toHaveBeenCalled();
  });

  it("explains unavailable browser translation without invoking native commands", async () => {
    vi.mocked(isDesktop).mockReturnValue(false);
    vi.mocked(browserTranslationEngine.getStatus).mockResolvedValue({ available: false, reason: "This browser does not support local translation." });
    render(card(message()));
    const button = await screen.findByRole("button", { name: "Translate → 简体中文" });
    expect(button).toBeDisabled();
    expect(button).toHaveAttribute("title", "This browser does not support local translation.");
    fireEvent.click(button);
    expect(browserTranslationEngine.start).not.toHaveBeenCalled();
    expect(getTranslationStatus).not.toHaveBeenCalled();
    expect(loadEventDetail).not.toHaveBeenCalled();
    expect(translateText).not.toHaveBeenCalled();
  });
});

describe("browser message translation", () => {
  beforeEach(() => { vi.mocked(isDesktop).mockReturnValue(false); });

  it("prepares models in the click before full detail loads, with download progress and a continuation action", async () => {
    const pending_detail = deferred<EventDetail>();
    const pending_translation = deferred<string[]>();
    vi.mocked(loadEventDetail).mockReturnValue(pending_detail.promise);
    browser_translate.mockReturnValue(pending_translation.promise);
    vi.mocked(browserTranslationEngine.start).mockImplementation((on_progress) => {
      browser_progress = on_progress;
      on_progress({ message: "Downloading language detector…" });
      return { translate: browser_translate, dispose: browser_dispose };
    });
    render(card(message()));
    expect(await screen.findByRole("button", { name: "Translate → 简体中文" })).toHaveAttribute("title", browserTranslationEngine.description);
    await startTranslation();
    expect(browserTranslationEngine.start).toHaveBeenCalledTimes(1);
    expect(vi.mocked(browserTranslationEngine.start).mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(loadEventDetail).mock.invocationCallOrder[0]);
    expect(screen.getByRole("status")).toHaveTextContent("Downloading language detector…");
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    expect(browser_translate).not.toHaveBeenCalled();

    act(() => browser_progress({ message: "Downloading language detector… 50%" }));
    expect(screen.getByRole("status")).toHaveTextContent("50%");
    const resume = vi.fn(() => browser_progress({ message: "Downloading translation model…" }));
    act(() => browser_progress({ message: "Continue to download the translation model.", resume }));
    fireEvent.click(screen.getByRole("button", { name: "Continue translation" }));
    expect(resume).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("status")).toHaveTextContent("Downloading translation model…");
    expect(screen.queryByRole("button", { name: "Continue translation" })).not.toBeInTheDocument();

    await act(async () => pending_detail.resolve(detail()));
    await waitFor(() => expect(browser_translate).toHaveBeenCalledWith(["Original response."]));
    await act(async () => pending_translation.resolve(["浏览器译文。"]));
    expect(await screen.findByText("浏览器译文。")).toBeInTheDocument();
    expect(screen.getByText("简体中文 · Browser Translation")).toBeInTheDocument();
    expect(browser_dispose).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Show original" }));
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Show translation" }));
    expect(screen.getByText("浏览器译文。")).toBeInTheDocument();
    expect(browser_translate).toHaveBeenCalledTimes(1);
    expect(getTranslationStatus).not.toHaveBeenCalled();
    expect(translateText).not.toHaveBeenCalled();
    expect(cancelTranslation).not.toHaveBeenCalled();
  });

  it("preserves Markdown formatting while excluding code and URLs from browser translation", async () => {
    const original = "# Full answer\n\nA **complete** response with [documentation](https://example.com/docs).\n\n```ts\nconst untouched = true;\n```";
    vi.mocked(loadEventDetail).mockResolvedValue(detail(original));
    browser_translate.mockImplementation(async (texts) => texts.map((text) => text
      .replace("Full answer", "完整回答")
      .replace("complete", "完整")
      .replace("documentation", "文档")));
    render(card(message({ summary: "# Full answer…", summary_truncated: true })));
    await startTranslation();
    expect(await screen.findByRole("heading", { name: "完整回答" })).toBeInTheDocument();
    expect(screen.getByText("完整").tagName).toBe("STRONG");
    expect(screen.getByText("文档")).toHaveClass("markdown-content__link");
    expect(screen.getByText("文档")).not.toHaveAttribute("href");
    expect(screen.getByText("const untouched = true;")).toBeInTheDocument();
    const prose = browser_translate.mock.calls.flatMap(([texts]) => texts).join(" ");
    expect(prose).toContain("documentation");
    expect(prose).not.toContain("https://example.com/docs");
    expect(prose).not.toContain("const untouched = true;");
    expect(browser_dispose).toHaveBeenCalledTimes(1);
  });

  it("disposes model preparation after full-detail failure and permits a fresh retry", async () => {
    vi.mocked(loadEventDetail).mockRejectedValueOnce(new Error("Session is unavailable."));
    render(card(message()));
    await startTranslation();
    expect(await screen.findByRole("alert")).toHaveTextContent("Session is unavailable.");
    expect(browser_dispose).toHaveBeenCalledTimes(1);
    expect(browser_translate).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Retry translation" }));
    expect(await screen.findByText("浏览器译文。")).toBeInTheDocument();
    expect(browserTranslationEngine.start).toHaveBeenCalledTimes(2);
    expect(browser_dispose).toHaveBeenCalledTimes(2);
  });

  it("disposes a failed browser translation and leaves the original visible", async () => {
    browser_translate.mockRejectedValueOnce(new Error("Translation model download failed."));
    render(card(message()));
    await startTranslation();
    expect(await screen.findByRole("alert")).toHaveTextContent("Translation model download failed.");
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    expect(browser_dispose).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("button", { name: "Retry translation" })).toBeEnabled();
  });

  it("disposes model preparation when cancelled before full detail loads and ignores later progress", async () => {
    const pending_detail = deferred<EventDetail>();
    vi.mocked(loadEventDetail).mockReturnValue(pending_detail.promise);
    render(card(message()));
    await startTranslation();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(browser_dispose).toHaveBeenCalled();
    act(() => browser_progress({ message: "Stale model progress", resume: vi.fn() }));
    await act(async () => pending_detail.resolve(detail()));
    expect(browser_translate).not.toHaveBeenCalled();
    expect(screen.queryByText("Stale model progress")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Continue translation" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Translate → 简体中文" })).toBeEnabled();
  });

  it("ignores a cancelled browser job's late progress and result after a new translation succeeds", async () => {
    const pending = deferred<string[]>();
    browser_translate.mockReturnValueOnce(pending.promise);
    render(card(message()));
    await startTranslation();
    await waitFor(() => expect(browser_translate).toHaveBeenCalledTimes(1));
    const old_progress = browser_progress;
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(browser_dispose).toHaveBeenCalled();
    await startTranslation();
    expect(await screen.findByText("浏览器译文。")).toBeInTheDocument();
    act(() => old_progress({ message: "Outdated download", resume: vi.fn() }));
    await act(async () => pending.resolve(["过期译文。"]));
    expect(screen.getByText("浏览器译文。")).toBeInTheDocument();
    expect(screen.queryByText("过期译文。")).not.toBeInTheDocument();
    expect(screen.queryByText("Outdated download")).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Continue translation" })).not.toBeInTheDocument();
  });

  it.each(["session", "content", "unmount"])("disposes a browser job on %s changes and ignores late activity", async (change) => {
    const pending = deferred<string[]>();
    browser_translate.mockReturnValueOnce(pending.promise);
    const event = message();
    const view = render(card(event));
    await startTranslation();
    await waitFor(() => expect(browser_translate).toHaveBeenCalledTimes(1));
    if (change === "unmount") view.unmount();
    else if (change === "session") view.rerender(card(event, "session.v1.second"));
    else view.rerender(card(message({ summary: "Updated response." })));
    expect(browser_dispose).toHaveBeenCalled();
    act(() => browser_progress({ message: "Outdated download", resume: vi.fn() }));
    await act(async () => pending.resolve(["过期译文。"]));
    expect(screen.queryByText("过期译文。")).not.toBeInTheDocument();
    expect(screen.queryByText("Outdated download")).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    if (change !== "unmount") expect(screen.getByRole("button", { name: "Translate → 简体中文" })).toBeEnabled();
  });
});

describe("message translation flow", () => {
  it("loads full Markdown before translating and keeps the original visible while pending", async () => {
    const original = "# Full answer\n\nA **complete** response beyond the preview.\n\n```ts\nconst untouched = true;\n```";
    const pending_detail = deferred<EventDetail>();
    const pending_translation = deferred<TranslateTextResponse>();
    vi.mocked(loadEventDetail).mockReturnValue(pending_detail.promise);
    vi.mocked(translateText).mockReturnValue(pending_translation.promise);
    render(card(message({ summary: "# Full answer…", summary_truncated: true })));

    await startTranslation();
    expect(screen.getByRole("heading", { name: "Full answer…" })).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Translating");
    expect(translateText).not.toHaveBeenCalled();
    expect(loadEventDetail).toHaveBeenCalledWith({ session_key: "session.v1.first", event_key: "event.v1.1" });

    await act(async () => pending_detail.resolve(detail(original)));
    await waitFor(() => expect(translateText).toHaveBeenCalledTimes(1));
    expect(screen.getByRole("heading", { name: "Full answer" })).toBeInTheDocument();
    expect(screen.getByText("complete").tagName).toBe("STRONG");
    expect(screen.getByText("const untouched = true;")).toBeInTheDocument();
    const request = vi.mocked(translateText).mock.calls[0][0];
    expect(request.target_language).toBe("zh-Hans");
    expect(request.request_id).toEqual(expect.any(String));
    expect(request.texts.join(" ")).toContain("response beyond the preview.");
    expect(request.texts.join(" ")).not.toContain("const untouched = true;");

    await act(async () => pending_translation.resolve({
      texts: request.texts.map((text) => text.replace("Full answer", "完整回答").replace("complete", "完整")),
    }));
    expect(await screen.findByRole("button", { name: "Show original" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "完整回答" })).toBeInTheDocument();
    expect(screen.getByText("完整").tagName).toBe("STRONG");
    expect(screen.getByText("const untouched = true;")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "完整回答" }).closest("[lang]")).toHaveAttribute("lang", "zh-Hans");
  });

  it("toggles cached translation and original without extra backend calls", async () => {
    render(card(message()));
    await startTranslation();
    fireEvent.click(await screen.findByRole("button", { name: "Show original" }));
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    expect(screen.queryByText("中文译文。")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Show translation" }));
    expect(screen.getByText("中文译文。")).toBeInTheDocument();
    expect(loadEventDetail).toHaveBeenCalledTimes(1);
    expect(translateText).toHaveBeenCalledTimes(1);
  });

  it("retains the original after failure and supports retry", async () => {
    vi.mocked(translateText).mockRejectedValueOnce(new Error("Language download failed."));
    render(card(message()));
    await startTranslation();
    expect(await screen.findByRole("alert")).toHaveTextContent("Language download failed.");
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Retry translation" }));
    expect(await screen.findByText("中文译文。")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(translateText).toHaveBeenCalledTimes(2);
  });

  it.each([
    detail("Original response.", { is_hidden: true }),
    detail("Original response.", { event_key: "event.v1.other" }),
    detail("Original response.", { event: { type: "message", role: "user", text: "Original response." } }),
    detail("Original response.", { event: { type: "message", role: "assistant", text: { truncated: true } } }),
    detail(" "),
  ])("rejects unavailable or mismatched full detail %#", async (invalid_detail) => {
    vi.mocked(loadEventDetail).mockResolvedValue(invalid_detail);
    render(card(message()));
    await startTranslation();
    expect(await screen.findByRole("alert")).toHaveTextContent("The full response is unavailable or too large to translate.");
    expect(translateText).not.toHaveBeenCalled();
  });

  it("rejects a changed response instead of translating another snapshot", async () => {
    vi.mocked(loadEventDetail).mockResolvedValue(detail("New response."));
    render(card(message()));
    await startTranslation();
    expect(await screen.findByRole("alert")).toHaveTextContent("This response has changed.");
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    expect(translateText).not.toHaveBeenCalled();
  });
});

describe("translation request lifetime", () => {
  it("cancels during full-detail loading without starting native translation later", async () => {
    const pending = deferred<EventDetail>();
    vi.mocked(loadEventDetail).mockReturnValue(pending.promise);
    render(card(message()));
    await startTranslation();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await act(async () => pending.resolve(detail()));
    expect(screen.getByRole("button", { name: "Translate → 简体中文" })).toBeEnabled();
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    expect(translateText).not.toHaveBeenCalled();
    expect(cancelTranslation).not.toHaveBeenCalled();
  });

  it("cancels the native request and ignores its late result after retry succeeds", async () => {
    const pending = deferred<TranslateTextResponse>();
    vi.mocked(translateText).mockReturnValueOnce(pending.promise);
    render(card(message()));
    await startTranslation();
    await waitFor(() => expect(translateText).toHaveBeenCalledTimes(1));
    const request_id = vi.mocked(translateText).mock.calls[0][0].request_id;
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(cancelTranslation).toHaveBeenCalledWith(request_id);
    await startTranslation();
    await screen.findByText("中文译文。");
    await act(async () => pending.resolve({ texts: ["过期译文。"] }));
    expect(screen.getByText("中文译文。")).toBeInTheDocument();
    expect(screen.queryByText("过期译文。")).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("cancels an outstanding request when the card unmounts", async () => {
    const pending = deferred<TranslateTextResponse>();
    vi.mocked(translateText).mockReturnValue(pending.promise);
    const view = render(card(message()));
    await startTranslation();
    await waitFor(() => expect(translateText).toHaveBeenCalledTimes(1));
    view.unmount();
    expect(cancelTranslation).toHaveBeenCalledWith(vi.mocked(translateText).mock.calls[0][0].request_id);
    await act(async () => pending.reject(new Error("Cancelled.")));
  });

  it("does not reuse a cached result for the same event key in another session", async () => {
    const event = message();
    const view = render(card(event));
    await startTranslation();
    await screen.findByText("中文译文。");
    view.rerender(card(event, "session.v1.second"));
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    expect(screen.queryByText("中文译文。")).not.toBeInTheDocument();
    await startTranslation();
    await screen.findByText("中文译文。");
    expect(loadEventDetail).toHaveBeenLastCalledWith({ session_key: "session.v1.second", event_key: event.event_key });
    expect(translateText).toHaveBeenCalledTimes(2);
  });

  it("invalidates a cached result when content changes under the same event key", async () => {
    const view = render(card(message()));
    await startTranslation();
    await screen.findByText("中文译文。");
    vi.mocked(loadEventDetail).mockResolvedValue(detail("Updated response."));
    view.rerender(card(message({ summary: "Updated response." })));
    expect(screen.getByText("Updated response.")).toBeInTheDocument();
    expect(screen.queryByText("中文译文。")).not.toBeInTheDocument();
    await startTranslation();
    await screen.findByText("中文译文。");
    expect(translateText).toHaveBeenCalledTimes(2);
    expect(vi.mocked(translateText).mock.calls[1][0].texts).toEqual(["Updated response."]);
  });

  it("keeps a cached truncated translation across an unchanged page refresh", async () => {
    const event = message({ summary: "Original…", summary_truncated: true, content_revision: "same-content" });
    vi.mocked(loadEventDetail).mockResolvedValue(detail("Original response.", { content_revision: "same-content" }));
    const view = render(card(event));
    await startTranslation();
    await screen.findByText("中文译文。");
    view.rerender(card({ ...event }));
    expect(screen.getByText("中文译文。")).toBeInTheDocument();
    expect(loadEventDetail).toHaveBeenCalledTimes(1);
    expect(translateText).toHaveBeenCalledTimes(1);
  });

  it("keeps an in-flight native translation across an unchanged page refresh", async () => {
    const pending = deferred<TranslateTextResponse>();
    vi.mocked(translateText).mockReturnValue(pending.promise);
    const event = message({ summary: "Original…", summary_truncated: true, content_revision: "same-content", phase: "started" });
    vi.mocked(loadEventDetail).mockResolvedValue(detail("Original response.", { content_revision: "same-content" }));
    const view = render(card(event));
    await startTranslation();
    await waitFor(() => expect(translateText).toHaveBeenCalledTimes(1));
    view.rerender(card({ ...event, phase: "finished" }));
    expect(screen.getByRole("status")).toHaveTextContent("Translating");
    expect(cancelTranslation).not.toHaveBeenCalled();
    await act(async () => pending.resolve({ texts: ["中文译文。"] }));
    expect(screen.getByText("中文译文。")).toBeInTheDocument();
    expect(loadEventDetail).toHaveBeenCalledTimes(1);
  });

  it("reloads a truncated response when its full-text revision changes but its visible prefix does not", async () => {
    const event = message({ summary: "Original…", summary_truncated: true, content_revision: "first-content" });
    vi.mocked(loadEventDetail).mockResolvedValue(detail("Original response.", { content_revision: "first-content" }));
    const view = render(card(event));
    await startTranslation();
    await screen.findByText("中文译文。");
    vi.mocked(loadEventDetail).mockResolvedValue(detail("Original response with a new ending.", { content_revision: "changed-content" }));
    view.rerender(card({ ...event, content_revision: "changed-content" }));
    expect(screen.getByText("Original…")).toBeInTheDocument();
    expect(screen.queryByText("中文译文。")).not.toBeInTheDocument();
    await startTranslation();
    await screen.findByText("中文译文。");
    expect(loadEventDetail).toHaveBeenCalledTimes(2);
    expect(vi.mocked(translateText).mock.calls[1][0].texts).toEqual(["Original response with a new ending."]);
  });

  it("rejects detail from a newer full-text revision before the page refreshes", async () => {
    vi.mocked(loadEventDetail).mockResolvedValue(detail("Original response with a new ending.", {
      content_revision: "changed-content",
    }));
    render(card(message({ summary: "Original…", summary_truncated: true, content_revision: "first-content" })));
    await startTranslation();
    expect(await screen.findByRole("alert")).toHaveTextContent("This response has changed. Refresh the conversation and try again.");
    expect(screen.getByText("Original…")).toBeInTheDocument();
    expect(translateText).not.toHaveBeenCalled();
  });

  it("invalidates a truncated translation on refresh when an older server omits the full-text revision", async () => {
    const event = message({ summary: "Original…", summary_truncated: true });
    const view = render(card(event));
    await startTranslation();
    await screen.findByText("中文译文。");
    vi.mocked(loadEventDetail).mockResolvedValue(detail("Original response with a new ending."));
    view.rerender(card({ ...event }));
    expect(screen.getByText("Original…")).toBeInTheDocument();
    expect(screen.queryByText("中文译文。")).not.toBeInTheDocument();
    await startTranslation();
    await screen.findByText("中文译文。");
    expect(vi.mocked(translateText).mock.calls[1][0].texts).toEqual(["Original response with a new ending."]);
  });

  it("cancels and discards an old session's completion after navigation", async () => {
    const pending = deferred<TranslateTextResponse>();
    const event = message();
    vi.mocked(translateText).mockReturnValueOnce(pending.promise);
    const view = render(card(event));
    await startTranslation();
    await waitFor(() => expect(translateText).toHaveBeenCalledTimes(1));
    view.rerender(card(event, "session.v1.second"));
    expect(cancelTranslation).toHaveBeenCalledWith(vi.mocked(translateText).mock.calls[0][0].request_id);
    await act(async () => pending.resolve({ texts: ["过期译文。"] }));
    expect(screen.getByText("Original response.")).toBeInTheDocument();
    expect(screen.queryByText("过期译文。")).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Translate → 简体中文" })).toBeEnabled();
  });
});

it("passes the parent session identity into assistant messages inside a trajectory", async () => {
  const child = message();
  const page: TrajectoryEventPageState = {
    events: [child],
    next_cursor: null,
    previous_cursor: null,
    total_events: 1,
    has_loaded: true,
    is_loading: false,
    is_loading_older: false,
    is_loading_newer: false,
    error: null,
    error_direction: null,
    error_cursor: null,
  };
  render(
    <TranslationProvider>
      <EventCard
        button_id="inspect-trajectory"
        detail={null}
        detail_error={null}
        detail_loading={false}
        event={message({ event_key: "trajectory.v1.1", type: "trajectory", role: null, title: "Turn events" })}
        is_expanded
        is_selected={false}
        on_retry_detail={vi.fn()}
        on_select={vi.fn()}
        on_toggle={vi.fn()}
        session_key="session.v1.nested"
        trajectory_page={page}
      />
    </TranslationProvider>,
  );
  await startTranslation();
  await screen.findByText("中文译文。");
  expect(loadEventDetail).toHaveBeenCalledWith({ session_key: "session.v1.nested", event_key: child.event_key });
});
