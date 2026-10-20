import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SessionComposer } from "./SessionComposer";
import { getSessionInputStatus, releaseImageUpload, submitSessionInput, uploadImageChunk } from "../lib/tauri";
import { captureTransport, viewerStorageScope } from "../lib/transport";
import { DEFAULT_IMAGE_LIMITS } from "../lib/imageDrafts";
import type { SessionSummary, UploadImageChunkResponse } from "../lib/types";
vi.mock("../lib/tauri", () => ({ getSessionInputStatus: vi.fn(), submitSessionInput: vi.fn(), uploadImageChunk: vi.fn(), releaseImageUpload: vi.fn() }));
vi.mock("../lib/transport", async (original) => ({ ...await original<typeof import("../lib/transport")>(), captureTransport: vi.fn(), viewerStorageScope: vi.fn() }));
const session = { session_key: "codex:one", provider: "codex" } as SessionSummary;
const other = { ...session, session_key: "codex:other" };
const send = vi.fn();
function attach() { fireEvent.change(screen.getByLabelText("Image files"), { target: { files: [new File(["png"], "screen.png", { type: "image/png" })] } }); }
async function ready() { await waitFor(() => expect(screen.getByRole("textbox", { name: "Message this session" })).toBeEnabled()); }
function clickSend() { fireEvent.click(screen.getByRole("button", { name: "Send" })); }
beforeEach(() => {
  vi.resetAllMocks();
  Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => "blob:draft") }, revokeObjectURL: { configurable: true, value: vi.fn() } });
  vi.mocked(viewerStorageScope).mockReturnValue("host-one");
  vi.mocked(captureTransport).mockReturnValue({ invoke: send, listen: vi.fn(), release: vi.fn(), on_close: () => () => {} });
  vi.mocked(getSessionInputStatus).mockResolvedValue({ available: true, message: "", max_length: 16_384, images_available: true, ...DEFAULT_IMAGE_LIMITS });
  vi.mocked(uploadImageChunk).mockImplementation(async (request) => ({ upload_id: request.upload_id, received_bytes: request.total_bytes, image_id: "uploaded" }));
  vi.mocked(releaseImageUpload).mockResolvedValue();
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({ request_id: request.request_id, status: "accepted", message: "Sent" }));
});
afterEach(() => { cleanup(); });
describe("composer image messages", () => {
  it("uploads only on send, supports image-only input and clears after acceptance", async () => {
    render(<SessionComposer session={session} />); await ready(); attach();
    expect(uploadImageChunk).not.toHaveBeenCalled();
    expect(screen.getByAltText("screen.png")).toHaveAttribute("src", "blob:draft");
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled(); clickSend();
    await waitFor(() => expect(submitSessionInput).toHaveBeenCalledOnce());
    expect(submitSessionInput).toHaveBeenCalledWith({ session_key: session.session_key, request_id: expect.any(String), text: "", image_ids: ["uploaded"] }, send);
    await waitFor(() => expect(screen.queryByAltText("screen.png")).not.toBeInTheDocument());
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:draft");
    const upload = vi.mocked(uploadImageChunk).mock.calls[0][0];
    expect(releaseImageUpload).toHaveBeenCalledWith({ target: upload.target, upload_id: upload.upload_id }, vi.mocked(captureTransport).mock.results[0].value.release);
  });
  it("retains per-session image drafts and releases removed or unmounted previews", async () => {
    const view = render(<SessionComposer session={session} />); await ready(); attach();
    view.rerender(<SessionComposer session={other} />); await ready();
    expect(screen.queryByAltText("screen.png")).not.toBeInTheDocument();
    view.rerender(<SessionComposer session={session} />); await ready();
    expect(screen.getByAltText("screen.png")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Remove screen.png" }));
    expect(URL.revokeObjectURL).toHaveBeenCalledOnce();
    attach(); view.unmount(); expect(URL.revokeObjectURL).toHaveBeenCalledTimes(2);
  });
  it("retains files on uncertain delivery and prevents another provider send until explicit editing", async () => {
    vi.mocked(submitSessionInput).mockRejectedValue(new Error("Disconnected"));
    render(<SessionComposer session={session} />); await ready(); attach(); clickSend();
    await screen.findByRole("button", { name: "Edit message" });
    expect(screen.getByAltText("screen.png")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Remove screen.png" })).toBeDisabled();
    fireEvent.submit(screen.getByRole("form", { name: "Send a message to this session" }));
    expect(submitSessionInput).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Edit message" }));
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
  });
  it("treats a failed upload as safely unsent and preserves editable attachments", async () => {
    vi.mocked(uploadImageChunk).mockRejectedValue(new Error("No connection"));
    render(<SessionComposer session={session} />); await ready(); attach(); clickSend();
    await screen.findByText(/Could not upload images/);
    expect(submitSessionInput).not.toHaveBeenCalled();
    expect(screen.getByAltText("screen.png")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Remove screen.png" })).toBeEnabled();
  });
  it("rejects a late upload when the selected host changes", async () => {
    let resolve!: (value: UploadImageChunkResponse) => void;
    vi.mocked(uploadImageChunk).mockReturnValue(new Promise((done) => { resolve = done; }));
    const view = render(<SessionComposer session={session} />); await ready(); attach(); clickSend();
    await waitFor(() => expect(uploadImageChunk).toHaveBeenCalledOnce());
    const request = vi.mocked(uploadImageChunk).mock.calls[0][0];
    vi.mocked(viewerStorageScope).mockReturnValue("host-two"); view.rerender(<SessionComposer session={session} />); await ready();
    await act(async () => resolve({ upload_id: request.upload_id, received_bytes: request.total_bytes, image_id: "old-host-image" }));
    expect(submitSessionInput).not.toHaveBeenCalled();
    expect(releaseImageUpload).toHaveBeenCalledWith({ target: request.target, upload_id: request.upload_id }, expect.any(Function));
    expect(screen.queryByAltText("screen.png")).not.toBeInTheDocument();
  });
  it("does not offer attachments without the host capability", async () => {
    vi.mocked(getSessionInputStatus).mockResolvedValue({ available: true, message: "", max_length: 10 });
    render(<SessionComposer session={session} />); await ready();
    expect(screen.queryByRole("button", { name: "Attach images" })).not.toBeInTheDocument();
    fireEvent.paste(screen.getByRole("textbox"), { clipboardData: { files: [new File(["png"], "a.png", { type: "image/png" })] } });
    expect(screen.queryByRole("list", { name: "Attached images" })).not.toBeInTheDocument();
  });
});

it("releases every safely unsent upload before an explicit retry", async () => {
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({ request_id: request.request_id, status: "not_sent", message: "Session is busy" }));
  render(<SessionComposer session={session} />); await ready(); attach(); clickSend();
  await waitFor(() => expect(releaseImageUpload).toHaveBeenCalledTimes(1));
  expect(screen.getByAltText("screen.png")).toBeInTheDocument();
  clickSend();
  await waitFor(() => expect(releaseImageUpload).toHaveBeenCalledTimes(2));
  const uploads = vi.mocked(uploadImageChunk).mock.calls.map(([request]) => request);
  expect(uploads[0].upload_id).not.toBe(uploads[1].upload_id);
  expect(vi.mocked(releaseImageUpload).mock.calls.map(([request]) => request.upload_id)).toEqual(uploads.map((request) => request.upload_id));
});

it("uses the original cleanup transport when its host closes during upload", async () => {
  let close!: () => void;
  let resolve!: (value: UploadImageChunkResponse) => void;
  const cleanup_send = vi.fn(async () => {});
  vi.mocked(captureTransport).mockReturnValue({ invoke: send, listen: vi.fn(), release: cleanup_send,
    on_close: (callback) => { close = callback; return () => {}; } });
  vi.mocked(uploadImageChunk).mockReturnValue(new Promise((done) => { resolve = done; }));
  render(<SessionComposer session={session} />); await ready(); attach(); clickSend();
  await waitFor(() => expect(uploadImageChunk).toHaveBeenCalledOnce());
  const request = vi.mocked(uploadImageChunk).mock.calls[0][0];
  act(() => close());
  expect(releaseImageUpload).toHaveBeenCalledWith({ target: request.target, upload_id: request.upload_id }, cleanup_send);
  await act(async () => resolve({ upload_id: request.upload_id, received_bytes: request.total_bytes, image_id: "unused" }));
  expect(submitSessionInput).not.toHaveBeenCalled();
});

it("places attachment previews above prompt text and keeps the picker below it", async () => {
  render(<SessionComposer session={session} />); await ready(); attach();
  const previews = screen.getByRole("list", { name: "Attached images" });
  const prompt = screen.getByRole("textbox", { name: "Message this session" });
  const picker = screen.getByRole("button", { name: "Attach images" });
  expect(previews.compareDocumentPosition(prompt) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(prompt.compareDocumentPosition(picker) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
});
