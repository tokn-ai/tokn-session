import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { addImageDrafts, DEFAULT_IMAGE_LIMITS, disposeImageDrafts, IMAGE_CHUNK_BYTES, imageLimits, uploadImageDrafts } from "./imageDrafts";
import { releaseImageUpload, uploadImageChunk } from "./tauri";
import type { CommandInvoker } from "./transport";
vi.mock("./tauri", () => ({ releaseImageUpload: vi.fn(), uploadImageChunk: vi.fn() }));
const send = vi.fn() as CommandInvoker;
const target = { type: "session" as const, session_key: "codex:one" };

beforeEach(() => {
  vi.resetAllMocks();
  vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: vi.fn(() => "blob:draft"), revokeObjectURL: vi.fn() }));
  vi.mocked(releaseImageUpload).mockResolvedValue();
  vi.mocked(uploadImageChunk).mockImplementation(async (request) => ({ upload_id: request.upload_id,
    received_bytes: request.offset + atob(request.data_base64).length,
    image_id: request.offset + atob(request.data_base64).length === request.total_bytes ? "opaque-image" : null }));
});
afterEach(() => { vi.unstubAllGlobals(); });

describe("image drafts", () => {
  it("keeps old hosts text-only and applies advertised bounds", () => {
    expect(imageLimits({ available: true, max_length: 10, message: "" })).toBeNull();
    expect(imageLimits({ available: true, max_length: 10, message: "", images_available: true,
      max_images: 2, max_image_bytes: 100, max_total_image_bytes: 200, image_mime_types: ["image/png", "image/svg+xml"] }))
      .toEqual({ max_images: 2, max_image_bytes: 100, max_total_image_bytes: 200, image_mime_types: ["image/png"] });
  });
  it("validates count, format and aggregate bytes without allocating rejected previews", () => {
    const good = new File(["123"], "ok.png", { type: "image/png" });
    const invalid = new File(["<svg>"], "bad.svg", { type: "image/svg+xml" });
    const result = addImageDrafts([], [invalid, good, good, good], { ...DEFAULT_IMAGE_LIMITS, max_images: 2, max_total_image_bytes: 4 });
    expect(result.images).toHaveLength(1);
    expect(result.error).toContain("total");
    expect(URL.createObjectURL).toHaveBeenCalledTimes(1);
    disposeImageDrafts(result.images);
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:draft");
  });
  it("uploads bounded chunks on the original session transport and returns only opaque IDs", async () => {
    const bytes = new Uint8Array(IMAGE_CHUNK_BYTES + 9).fill(7);
    const { images } = addImageDrafts([], [new File([bytes], "large.png", { type: "image/png" })], DEFAULT_IMAGE_LIMITS);
    expect(uploadImageChunk).not.toHaveBeenCalled();
    await expect(uploadImageDrafts(images, target, send)).resolves.toEqual(["opaque-image"]);
    expect(uploadImageChunk).toHaveBeenCalledTimes(2);
    const calls = vi.mocked(uploadImageChunk).mock.calls;
    expect(calls[0][0].offset).toBe(0);
    expect(atob(calls[0][0].data_base64)).toHaveLength(IMAGE_CHUNK_BYTES);
    expect(calls[1][0]).toMatchObject({ target, offset: IMAGE_CHUNK_BYTES, total_bytes: bytes.length });
    expect(calls[0][1]).toBe(send);
    expect(calls[1][0].upload_id).toBe(calls[0][0].upload_id);
  });
  it("releases a failed upload and never retries an uncertain provider action", async () => {
    const { images } = addImageDrafts([], [new File(["png"], "a.png", { type: "image/png" })], DEFAULT_IMAGE_LIMITS);
    vi.mocked(uploadImageChunk).mockRejectedValue(new Error("disconnected"));
    await expect(uploadImageDrafts(images, target, send)).rejects.toThrow("disconnected");
    expect(uploadImageChunk).toHaveBeenCalledOnce();
    expect(releaseImageUpload).toHaveBeenCalledWith({ target, upload_id: expect.any(String) }, send);
  });
  it("stops before sending any bytes if its machine lifetime has ended", async () => {
    const { images } = addImageDrafts([], [new File(["png"], "a.png", { type: "image/png" })], DEFAULT_IMAGE_LIMITS);
    await expect(uploadImageDrafts(images, target, send, () => false)).rejects.toThrow("machine changed");
    expect(uploadImageChunk).not.toHaveBeenCalled();
  });
});
