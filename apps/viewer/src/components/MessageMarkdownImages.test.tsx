import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MessageCard } from "./MessageCard";
import { loadSessionImage } from "../lib/tauri";
import { captureTransport, viewerStorageScope } from "../lib/transport";
import { useMessageTranslation } from "../lib/useMessageTranslation";
import type { EventSummary, MessageImageSummary } from "../lib/types";
vi.mock("../lib/tauri", () => ({ loadSessionImage: vi.fn() }));
vi.mock("../lib/useMessageTranslation", () => ({ useMessageTranslation: vi.fn() }));
vi.mock("../lib/transport", async (original) => ({ ...await original<typeof import("../lib/transport")>(), captureTransport: vi.fn(), viewerStorageScope: vi.fn() }));
const markdown = "![Updated Runtimes screen](/tmp/runtime-screen.png)";
const prefix = "Before the screen ";
const image: MessageImageSummary = { image_id: "markdown.v1.authorized", name: "runtime-screen.png", mime_type: "image/png", availability: "available",
  markdown_range: { start: prefix.length, end: prefix.length + markdown.length } };
function event(overrides: Partial<EventSummary> = {}): EventSummary {
  return { event_key: "window.v1.generation.event.v1.1", type: "message", role: "assistant", provider: "codex", phase: "finished", timestamp: null,
    title: "Assistant", summary: `${prefix}${markdown} after the screen.`, summary_truncated: false, is_hidden: false, is_error: false,
    images: [image], content_revision: "image-content", tool: null, reasoning: null, usage: null, ...overrides };
}
function card(value: EventSummary) { return <MessageCard event={value} session_key="session" button_id="inspect" is_selected={false} on_select={vi.fn()} />; }
let translated: string | null = null;
let scope = 0;
let close: () => void;
const send = vi.fn();
beforeEach(() => {
  vi.resetAllMocks(); translated = null;
  Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => `blob:inline-${Math.random()}`) }, revokeObjectURL: { configurable: true, value: vi.fn() } });
  vi.mocked(viewerStorageScope).mockReturnValue(`markdown-host-${scope++}`);
  vi.mocked(captureTransport).mockReturnValue({ invoke: send, listen: vi.fn(), release: vi.fn(), on_close: (callback) => { close = callback; return () => {}; } });
  vi.mocked(loadSessionImage).mockResolvedValue({ mime_type: "image/png", data_base64: btoa("png"), width: 1, height: 1 });
  vi.mocked(useMessageTranslation).mockImplementation((value) => ({ content: translated ?? value.summary, loading: false, translated: translated !== null,
    showing_translation: translated !== null, error: null, can_open_settings: false, settings_loading: false, settings_error: null, progress: null,
    translate: vi.fn(async () => {}), cancel: vi.fn(), openSettings: vi.fn(async () => {}), toggle: vi.fn() }));
});
afterEach(() => { cleanup(); close?.(); });

describe("authorized inline Markdown images", () => {
  it("renders at the Markdown position without a duplicate tray or a raw path request", async () => {
    const { container } = render(card(event()));
    const thumbnail = await screen.findByAltText("Updated Runtimes screen");
    const paragraph = thumbnail.closest("p")!;
    expect(paragraph).toHaveTextContent("Before the screen");
    expect(paragraph).toHaveTextContent("after the screen");
    expect(container.querySelector("p div")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Message images")).not.toBeInTheDocument();
    expect(screen.queryByText("[Image: Updated Runtimes screen]")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Preview Updated Runtimes screen" }));
    await waitFor(() => expect(loadSessionImage).toHaveBeenCalledTimes(2));
    expect(loadSessionImage).toHaveBeenLastCalledWith({ session_key: "session", event_key: event().event_key, image_id: image.image_id, variant: "full" }, send);
    expect(JSON.stringify(vi.mocked(loadSessionImage).mock.calls)).not.toContain("/tmp/");
  });

  it("keeps old-host and unmatched Markdown destinations as inert placeholders", () => {
    const { container } = render(card(event({ images: undefined, summary: `${markdown} ![remote](https://example.com/tracker.png)` })));
    expect(screen.getByRole("img", { name: "Image: Updated Runtimes screen" })).toBeInTheDocument();
    expect(screen.getByRole("img", { name: "Image: remote" })).toBeInTheDocument();
    expect(container.querySelector("img")).not.toBeInTheDocument();
    expect(container.innerHTML).not.toContain("tracker.png");
    expect(loadSessionImage).not.toHaveBeenCalled();
  });

  it("shows ranges beyond a clipped summary in the tray while avoiding a duplicate visible inline image", async () => {
    const beyond = { ...image, image_id: "outside-preview", name: "older.png", markdown_range: { start: 1000, end: 1050 } };
    render(card(event({ summary_truncated: true, images: [image, beyond] })));
    expect(await screen.findByAltText("Updated Runtimes screen")).toBeInTheDocument();
    expect(await screen.findByAltText("older.png")).toBeInTheDocument();
    expect(screen.getByLabelText("Message images").querySelectorAll("img")).toHaveLength(1);
  });

  it("uses the tray for translated text and restores original inline placement without fetching again", async () => {
    const value = event();
    const view = render(card(value));
    await screen.findByAltText("Updated Runtimes screen");
    translated = `译文改变了位置 ${markdown}`;
    view.rerender(card(value));
    expect(screen.getByRole("img", { name: "Image: Updated Runtimes screen" })).toBeInTheDocument();
    expect(await screen.findByAltText("runtime-screen.png")).not.toBeNull();
    expect(screen.getByLabelText("Message images")).toBeInTheDocument();
    translated = null; view.rerender(card(value));
    await screen.findByAltText("Updated Runtimes screen");
    expect(screen.queryByLabelText("Message images")).not.toBeInTheDocument();
    expect(loadSessionImage).toHaveBeenCalledOnce();
  });

  it("shows an unavailable inline reference without reading bytes", () => {
    render(card(event({ images: [{ ...image, availability: "unavailable" }] })));
    expect(screen.getByText(/Image unavailable/)).toBeInTheDocument();
    expect(screen.queryByLabelText("Message images")).not.toBeInTheDocument();
    expect(loadSessionImage).not.toHaveBeenCalled();
  });

  it("bounds Markdown labels like host metadata", async () => {
    const alt = `${"📸".repeat(300)}\u0007`;
    const content = `![${alt}](/tmp/screen.png)`;
    render(card(event({ summary: content, images: [{ ...image, markdown_range: { start: 0, end: content.length } }] })));
    const label = "📸".repeat(255);
    expect(await screen.findByAltText(label)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: `Preview ${label}` })).toBeInTheDocument();
  });
});

it("places a reference-style image after Unicode using full-source UTF-16 offsets", async () => {
  const reference = "![Updated Runtimes screen][shot]";
  const text = `📸 画面\n\n${reference}\n\n[shot]: /tmp/runtime-screen.png`;
  const start = text.indexOf(reference);
  render(card(event({ summary: text, images: [{ ...image, markdown_range: { start, end: start + reference.length } }] })));
  expect((await screen.findByAltText("Updated Runtimes screen")).closest("p")).not.toBeNull();
  expect(screen.queryByLabelText("Message images")).not.toBeInTheDocument();
  expect(loadSessionImage).toHaveBeenCalledOnce();
});

it("binds a Markdown image after a leading BOM and emoji without duplicating the tray", async () => {
  const text = `\uFEFF📸 ${markdown}`;
  const start = text.indexOf(markdown);
  render(card(event({ summary: text, images: [{ ...image, markdown_range: { start, end: start + markdown.length } }] })));
  expect((await screen.findByAltText("Updated Runtimes screen")).closest("p")).not.toBeNull();
  expect(screen.queryByLabelText("Message images")).not.toBeInTheDocument();
  expect(screen.queryByRole("img", { name: "Image: Updated Runtimes screen" })).not.toBeInTheDocument();
  expect(loadSessionImage).toHaveBeenCalledOnce();
});
