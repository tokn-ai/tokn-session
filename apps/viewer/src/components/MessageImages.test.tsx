import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { loadSessionImage } from "../lib/tauri";
import { captureTransport, viewerStorageScope } from "../lib/transport";
import { MessageCard } from "./MessageCard";
import { InlineMessageImage, MessageImages } from "./MessageImages";
import { ActionNotificationsProvider, useActionNotification } from "./ActionNotifications";
import type { EventSummary, MessageImageSummary } from "../lib/types";
vi.mock("../lib/tauri", () => ({ loadSessionImage: vi.fn(), loadEventDetail: vi.fn() }));
vi.mock("../lib/transport", async (original) => ({ ...await original<typeof import("../lib/transport")>(), captureTransport: vi.fn(), viewerStorageScope: vi.fn() }));
const image: MessageImageSummary = { image_id: "image-0", name: "photo.png", mime_type: "image/png", availability: "available" };
const send = vi.fn();
let sequence = 0;
let close: () => void;
beforeEach(() => {
  vi.resetAllMocks();
  Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => `blob:image-${Math.random()}`) }, revokeObjectURL: { configurable: true, value: vi.fn() } });
  vi.mocked(viewerStorageScope).mockReturnValue(`host-${sequence++}`);
  vi.mocked(captureTransport).mockReturnValue({ invoke: send, listen: vi.fn(), release: vi.fn(), on_close: (callback) => { close = callback; return () => {}; } });
  vi.mocked(loadSessionImage).mockResolvedValue({ mime_type: "image/png", data_base64: btoa("image"), width: 100, height: 100 });
});
afterEach(() => { cleanup(); close?.(); vi.useRealTimers(); vi.unstubAllGlobals(); });
describe("recorded image display", () => {
  it("loads a thumbnail and requests full bytes only after opening its preview", async () => {
    render(<MessageImages session_key="session" event_key="window.generation.event" images={[image]} />);
    fireEvent.click(await screen.findByRole("button", { name: "Preview photo.png" }));
    expect(await screen.findByRole("dialog", { name: "Image preview" })).toBeVisible();
    await waitFor(() => expect(loadSessionImage).toHaveBeenCalledTimes(2));
    expect(vi.mocked(loadSessionImage).mock.calls.map(([request]) => request.variant)).toEqual(["thumbnail", "full"]);
    expect(loadSessionImage).toHaveBeenCalledWith({ session_key: "session", event_key: "window.generation.event", image_id: "image-0", variant: "full" }, send);
    fireEvent.click(screen.getByRole("button", { name: "Close image preview" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
  it.each(["attachment", "inline"])("dismisses the %s preview only for a complete backdrop click and restores focus", async (kind) => {
    render(kind === "inline"
      ? <InlineMessageImage session_key="session" event_key="event" revision={null} image={image} />
      : <MessageImages session_key="session" event_key="event" images={[image]} />);
    const opener = await screen.findByRole("button", { name: "Preview photo.png" });
    opener.focus();
    fireEvent.click(opener);
    const dialog = screen.getByRole("dialog", { name: "Image preview" });
    vi.spyOn(dialog, "getBoundingClientRect").mockReturnValue({ left: 100, top: 100, right: 500, bottom: 400 } as DOMRect);
    const full_image = await within(dialog).findByRole("img", { name: "photo.png" });
    const pointerDown = (target: Element, clientX: number, clientY: number, button = 0) =>
      fireEvent(target, new MouseEvent("pointerdown", { bubbles: true, clientX, clientY, button }));

    // Image/header content and the dialog's own empty area all stay open.
    for (const target of [full_image, within(dialog).getByText("photo.png"), dialog]) {
      pointerDown(target, 150, 150);
      fireEvent.click(target, { clientX: 150, clientY: 150 });
      expect(dialog).toBeInTheDocument();
    }
    // Ending a gesture on the other side of the boundary is not dismissal.
    pointerDown(full_image, 150, 150);
    fireEvent.click(dialog, { clientX: 50, clientY: 50 });
    expect(dialog).toBeInTheDocument();
    pointerDown(dialog, 50, 50);
    fireEvent.click(dialog, { clientX: 150, clientY: 150 });
    expect(dialog).toBeInTheDocument();
    pointerDown(dialog, 50, 50);
    fireEvent.pointerCancel(dialog);
    fireEvent.click(dialog, { clientX: 50, clientY: 50 });
    expect(dialog).toBeInTheDocument();
    pointerDown(dialog, 50, 50, 2);
    fireEvent.click(dialog, { clientX: 50, clientY: 50, button: 2 });
    expect(dialog).toBeInTheDocument();

    pointerDown(dialog, 50, 50);
    fireEvent.click(dialog, { clientX: 50, clientY: 50 });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
    fireEvent.click(opener);
    fireEvent(screen.getByRole("dialog"), new Event("cancel", { cancelable: true }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
  });
  it("keeps late action notifications inside the image preview and returns them to the page on close", async () => {
    let notify!: ReturnType<typeof useActionNotification>;
    function NotificationSource() { notify = useActionNotification(); return null; }
    render(<ActionNotificationsProvider><NotificationSource />
      <MessageImages session_key="session" event_key="event" images={[image]} />
    </ActionNotificationsProvider>);
    const preview = await screen.findByRole("button", { name: "Preview photo.png" });
    const thumbnail_url = within(preview).getByRole("img").getAttribute("src");
    fireEvent.click(preview);
    const dialog = screen.getByRole("dialog", { name: "Image preview" });
    expect(await within(dialog).findByRole("img", { name: "photo.png" })).toHaveAttribute("src", expect.stringMatching(/^blob:image-/));
    vi.useFakeTimers();
    act(() => { notify({ title: "Pi", message: "Restart requested" }); });
    expect(within(dialog).getByRole("status")).toHaveTextContent("Restart requested");
    expect(within(dialog).getByRole("status")).toHaveAttribute("data-placement", "dialog");
    act(() => { vi.advanceTimersByTime(2000); });
    fireEvent.click(within(dialog).getByRole("button", { name: "Close image preview" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveAttribute("data-placement", "page");
    expect(screen.getByRole("status")).toHaveTextContent("Restart requested");
    expect(within(preview).getByRole("img")).toHaveAttribute("src", thumbnail_url);
    expect(vi.mocked(loadSessionImage).mock.calls.map(([request]) => request.variant)).toEqual(["thumbnail", "full"]);
    act(() => { vi.advanceTimersByTime(2999); });
    expect(screen.getByRole("status")).toBeInTheDocument();
    act(() => { vi.advanceTimersByTime(1); });
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });
  it("waits until an image approaches the viewport", async () => {
    let observer!: (entries: { isIntersecting: boolean }[]) => void;
    vi.stubGlobal("IntersectionObserver", class {
      constructor(callback: typeof observer) { observer = callback; }
      observe() {} disconnect() {}
    });
    render(<MessageImages session_key="session" event_key="event" images={[image]} />);
    expect(loadSessionImage).not.toHaveBeenCalled();
    act(() => observer([{ isIntersecting: true }]));
    await screen.findByRole("button", { name: "Preview photo.png" });
    expect(loadSessionImage).toHaveBeenCalledOnce();
  });
  it("shows unavailable metadata without requesting bytes and retains an image-only user row", () => {
    const event = { event_key: "event", type: "message", role: "user", title: "User message", summary: "", is_hidden: false,
      images: [{ ...image, availability: "unavailable" }] } as EventSummary;
    render(<MessageCard event={event} session_key="session" button_id="inspect" is_selected={false} on_select={vi.fn()} />);
    expect(screen.getByText(/Image unavailable/)).toBeInTheDocument();
    expect(screen.queryByText("User message")).not.toBeInTheDocument();
    expect(loadSessionImage).not.toHaveBeenCalled();
  });
  it("keeps failed reads visible and retries explicitly", async () => {
    vi.mocked(loadSessionImage).mockRejectedValueOnce(new Error("Image is missing"));
    render(<MessageImages session_key="session" event_key="event" images={[image]} />);
    fireEvent.click(await screen.findByRole("button", { name: "Retry image" }));
    await screen.findByRole("button", { name: "Preview photo.png" });
    expect(loadSessionImage).toHaveBeenCalledTimes(2);
  });
  it("revokes the host's cached URLs when its connection closes", async () => {
    render(<MessageImages session_key="session" event_key="event" images={[image]} />);
    await screen.findByRole("button", { name: "Preview photo.png" });
    act(() => close());
    expect(URL.revokeObjectURL).toHaveBeenCalledOnce();
  });
});

it.each(["user", "assistant"])("keeps recorded %s attachment order consistent with its message role", (role) => {
  const event = { event_key: "event", type: "message", role, title: "Message", summary: "Message text", is_hidden: false,
    images: [{ ...image, availability: "unavailable" }] } as EventSummary;
  render(<MessageCard event={event} session_key="session" button_id="inspect" is_selected={false} on_select={vi.fn()} />);
  const attachment = screen.getByText(/Image unavailable/);
  const text = screen.getByText("Message text");
  const order = role === "user" ? Node.DOCUMENT_POSITION_FOLLOWING : Node.DOCUMENT_POSITION_PRECEDING;
  expect(attachment.compareDocumentPosition(text) & order).toBeTruthy();
});
