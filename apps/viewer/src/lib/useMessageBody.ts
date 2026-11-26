import { useMemo, useState } from "react";
import { loadEventDetail } from "./tauri";
import type { EventSummary } from "./types";

class MessageSizeLimit extends Error {}

/** Keep a full body only for the mounted message and the exact revision that requested it. */
function messageSource(session_key: string | undefined, event_key: string, role: EventSummary["role"],
  summary: string, truncated: boolean, revision: string | null, readable: boolean) {
  let pending: Promise<string> | undefined;

  async function load() {
    if (!readable || !session_key) throw new Error("Full message unavailable.");
    const detail = await loadEventDetail({ session_key, event_key });
    const value = detail.event;
    if (!detail.is_hidden && detail.event_key === event_key && value && typeof value === "object"
      && !Array.isArray(value) && value.truncated === true) {
      throw new MessageSizeLimit("This message exceeds the viewer’s size limit. Showing the available preview.");
    }
    if (detail.is_hidden || detail.event_key !== event_key || !value || typeof value !== "object"
      || Array.isArray(value) || value.type !== "message" || value.role !== role
      || typeof value.text !== "string" || !value.text.trim()) {
      throw new Error("The full message is unavailable or exceeds the viewer’s size limit.");
    }
    const matches = truncated ? value.text.startsWith(summary.replace(/…$/, "")) : value.text === summary;
    if (!matches || (revision !== null && detail.content_revision !== revision)) {
      throw new Error("This message has changed. Refresh the conversation and try again.");
    }
    return value.text;
  }

  return {
    read() {
      pending ??= load().catch(error => { pending = undefined; throw error; });
      return pending;
    },
  };
}

export type MessageSource = ReturnType<typeof messageSource>;

type ReadingState = { source: MessageSource } & (
  { status: "loading" | "collapsed" } | { status: "expanded"; text: string } | { status: "error"; message: string; retryable: boolean }
);

export function useMessageBody(event: EventSummary, session_key?: string) {
  const { event_key, role, summary, summary_truncated, is_hidden } = event;
  const readable = !is_hidden && event.type === "message" && (role === "user" || role === "assistant");
  const revision = event.content_revision ?? null;
  // A truncated preview alone cannot identify edits beyond the visible prefix.
  const observation = summary_truncated && !revision ? event : null;
  const source = useMemo(() => messageSource(session_key, event_key, role, summary, summary_truncated, revision, readable),
    [session_key, event_key, role, summary, summary_truncated, revision, readable, observation]);
  const [state, setState] = useState<ReadingState | null>(null);
  const current = state?.source === source ? state : null;

  async function expand() {
    if (!summary_truncated) {
      setState({ source, status: "expanded", text: summary });
      return;
    }
    setState({ source, status: "loading" });
    try {
      const text = await source.read();
      setState(previous => previous?.source === source ? { source, status: "expanded", text } : previous);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setState(previous => previous?.source === source
        ? { source, status: "error", message, retryable: !(error instanceof MessageSizeLimit) } : previous);
    }
  }
  return { source, current, expand, collapse: () => setState({ source, status: "collapsed" }) };
}

/** Bound the initial reading height without cutting through a word when possible. */
export function messageExcerpt(text: string, maxChars: number, maxLines: number): string {
  const prefix = text.slice(0, maxChars).split("\n", maxLines + 1);
  if (text.length <= maxChars && prefix.length <= maxLines) return text;
  const clipped = prefix.length > maxLines ? prefix.slice(0, maxLines).join("\n")
    : prefix.join("\n").replace(/\s+\S*$/, match => match.length <= 64 ? "" : match);
  return clipped.trimEnd().replace(/[\uD800-\uDBFF]$/, "") + "…";
}
