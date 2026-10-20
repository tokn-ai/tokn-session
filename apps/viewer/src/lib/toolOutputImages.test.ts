import { describe, expect, it } from "vitest";
import { toolOutputImages } from "./toolOutputImages";
import type { EventDetail, EventSummary } from "./types";
const image = { image_id: "image", mime_type: "image/png", name: null, availability: "available" as const };
const event = { event_key: "operation", type: "tool_call", is_hidden: false } as EventSummary;
const detail: EventDetail = { event_key: "operation", is_hidden: false, native: null, tool_output: null, event: { type: "tool_operation", images: [image] } };
describe("typed tool output image metadata", () => {
  it("accepts only complete display metadata and does not treat raw image data as a resource", () => {
    const result = toolOutputImages(event, { ...detail, event: { type: "tool_operation", images: [
      image, { type: "image", data: "bytes" }, { ...image, image_id: "" }, { ...image, availability: "guess" }, { ...image, source_event_key: 2 },
    ] } });
    expect(result.images).toEqual([image]);
  });
  it("rejects stale or mismatched detail and trusts explicit empty live metadata", () => {
    expect(toolOutputImages(event, { ...detail, event_key: "another" }).images).toEqual([]);
    expect(toolOutputImages({ ...event, content_revision: "new" }, { ...detail, content_revision: "old" }).images).toEqual([]);
    expect(toolOutputImages({ ...event, images: [] }, detail).images).toEqual([]);
  });
  it("never shows hidden tool attachments", () => {
    expect(toolOutputImages({ ...event, images: [image], is_hidden: true }, detail).images).toEqual([]);
    expect(toolOutputImages(event, { ...detail, is_hidden: true }).images).toEqual([]);
  });
});
