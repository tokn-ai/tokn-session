import { useEffect, useMemo, useRef, useState } from "react";
import { loadEventDetail } from "./tauri";
import { translateMarkdown } from "./markdownTranslation";
import type { TranslationEngine, TranslationJob, TranslationProgress } from "./translationEngine";
import type { EventSummary } from "./types";

interface TranslationSource {
  session_key: string | undefined;
  event_key: string;
  summary: string;
  summary_truncated: boolean;
  content_revision: string | null;
}

interface TranslationState {
  source: TranslationSource;
  original: string | null;
  translated: string | null;
  show_translation: boolean;
  loading: boolean;
  error: string | null;
  progress: TranslationProgress | null;
}

/** Results belong to the source text, never just its reusable event index. */
export function useMessageTranslation(event: EventSummary, sessionKey?: string, engine?: TranslationEngine | null) {
  // Older servers cannot identify changes beyond a truncated preview, so
  // retain conservative invalidation until they provide a full-text revision.
  const contentRevision = event.summary_truncated ? event.content_revision ?? null : null;
  const truncatedObservation = event.summary_truncated && contentRevision === null ? event : null;
  const source = useMemo<TranslationSource>(() => ({
    session_key: sessionKey,
    event_key: event.event_key,
    summary: event.summary,
    summary_truncated: event.summary_truncated,
    content_revision: contentRevision,
  }), [sessionKey, event.event_key, event.summary, event.summary_truncated,
    event.is_hidden, event.role, contentRevision, truncatedObservation]);
  const currentSource = useRef(source);
  currentSource.current = source;
  const active = useRef<{ source: TranslationSource; job: TranslationJob | null } | null>(null);
  const [state, setState] = useState<TranslationState | null>(null);
  const visible = state?.source === source ? state : null;

  function abortActive() {
    const request = active.current;
    active.current = null;
    request?.job?.dispose();
  }

  useEffect(() => {
    return () => { abortActive(); };
  }, [source, engine]);

  function cancel() {
    abortActive();
    setState((previous) => previous?.source === source ? { ...previous, loading: false, progress: null } : previous);
  }

  async function translate() {
    if (!engine || !sessionKey || active.current || event.is_hidden || event.role !== "assistant" || !event.summary.trim()) return;
    const request = { source, job: null as TranslationJob | null };
    active.current = request;
    const isCurrent = () => active.current === request && currentSource.current === source;
    setState({ source, original: null, translated: null, show_translation: false, loading: true, error: null, progress: null });
    try {
      // Model preparation starts in the click handler while user activation is
      // still available, before the potentially slow history request.
      const job = engine.start((progress) => {
        if (isCurrent()) setState((previous) => previous?.source === source ? { ...previous, progress } : previous);
      });
      request.job = job;
      const detail = await loadEventDetail({ session_key: sessionKey, event_key: event.event_key });
      if (!isCurrent()) return;
      const value = detail.event;
      if (detail.is_hidden || detail.event_key !== event.event_key
        || !value || typeof value !== "object" || Array.isArray(value)
        || value.type !== "message" || value.role !== "assistant"
        || typeof value.text !== "string" || !value.text.trim()) {
        throw new Error("The full response is unavailable or too large to translate.");
      }
      const original = value.text;
      const expected = source.summary_truncated ? source.summary.replace(/…$/, "") : source.summary;
      const revisionChanged = source.content_revision !== null && detail.content_revision != null
        && detail.content_revision !== source.content_revision;
      if (revisionChanged || (source.summary_truncated ? !original.startsWith(expected) : original !== expected)) {
        throw new Error("This response has changed. Refresh the conversation and try again.");
      }
      setState((previous) => previous?.source === source ? { ...previous, original } : previous);
      const translated = await translateMarkdown(original, async (texts) => {
        if (!isCurrent()) throw new Error("Translation cancelled.");
        const response = await job.translate(texts);
        if (!isCurrent()) throw new Error("Translation cancelled.");
        return response;
      });
      if (isCurrent()) setState({ source, original, translated, show_translation: true, loading: false, error: null, progress: null });
    } catch (error) {
      if (isCurrent()) setState((previous) => ({
        source,
        original: previous?.source === source ? previous.original : null,
        translated: null,
        show_translation: false,
        loading: false,
        error: error instanceof Error ? error.message : String(error),
        progress: null,
      }));
    } finally {
      if (active.current === request) active.current = null;
      request.job?.dispose();
    }
  }

  return {
    content: visible?.show_translation ? visible.translated! : visible?.original ?? event.summary,
    loading: visible?.loading ?? false,
    translated: visible?.translated !== null && visible?.translated !== undefined,
    showing_translation: visible?.show_translation ?? false,
    error: visible?.error ?? null,
    progress: visible?.progress ?? null,
    translate,
    cancel,
    toggle: () => setState((previous) => previous?.source === source && previous.translated !== null
      ? { ...previous, show_translation: !previous.show_translation }
      : previous),
  };
}
