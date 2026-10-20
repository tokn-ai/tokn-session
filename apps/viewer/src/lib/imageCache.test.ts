import { beforeEach, describe, expect, it, vi } from "vitest";
import { ImageBlobCache } from "./imageCache";
import { loadSessionImage } from "./tauri";
import type { CommandInvoker } from "./transport";
import type { LoadSessionImageResponse } from "./types";
vi.mock("./tauri", () => ({ loadSessionImage: vi.fn() }));
const request = { session_key: "same", event_key: "windows.v1.generation.1", image_id: "0", variant: "thumbnail" as const };
const send = vi.fn() as CommandInvoker;
function response(text = "image"): LoadSessionImageResponse { return { mime_type: "image/png", data_base64: btoa(text), width: 1, height: 1 }; }
beforeEach(() => {
  vi.resetAllMocks();
  Object.defineProperties(URL, {
    createObjectURL: { configurable: true, value: vi.fn(() => `blob:${Math.random()}`) },
    revokeObjectURL: { configurable: true, value: vi.fn() },
  });
  vi.mocked(loadSessionImage).mockResolvedValue(response());
});
describe("image resource cache", () => {
  it("deduplicates equal resources while separating variants, revisions and sessions", async () => {
    const cache = new ImageBlobCache(send);
    const first = cache.acquire(request, "a");
    const second = cache.acquire(request, "a");
    expect(await first.ready).toBe(await second.ready);
    await cache.acquire({ ...request, variant: "full" }, "a").ready;
    await cache.acquire(request, "b").ready;
    await cache.acquire({ ...request, session_key: "other" }, "a").ready;
    expect(loadSessionImage).toHaveBeenCalledTimes(4);
    expect(loadSessionImage).toHaveBeenCalledWith(request, send);
    first.release(); second.release(); cache.clear();
    expect(URL.revokeObjectURL).toHaveBeenCalledTimes(4);
  });
  it("evicts unreferenced bytes and never shares a same-key resource between hosts", async () => {
    const first_host = new ImageBlobCache(send, 6);
    const first = first_host.acquire(request);
    await first.ready; first.release();
    await first_host.acquire({ ...request, image_id: "1" }).ready;
    expect(URL.revokeObjectURL).toHaveBeenCalledOnce();
    const second_host = new ImageBlobCache(vi.fn() as CommandInvoker);
    await second_host.acquire(request).ready;
    expect(loadSessionImage).toHaveBeenCalledTimes(3);
    first_host.clear(); second_host.clear();
  });
  it("discards a late disconnected response before allocating its URL", async () => {
    let resolve!: (value: LoadSessionImageResponse) => void;
    vi.mocked(loadSessionImage).mockReturnValue(new Promise((done) => { resolve = done; }));
    const cache = new ImageBlobCache(send);
    const resource = cache.acquire(request);
    cache.clear(); resolve(response());
    await expect(resource.ready).rejects.toThrow("disconnected");
    expect(URL.createObjectURL).not.toHaveBeenCalled();
  });
  it("does not render active image formats and allows an explicit retry after failure", async () => {
    vi.mocked(loadSessionImage).mockResolvedValueOnce({ ...response(), mime_type: "image/svg+xml" });
    const cache = new ImageBlobCache(send);
    await expect(cache.acquire(request).ready).rejects.toThrow("format");
    await expect(cache.acquire(request).ready).resolves.toMatch(/^blob:/);
    expect(URL.createObjectURL).toHaveBeenCalledOnce(); cache.clear();
  });
});

it("bounds pending requests even when every resource is pinned", async () => {
  vi.mocked(loadSessionImage).mockReturnValue(new Promise(() => {}));
  const cache = new ImageBlobCache(send, 100, 1);
  const pinned = cache.acquire(request);
  await expect(cache.acquire({ ...request, image_id: "extra" }).ready).rejects.toThrow("Too many images");
  expect(loadSessionImage).toHaveBeenCalledOnce();
  pinned.release(); cache.clear();
});
