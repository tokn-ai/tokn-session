import type { Nodes } from "mdast";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import { unified } from "unified";
import type { MessageImageSummary } from "./types";

const parser = unified().use(remarkParse).use(remarkGfm);

export function markdownImageRangeKey(start: number | undefined, end: number | undefined): string | null {
  if (typeof start !== "number" || typeof end !== "number"
    || !Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 0 || end <= start) return null;
  return `${start}:${end}`;
}

/** Match parsed image spans to host-owned metadata; destinations never authorize a read. */
export function bindMarkdownImages(content: string, images: MessageImageSummary[]): ReadonlyMap<string, MessageImageSummary> {
  const candidates = new Map<string, MessageImageSummary | null>();
  // remark removes one leading BOM before assigning its UTF-16 positions.
  const offset = content.startsWith("\uFEFF") ? 1 : 0;
  for (const image of images) {
    const range = image.markdown_range;
    if (!range || range.end > content.length) continue;
    const key = markdownImageRangeKey(range.start - offset, range.end - offset);
    if (key) candidates.set(key, candidates.has(key) ? null : image);
  }
  if (!candidates.size) return new Map();
  const tree = parser.parse(content);
  const definitions = new Set<string>();
  function visit(node: Nodes, apply: (node: Nodes) => void) {
    apply(node);
    if ("children" in node) for (const child of node.children) visit(child, apply);
  }
  visit(tree, (node) => { if (node.type === "definition") definitions.add(node.identifier); });
  const bound = new Map<string, MessageImageSummary>();
  visit(tree, (node) => {
    if (node.type !== "image" && !(node.type === "imageReference" && definitions.has(node.identifier))) return;
    const key = markdownImageRangeKey(node.position?.start.offset, node.position?.end.offset);
    const image = key ? candidates.get(key) : null;
    if (key && image) bound.set(key, image);
  });
  return bound;
}
