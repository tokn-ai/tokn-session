import { createUuid } from "./id";
import { releaseImageUpload, uploadImageChunk } from "./tauri";
import type { CommandInvoker } from "./transport";
import type { ImageUploadTarget, ReleaseImageUploadRequest, SessionInputStatus } from "./types";

export interface ImageDraft { draft_id: string; file: File; url: string }
export interface ImageLimits { max_images: number; max_image_bytes: number; max_total_image_bytes: number; image_mime_types: string[] }
export const DEFAULT_IMAGE_LIMITS: ImageLimits = {
  max_images: 4, max_image_bytes: 8 * 1024 * 1024, max_total_image_bytes: 24 * 1024 * 1024,
  image_mime_types: ["image/png", "image/jpeg", "image/webp", "image/gif"],
};
export const IMAGE_CHUNK_BYTES = 256 * 1024;

export function imageLimits(status: SessionInputStatus | null): ImageLimits | null {
  if (!status?.available || !status.images_available) return null;
  return {
    max_images: Math.min(status.max_images ?? 0, DEFAULT_IMAGE_LIMITS.max_images),
    max_image_bytes: Math.min(status.max_image_bytes ?? 0, DEFAULT_IMAGE_LIMITS.max_image_bytes),
    max_total_image_bytes: Math.min(status.max_total_image_bytes ?? 0, DEFAULT_IMAGE_LIMITS.max_total_image_bytes),
    image_mime_types: (status.image_mime_types ?? []).filter((mime) => DEFAULT_IMAGE_LIMITS.image_mime_types.includes(mime)),
  };
}

export function addImageDrafts(existing: ImageDraft[], files: File[], limits: ImageLimits): { images: ImageDraft[]; error: string | null } {
  let total = existing.reduce((bytes, image) => bytes + image.file.size, 0);
  const added: ImageDraft[] = [];
  let error: string | null = null;
  for (const file of files) {
    if (!limits.image_mime_types.includes(file.type)) { error = "Choose a PNG, JPEG, WebP, or GIF image."; continue; }
    if (!file.size) { error = "Empty images cannot be attached."; continue; }
    if (existing.length + added.length >= limits.max_images) { error = `Attach up to ${limits.max_images} images.`; continue; }
    if (file.size > limits.max_image_bytes) { error = `Each image must be at most ${Math.floor(limits.max_image_bytes / 1024 / 1024)} MB.`; continue; }
    if (total + file.size > limits.max_total_image_bytes) { error = `Attachments must total at most ${Math.floor(limits.max_total_image_bytes / 1024 / 1024)} MB.`; continue; }
    try {
      added.push({ draft_id: createUuid(), file, url: URL.createObjectURL(file) });
      total += file.size;
    } catch { error = "Could not prepare this image. Try attaching it again."; }
  }
  return { images: [...existing, ...added], error };
}

export function disposeImageDrafts(images: ImageDraft[]) {
  for (const image of images) URL.revokeObjectURL(image.url);
}

export function readBlobBytes(blob: Blob): Promise<Uint8Array> {
  if (blob.arrayBuffer) return blob.arrayBuffer().then((buffer) => new Uint8Array(buffer));
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(new Uint8Array(reader.result as ArrayBuffer));
    reader.onerror = () => reject(reader.error ?? new Error("Image could not be read."));
    reader.readAsArrayBuffer(blob);
  });
}

function toBase64(bytes: Uint8Array): string {
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 8192) binary += String.fromCharCode(...bytes.subarray(offset, offset + 8192));
  return btoa(binary);
}

export type ImageUploadReceipt = ReleaseImageUploadRequest;
export type ImageReleaseInvoker = (command: string, payload?: Record<string, unknown>) => Promise<unknown>;

/** Drain owned tokens before awaiting cleanup, so close/finally cannot release twice. */
export async function releaseImageUploads(receipts: ImageUploadReceipt[], send: ImageReleaseInvoker) {
  await Promise.allSettled(receipts.splice(0).map((receipt) => releaseImageUpload(receipt, send)));
}

/**
 * Upload bytes once per attempt; provider input is a separate, never-replayed action.
 * A caller that records receipts owns their disposal, including partial failures.
 */
export async function uploadImageDrafts(images: ImageDraft[], target: ImageUploadTarget, send: CommandInvoker, is_current: () => boolean = () => true, on_upload?: (receipt: ImageUploadReceipt) => void): Promise<string[]> {
  const completed: string[] = [];
  const staged_uploads: string[] = [];
  try {
    for (const image of images) {
      if (!is_current()) throw new Error("The selected machine changed. Images were not sent.");
      const upload_id = createUuid();
      staged_uploads.push(upload_id);
      on_upload?.({ target, upload_id });
      let image_id: string | null = null;
      for (let offset = 0; offset < image.file.size; offset += IMAGE_CHUNK_BYTES) {
        if (!is_current()) throw new Error("The selected machine changed. Images were not sent.");
        const bytes = await readBlobBytes(image.file.slice(offset, offset + IMAGE_CHUNK_BYTES));
        if (!is_current()) throw new Error("The selected machine changed. Images were not sent.");
        const result = await uploadImageChunk({ target, upload_id, offset, total_bytes: image.file.size,
          mime_type: image.file.type, name: image.file.name || null, data_base64: toBase64(bytes) }, send);
        if (result.upload_id !== upload_id || result.received_bytes !== offset + bytes.length) throw new Error("The image upload was not confirmed. Try again.");
        image_id = result.image_id;
      }
      if (!image_id) throw new Error("The image upload did not complete. Try again.");
      completed.push(image_id);
    }
    return completed;
  } catch (failure) {
    if (!on_upload) for (const staged_id of staged_uploads) void releaseImageUpload({ target, upload_id: staged_id }, send).catch(() => {});
    throw failure;
  }
}
