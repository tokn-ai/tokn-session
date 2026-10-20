import { useRef, type ClipboardEvent, type DragEvent } from "react";
import type { ImageDraft, ImageLimits } from "../lib/imageDrafts";
import "./MessageImages.css";

export function imagePaste(event: ClipboardEvent, add: (files: File[]) => void) {
  const files = Array.from(event.clipboardData.files).filter((file) => file.type.startsWith("image/"));
  if (files.length) { event.preventDefault(); add(files); }
}
export function imageDrop(event: DragEvent, add: (files: File[]) => void) {
  if (!event.dataTransfer.types.includes("Files")) return;
  event.preventDefault();
  add(Array.from(event.dataTransfer.files));
}

export function ImageAttachments({ images, limits, disabled, on_add, on_remove }: {
  images: ImageDraft[];
  limits: ImageLimits | null;
  disabled: boolean;
  on_add: (files: File[]) => void;
  on_remove: (draft_id: string) => void;
}) {
  const picker = useRef<HTMLInputElement>(null);
  return <div className="image-attachments">
    {images.length ? <ul className="image-attachments__previews" aria-label="Attached images">
      {images.map((image) => <li key={image.draft_id}>
        <img alt={image.file.name || "Attached image"} src={image.url} />
        <span title={image.file.name}>{image.file.name || "Image"}</span>
        <button type="button" disabled={disabled} aria-label={`Remove ${image.file.name || "image"}`} onClick={() => on_remove(image.draft_id)}>×</button>
      </li>)}
    </ul> : null}
    {limits ? <>
      <input className="sr-only" tabIndex={-1} ref={picker} aria-label="Image files" type="file" multiple accept={limits.image_mime_types.join(",")}
        disabled={disabled} onChange={(event) => {
          on_add(Array.from(event.target.files ?? []));
          event.target.value = "";
        }} />
      <button className="image-attachments__add" type="button" disabled={disabled || images.length >= limits.max_images}
        title={`Attach up to ${limits.max_images} images, ${Math.floor(limits.max_image_bytes / 1024 / 1024)} MB each. You can also paste or drop images.`}
        onClick={() => picker.current?.click()}><span aria-hidden="true">＋</span> Attach images</button>
    </> : null}
  </div>;
}
