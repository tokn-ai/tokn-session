import { loadSessionImage } from "./tauri";
import { captureTransport, viewerStorageScope, type CommandInvoker } from "./transport";
import type { LoadSessionImageRequest } from "./types";
import { DEFAULT_IMAGE_LIMITS } from "./imageDrafts";

interface CachedImage {
  promise: Promise<string>;
  url: string | null;
  bytes: number;
  users: number;
}

/** Independent of timeline coverage: bounded, de-duplicated byte resources. */
export class ImageBlobCache {
  private entries = new Map<string, CachedImage>();
  private bytes = 0;
  private epoch = 0;
  constructor(private send: CommandInvoker, private max_bytes = 40 * 1024 * 1024, private max_entries = 64) {}

  acquire(request: LoadSessionImageRequest, revision: string | null = null): { ready: Promise<string>; release: () => void } {
    const key = JSON.stringify([request.session_key, request.event_key, revision, request.image_id, request.variant]);
    let entry = this.entries.get(key);
    if (entry) { this.entries.delete(key); this.entries.set(key, entry); }
    else {
      // Active resources are pinned. Refuse extra requests rather than allowing
      // a long expanded history to grow an unbounded set of pending reads.
      for (const [old_key, old] of this.entries) {
        if (this.entries.size < this.max_entries) break;
        if (old.users) continue;
        this.entries.delete(old_key);
        if (old.url) URL.revokeObjectURL(old.url);
        this.bytes -= old.bytes;
      }
      if (this.entries.size >= this.max_entries) return {
        ready: Promise.reject(new Error("Too many images are open. Close a group and retry.")), release() {},
      };
      const epoch = this.epoch;
      const value: CachedImage = { promise: Promise.resolve(""), url: null, bytes: 0, users: 0 };
      entry = value;
      this.entries.set(key, value);
      value.promise = loadSessionImage(request, this.send).then((response) => {
        if (epoch !== this.epoch || this.entries.get(key) !== value) throw new Error("Machine disconnected");
        if (!DEFAULT_IMAGE_LIMITS.image_mime_types.includes(response.mime_type)
          || response.data_base64.length > Math.ceil(DEFAULT_IMAGE_LIMITS.max_image_bytes * 4 / 3) + 4) {
          throw new Error("This image format or size cannot be displayed.");
        }
        const decoded = atob(response.data_base64);
        const data = new Uint8Array(decoded.length);
        for (let index = 0; index < decoded.length; index += 1) data[index] = decoded.charCodeAt(index);
        this.evict(data.length, key);
        if (this.bytes + data.length > this.max_bytes) throw new Error("Close another image preview and try again.");
        value.url = URL.createObjectURL(new Blob([data], { type: response.mime_type }));
        value.bytes = data.length;
        this.bytes += value.bytes;
        return value.url;
      }).catch((failure) => {
        if (this.entries.get(key) === value) this.entries.delete(key);
        throw failure;
      });
    }
    entry.users += 1;
    let released = false;
    return { ready: entry.promise, release: () => {
      if (released) return;
      released = true;
      entry.users = Math.max(0, entry.users - 1);
      this.evict(0);
    } };
  }

  private evict(additional: number, preserve?: string) {
    for (const [key, entry] of this.entries) {
      if (this.bytes + additional <= this.max_bytes && this.entries.size <= this.max_entries) break;
      if (entry.users || key === preserve) continue;
      this.entries.delete(key);
      if (entry.url) URL.revokeObjectURL(entry.url);
      this.bytes -= entry.bytes;
    }
  }

  clear() {
    this.epoch += 1;
    for (const entry of this.entries.values()) if (entry.url) URL.revokeObjectURL(entry.url);
    this.entries.clear();
    this.bytes = 0;
  }
}

const machine_caches = new Map<string, ImageBlobCache>();
export function currentImageCache(): { scope: string; cache: ImageBlobCache } {
  const scope = viewerStorageScope();
  let cache = machine_caches.get(scope);
  if (!cache) {
    const transport = captureTransport();
    cache = new ImageBlobCache(transport.invoke);
    machine_caches.set(scope, cache);
    const owned = cache;
    transport.on_close(() => {
      owned.clear();
      if (machine_caches.get(scope) === owned) machine_caches.delete(scope);
    });
  }
  return { scope, cache };
}
