import { type MouseEvent, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { currentImageCache } from "../lib/imageCache";
import { errorMessage } from "../lib/state";
import type { MessageImageSummary } from "../lib/types";
import "./MessageImages.css";

interface ImageProps {
  session_key: string;
  event_key: string;
  revision: string | null;
  image: MessageImageSummary;
}

function ImageResource({ session_key, event_key, revision, image, full = false, on_open }: ImageProps & { full?: boolean; on_open?: () => void }) {
  const container = useRef<HTMLSpanElement>(null);
  const [visible, setVisible] = useState(full || typeof IntersectionObserver === "undefined");
  const [retry, setRetry] = useState(0);

  // Image-bearing rows capture the selected host, never a global URL from Markdown.
  const current = currentImageCache();
  const source_event_key = image.source_event_key ?? event_key;
  const source = useMemo(() => ({ session_key, event_key: source_event_key, image_id: image.image_id, variant: full ? "full" as const : "thumbnail" as const }),
    [session_key, source_event_key, image.image_id, full]);
  const observation = useMemo(() => ({ source, revision, cache: current.cache }), [source, revision, current.cache]);
  const [result, setResult] = useState<{ observation: typeof observation; url: string | null; error: string | null } | null>(null);
  const visible_result = result?.observation === observation ? result : null;
  useEffect(() => {
    if (visible || !container.current) return;
    const observer = new IntersectionObserver((entries) => { if (entries.some((entry) => entry.isIntersecting)) setVisible(true); }, { rootMargin: "160px" });
    observer.observe(container.current);
    return () => observer.disconnect();
  }, [visible]);
  useEffect(() => {
    setResult({ observation, url: null, error: null });
    if (!visible || image.availability !== "available") return;
    let cancelled = false;
    const resource = current.cache.acquire(source, revision);
    void resource.ready.then((url) => { if (!cancelled) setResult({ observation, url, error: null }); })
      .catch((failure) => { if (!cancelled) setResult({ observation, url: null, error: errorMessage(failure) }); });
    return () => { cancelled = true; resource.release(); };
  }, [current.cache, image.availability, revision, source, visible, retry, observation]);
  const label = image.name || "Recorded image";
  return <span ref={container} className={full ? "image-preview__body" : "message-image"}>
    {image.availability !== "available" ? <span className="message-image__status">{label}<br />Image unavailable</span>
      : visible_result?.url ? full ? <img src={visible_result.url} alt={label} />
        : <button aria-label={`Preview ${label}`} onClick={on_open} type="button"><img src={visible_result.url} alt={label} /></button>
        : <span className="message-image__status" role="status">{visible_result?.error ? <>{label}<br />{visible_result.error}<br /><button type="button" className="text-button" onClick={() => setRetry((value) => value + 1)}>Retry image</button></> : visible ? "Loading image…" : "Image"}</span>}
  </span>;
}

function isDialogBackdrop(event: MouseEvent<HTMLDialogElement>): boolean {
  if (event.target !== event.currentTarget) return false;
  const rect = event.currentTarget.getBoundingClientRect();
  return event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom;
}

function ImagePreview({ on_close, ...props }: ImageProps & { on_close: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const backdrop_pressed = useRef(false);
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog.current?.showModal();
    return () => { dialog.current?.close(); if (opener?.isConnected) opener.focus({ preventScroll: true }); };
  }, []);
  return createPortal(<dialog ref={dialog} className="image-preview" aria-label="Image preview"
    onPointerDown={(event) => { backdrop_pressed.current = event.button === 0 && isDialogBackdrop(event); }}
    onPointerCancel={() => { backdrop_pressed.current = false; }}
    onClick={(event) => {
      const started_outside = backdrop_pressed.current;
      backdrop_pressed.current = false;
      if (started_outside && event.button === 0 && isDialogBackdrop(event)) on_close();
    }}
    onCancel={(event) => { event.preventDefault(); on_close(); }}>
    <header><span>{props.image.name || "Image preview"}</span><button type="button" className="text-button" aria-label="Close image preview" onClick={on_close}>Close</button></header>
    <ImageResource {...props} full />
  </dialog>, document.body);
}

export function MessageImages({ session_key, event_key, revision = null, images, aria_label = "Message images" }: {
  session_key: string; event_key: string; revision?: string | null; images: MessageImageSummary[]; aria_label?: string;
}) {
  const [preview, setPreview] = useState<string | null>(null);
  const imageKey = (image: MessageImageSummary) => JSON.stringify([image.source_event_key ?? event_key, image.image_id]);
  const selected = images.find((image) => imageKey(image) === preview);
  useEffect(() => { setPreview(null); }, [session_key, event_key, revision]);
  return <div className="message-images" aria-label={aria_label}>
    {images.map((image) => <ImageResource key={imageKey(image)} session_key={session_key} event_key={event_key} revision={revision} image={image} on_open={() => setPreview(imageKey(image))} />)}
    {selected ? <ImagePreview session_key={session_key} event_key={event_key} revision={revision} image={selected} on_close={() => setPreview(null)} /> : null}
  </div>;
}

/** Paragraph-safe image presentation for an authorized Markdown image span. */
export function InlineMessageImage({ session_key, event_key, revision, image, alt_text }: ImageProps & { alt_text?: string }) {
  const [preview, setPreview] = useState(false);
  const label = alt_text ? Array.from(alt_text.replace(/[\u0000-\u001f\u007f-\u009f]/g, "")).slice(0, 255).join("") : null;
  const display_image = label ? { ...image, name: label } : image;
  useEffect(() => { setPreview(false); }, [session_key, event_key, revision, image.image_id, image.source_event_key]);
  return <span className="message-image-inline">
    <ImageResource session_key={session_key} event_key={event_key} revision={revision} image={display_image} on_open={() => setPreview(true)} />
    {preview ? <ImagePreview session_key={session_key} event_key={event_key} revision={revision} image={display_image} on_close={() => setPreview(false)} /> : null}
  </span>;
}
