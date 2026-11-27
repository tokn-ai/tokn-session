import { useEffect, useRef, useState } from "react";
import type { MessageSource } from "./useMessageBody";
import { translateMarkdown } from "./markdownTranslation";
import { NativeTranslationError, normalizeTranslationFailure, type TranslationEngine, type TranslationJob, type TranslationProgress } from "./translationEngine";
import type { EventSummary, TranslationRecovery } from "./types";

interface TranslationState {
  source: MessageSource;
  original: string | null;
  translated: string | null;
  show_translation: boolean;
  loading: boolean;
  error: string | null;
  recovery: TranslationRecovery | null;
  settings_loading: boolean;
  settings_error: string | null;
  progress: TranslationProgress | null;
}

/** Results belong to the source text, never just its reusable event index. */
export function useMessageTranslation(event: EventSummary, source: MessageSource, sessionKey?: string, engine?: TranslationEngine | null) {
  const currentSource = useRef(source);
  currentSource.current = source;
  const currentEngine = useRef(engine);
  currentEngine.current = engine;
  const active = useRef<{ source: MessageSource; job: TranslationJob | null } | null>(null);
  const settingsRequest = useRef<object | null>(null);
  const [state, setState] = useState<TranslationState | null>(null);
  const visible = state?.source === source ? state : null;

  function abortActive() {
    const request = active.current;
    active.current = null;
    request?.job?.dispose();
  }

  useEffect(() => {
    return () => {
      abortActive();
      settingsRequest.current = null;
    };
  }, [source, engine]);

  function cancel() {
    abortActive();
    setState((previous) => previous?.source === source ? { ...previous, loading: false, progress: null } : previous);
  }

  async function translate() {
    if (!engine || !sessionKey || active.current || event.is_hidden || event.role !== "assistant" || !event.summary.trim()) return;
    const request = { source, job: null as TranslationJob | null };
    active.current = request;
    settingsRequest.current = null;
    const isCurrent = () => active.current === request && currentSource.current === source;
    setState({ source, original: null, translated: null, show_translation: false, loading: true, error: null,
      recovery: null, settings_loading: false, settings_error: null, progress: null });
    try {
      // Model preparation starts in the click handler while user activation is
      // still available, before the potentially slow history request.
      const job = engine.start((progress) => {
        if (isCurrent()) setState((previous) => previous?.source === source ? { ...previous, progress } : previous);
      });
      request.job = job;
      const original = await source.read();
      if (!isCurrent()) return;
      setState((previous) => previous?.source === source ? { ...previous, original } : previous);
      const translated = await translateMarkdown(original, async (texts) => {
        if (!isCurrent()) throw new Error("Translation cancelled.");
        const response = await job.translate(texts);
        if (!isCurrent()) throw new Error("Translation cancelled.");
        return response;
      });
      if (isCurrent()) setState({ source, original, translated, show_translation: true, loading: false, error: null,
        recovery: null, settings_loading: false, settings_error: null, progress: null });
    } catch (error) {
      if (isCurrent()) setState((previous) => ({
        source,
        original: previous?.source === source ? previous.original : null,
        translated: null,
        show_translation: false,
        loading: false,
        error: normalizeTranslationFailure(error).message,
        recovery: error instanceof NativeTranslationError ? error.recovery : null,
        settings_loading: false,
        settings_error: null,
        progress: null,
      }));
    } finally {
      if (active.current === request) active.current = null;
      request.job?.dispose();
    }
  }

  async function openSettings() {
    if (visible?.recovery !== "translation_settings" || !engine?.openSettings || settingsRequest.current) return;
    const request = {};
    settingsRequest.current = request;
    const isCurrent = () => settingsRequest.current === request
      && currentSource.current === source && currentEngine.current === engine;
    setState((previous) => previous?.source === source ? { ...previous, settings_loading: true, settings_error: null } : previous);
    try {
      await engine.openSettings();
    } catch (error) {
      if (isCurrent()) setState((previous) => previous?.source === source
        ? { ...previous, settings_error: normalizeTranslationFailure(error).message } : previous);
    } finally {
      if (isCurrent()) {
        settingsRequest.current = null;
        setState((previous) => previous?.source === source ? { ...previous, settings_loading: false } : previous);
      }
    }
  }

  return {
    content: visible?.show_translation ? visible.translated : visible?.original ?? null,
    loading: visible?.loading ?? false,
    translated: visible?.translated !== null && visible?.translated !== undefined,
    showing_translation: visible?.show_translation ?? false,
    error: visible?.error ?? null,
    can_open_settings: visible?.recovery === "translation_settings" && Boolean(engine?.openSettings),
    settings_loading: visible?.settings_loading ?? false,
    settings_error: visible?.settings_error ?? null,
    progress: visible?.progress ?? null,
    translate,
    cancel,
    openSettings,
    toggle: () => setState((previous) => previous?.source === source && previous.translated !== null
      ? { ...previous, show_translation: !previous.show_translation }
      : previous),
  };
}
