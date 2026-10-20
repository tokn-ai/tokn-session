import { useEffect, useMemo, useRef, useState } from "react";
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
  const container = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(full || typeof IntersectionObserver === "undefined");
  const [retry, setRetry] = useState(0);
  const [result, setResult] = useState<{ url: string | null; error: string | null }>({ url: null, error: null });
  // Image-bearing rows capture the selected host, never a global URL from Markdown.
  const current = currentImageCache();
  const source = useMemo(() => ({ session_key, event_key, image_id: image.image_id, variant: full ? "full" as const : "thumbnail" as const }),
    [session_key, event_key, image.image_id, full]);
  useEffect(() => {
    if (visible || !container.current) return;
    const observer = new IntersectionObserver((entries) => { if (entries.some((entry) => entry.isIntersecting)) setVisible(true); }, { rootMargin: "160px" });
    observer.observe(container.current);
    return () => observer.disconnect();
  }, [visible]);
  useEffect(() => {
    setResult({ url: null, error: null });
    if (!visible || image.availability !== "available") return;
    let cancelled = false;
    const resource = current.cache.acquire(source, revision);
    void resource.ready.then((url) => { if (!cancelled) setResult({ url, error: null }); })
      .catch((failure) => { if (!cancelled) setResult({ url: null, error: errorMessage(failure) }); });
    return () => { cancelled = true; resource.release(); };
  }, [current.cache, image.availability, revision, source, visible, retry]);
  const label = image.name || "Recorded image";
  return <div ref={container} className={full ? "image-preview__body" : "message-image"}>
    {image.availability !== "available" ? <span className="message-image__status">{label}<br />Image unavailable</span>
      : result.url ? full ? <img src={result.url} alt={label} />
        : <button aria-label={`Preview ${label}`} onClick={on_open} type="button"><img src={result.url} alt={label} /></button>
        : <span className="message-image__status" role="status">{result.error ? <>{label}<br />{result.error}<br /><button type="button" className="text-button" onClick={() => setRetry((value) => value + 1)}>Retry image</button></> : visible ? "Loading image…" : "Image"}</span>}
  </div>;
}

function ImagePreview({ on_close, ...props }: ImageProps & { on_close: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog.current?.showModal();
    return () => { dialog.current?.close(); if (opener?.isConnected) opener.focus({ preventScroll: true }); };
  }, []);
  return createPortal(<dialog ref={dialog} className="image-preview" aria-label="Image preview" onCancel={(event) => { event.preventDefault(); on_close(); }}>
    <header><span>{props.image.name || "Image preview"}</span><button type="button" className="text-button" aria-label="Close image preview" onClick={on_close}>Close</button></header>
    <ImageResource {...props} full />
  </dialog>, document.body);
}

export function MessageImages({ session_key, event_key, revision = null, images }: {
  session_key: string; event_key: string; revision?: string | null; images: MessageImageSummary[];
}) {
  const [preview, setPreview] = useState<string | null>(null);
  const selected = images.find((image) => image.image_id === preview);
  useEffect(() => { setPreview(null); }, [session_key, event_key, revision]);
  return <div className="message-images" aria-label="Message images">
    {images.map((image) => <ImageResource key={image.image_id} session_key={session_key} event_key={event_key} revision={revision} image={image} on_open={() => setPreview(image.image_id)} />)}
    {selected ? <ImagePreview session_key={session_key} event_key={event_key} revision={revision} image={selected} on_close={() => setPreview(null)} /> : null}
  </div>;
}
