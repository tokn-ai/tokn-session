import type { EventDetail, EventSummary, JsonValue, MessageImageSummary } from "./types";

function imageSummary(value: JsonValue): MessageImageSummary | null {
  if (!value || typeof value !== "object" || Array.isArray(value)
    || typeof value.image_id !== "string" || !value.image_id
    || value.availability !== "available" && value.availability !== "unavailable"
    || value.mime_type !== null && typeof value.mime_type !== "string"
    || value.name !== null && typeof value.name !== "string"
    || value.source_event_key !== undefined && (typeof value.source_event_key !== "string" || !value.source_event_key)) return null;
  return {
    image_id: value.image_id,
    mime_type: value.mime_type,
    name: value.name,
    availability: value.availability,
    ...(typeof value.source_event_key === "string" ? { source_event_key: value.source_event_key } : {}),
  };
}

/** Compact live metadata owns replacements; older detail cannot revive removed images. */
export function toolOutputImages(event: EventSummary, detail: EventDetail | null): { images: MessageImageSummary[]; revision: string | null } {
  if (event.is_hidden || detail?.is_hidden) return { images: [], revision: null };
  if (event.images !== undefined) return { images: event.images, revision: event.content_revision ?? null };
  const revision = event.content_revision ?? null;
  if (!detail || detail.event_key !== event.event_key
    || revision !== null && detail.content_revision !== revision) return { images: [], revision };
  const value = detail.event;
  if (!value || typeof value !== "object" || Array.isArray(value) || !Array.isArray(value.images)) return { images: [], revision };
  return { images: value.images.slice(0, 64).flatMap((value) => { const image = imageSummary(value); return image ? [image] : []; }),
    revision: detail.content_revision ?? revision };
}
