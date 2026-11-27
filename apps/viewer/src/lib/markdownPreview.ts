import type { Nodes } from "mdast";
import { useMemo } from "react";
import { markdownParser } from "./markdownParser";
import { sessionDisplayTitle } from "./state";
import type { SessionSummary } from "./types";

function plainText(node: Nodes): string {
  if ("value" in node) return node.value;
  if ("alt" in node) return node.alt ?? "";
  if (node.type === "break") return "\n";
  if (node.type === "footnoteReference") return `[^${node.label ?? node.identifier}]`;
  if (!("children" in node) || node.type === "footnoteDefinition") return "";
  const block = node.type === "root" || node.type === "list" || node.type === "listItem"
    || node.type === "blockquote" || node.type === "table";
  return node.children.map(plainText).join(block ? "\n" : node.type === "tableRow" ? " · " : "");
}

/** A single readable line for catalog rows. Never change the indexed preview or its search text. */
export function markdownPreview(source: string | null | undefined): string {
  if (!source) return "";
  const text = plainText(markdownParser.parse(source));
  return text.split(/\r?\n/).find((line) => line.trim())?.trim() ?? "";
}

export function useSessionRowText(session: SessionSummary, preview: string | null | undefined) {
  const { title, preview: firstPrompt } = session;
  const fallback = title?.trim() ? undefined : firstPrompt;
  const fallbackTitle = useMemo(() => markdownPreview(fallback), [fallback]);
  const line = useMemo(() => preview === fallback ? fallbackTitle : markdownPreview(preview), [preview, fallback, fallbackTitle]);
  return { title: fallbackTitle || sessionDisplayTitle(session), preview: line };
}
