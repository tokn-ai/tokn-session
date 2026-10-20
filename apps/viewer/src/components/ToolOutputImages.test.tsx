import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventCard } from "./EventCard";
import { loadSessionImage } from "../lib/tauri";
import { captureTransport, viewerStorageScope } from "../lib/transport";
import type { EventDetail, EventSummary, LoadSessionImageResponse, MessageImageSummary, ToolCardSummary } from "../lib/types";
vi.mock("../lib/tauri", () => ({ loadSessionImage: vi.fn() }));
vi.mock("../lib/transport", async (original) => ({ ...await original<typeof import("../lib/transport")>(), captureTransport: vi.fn(), viewerStorageScope: vi.fn() }));
const image: MessageImageSummary = { image_id: "image.v1.0", name: "result.png", mime_type: "image/png", availability: "available" };
const tool = { kind: "other", tool_name: "image_tool", tool_call_id: "call", command: null, cwd: null, path: null, query: null, url: null,
  task_title: null, exit_code: null, bytes: null, added: null, removed: null } as ToolCardSummary;
function event(overrides: Partial<EventSummary> = {}): EventSummary {
  return { event_key: "event.v1.2", type: "tool_call", provider: "pi", timestamp: null, phase: "finished", role: null,
    title: "Image tool", summary: "", summary_truncated: false, is_hidden: false, is_error: false, tool, usage: null, reasoning: null, ...overrides };
}
function detail(overrides: Partial<EventDetail> = {}): EventDetail {
  return { event_key: "event.v1.2", event: { type: "tool_call" }, native: null, is_hidden: false, tool_output: null, ...overrides };
}
function card(value: EventSummary, extra: Partial<React.ComponentProps<typeof EventCard>> = {}) {
  return <EventCard session_key="session" event={value} button_id="inspect-tool" is_selected={false} is_expanded
    detail={null} detail_error={null} detail_loading={false} on_select={vi.fn()} on_toggle={vi.fn()} on_retry_detail={vi.fn()} {...extra} />;
}
let scope = 0;
let close: () => void;
const send = vi.fn();
beforeEach(() => {
  vi.resetAllMocks();
  Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => `blob:tool-${Math.random()}`) }, revokeObjectURL: { configurable: true, value: vi.fn() } });
  vi.mocked(viewerStorageScope).mockReturnValue(`tool-host-${scope++}`);
  vi.mocked(captureTransport).mockReturnValue({ invoke: send, listen: vi.fn(), release: vi.fn(), on_close: (callback) => { close = callback; return () => {}; } });
  vi.mocked(loadSessionImage).mockResolvedValue({ mime_type: "image/png", data_base64: btoa("png"), width: 1, height: 1 });
});
afterEach(() => { cleanup(); close?.(); });

describe("tool output images", () => {
  it("never requests image bytes from a collapsed tool heading", () => {
    render(card(event({ images: [image] }), { is_expanded: false }));
    expect(loadSessionImage).not.toHaveBeenCalled();
    expect(screen.queryByLabelText("Tool output images")).not.toBeInTheDocument();
  });

  it("renders image-only raw output and uses its parent event for resource reads", async () => {
    render(card(event({ images: [image], content_revision: "raw-image" })));
    fireEvent.click(await screen.findByRole("button", { name: "Preview result.png" }));
    await waitFor(() => expect(loadSessionImage).toHaveBeenCalledTimes(2));
    expect(loadSessionImage).toHaveBeenLastCalledWith({ session_key: "session", event_key: "event.v1.2", image_id: image.image_id, variant: "full" }, send);
    expect(screen.queryByText("No output was captured for this tool call.")).not.toBeInTheDocument();
  });

  it("reads semantic detail images through the raw output owner for both preview variants", async () => {
    render(card(event({ event_key: "operation.v1.1", content_revision: "semantic" }), { detail: detail({ event_key: "operation.v1.1", content_revision: "semantic",
      event: { type: "tool_operation", images: [{ ...image, source_event_key: "window.v1.generation.event.v1.9" }] } }) }));
    fireEvent.click(await screen.findByRole("button", { name: "Preview result.png" }));
    await waitFor(() => expect(loadSessionImage).toHaveBeenCalledTimes(2));
    expect(vi.mocked(loadSessionImage).mock.calls.map(([request]) => request.event_key)).toEqual(["window.v1.generation.event.v1.9", "window.v1.generation.event.v1.9"]);
  });

  it("keeps shell text, failure, truncation, and unavailable-image state together", () => {
    render(card(event({ is_error: true, images: [{ ...image, availability: "unavailable" }], tool: { ...tool, kind: "shell", command: "render result", cwd: "/project", exit_code: 1 } }), {
      detail: detail({ tool_output: { source_event_key: "event.v1.2", sections: [{ label: "stderr", text: "Renderer exited", format: "text" }], truncated: true, original_size_bytes: 1000 } }),
      detail_error: "Could not refresh output",
    }));
    expect(screen.getByText("Renderer exited")).toBeInTheDocument();
    expect(screen.getByText("Exit 1")).toBeInTheDocument();
    expect(screen.getByText(/Output preview is truncated/)).toBeInTheDocument();
    expect(screen.getByText(/Image unavailable/)).toBeInTheDocument();
    expect(screen.getByText(/Could not refresh output/)).toBeInTheDocument();
    expect(loadSessionImage).not.toHaveBeenCalled();
  });

  it("lets repeated part IDs from separate outputs open their own full image", async () => {
    render(card(event({ images: [{ ...image, name: "first.png", source_event_key: "output-one" }, { ...image, name: "second.png", source_event_key: "output-two" }] })));
    await screen.findByRole("button", { name: "Preview first.png" });
    fireEvent.click(await screen.findByRole("button", { name: "Preview second.png" }));
    await waitFor(() => expect(vi.mocked(loadSessionImage).mock.calls.some(([request]) => request.variant === "full")).toBe(true));
    expect(loadSessionImage).toHaveBeenLastCalledWith(expect.objectContaining({ event_key: "output-two", variant: "full" }), send);
  });

  it("clears an output image when live metadata replaces it with an empty array", async () => {
    const retained = detail({ content_revision: "old", event: { type: "tool_operation", images: [{ ...image }] } });
    const view = render(card(event({ images: [image], content_revision: "old" }), { detail: retained }));
    await screen.findByRole("button", { name: "Preview result.png" });
    view.rerender(card(event({ images: [], content_revision: null }), { detail: retained }));
    expect(screen.queryByLabelText("Tool output images")).not.toBeInTheDocument();
    expect(screen.queryByAltText("result.png")).not.toBeInTheDocument();
    expect(loadSessionImage).toHaveBeenCalledOnce();
  });

  it("does not keep the old thumbnail visible while a revised output image loads", async () => {
    const view = render(card(event({ images: [image], content_revision: "old" })));
    const previous = await screen.findByAltText("result.png");
    const old_url = previous.getAttribute("src");
    let resolve!: (value: LoadSessionImageResponse) => void;
    vi.mocked(loadSessionImage).mockReturnValue(new Promise((done) => { resolve = done; }));
    view.rerender(card(event({ images: [image], content_revision: "new" })));
    expect(screen.queryByAltText("result.png")).not.toBeInTheDocument();
    await act(async () => resolve({ mime_type: "image/png", data_base64: btoa("new"), width: 1, height: 1 }));
    expect(await screen.findByAltText("result.png")).not.toHaveAttribute("src", old_url);
  });
});
