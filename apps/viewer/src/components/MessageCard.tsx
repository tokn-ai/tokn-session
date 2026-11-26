import { useEffect, useMemo, useState } from "react";
import { bindMarkdownImages } from "../lib/markdownImages";
import type { EventSummary } from "../lib/types";
import { formatTimestamp, providerLabel } from "../lib/state";
import { useMessageTranslation } from "../lib/useMessageTranslation";
import { MessageImages } from "./MessageImages";
import { MarkdownContent } from "./MarkdownContent";
import { InspectorIcon } from "./Icons";
import { useTranslationEngine, useTranslationStatus } from "./TranslationProvider";
import "./Transcript.css";

interface MessageCardProps {
  event: EventSummary;
  session_key?: string;
  button_id: string;
  is_selected: boolean;
  on_select: (event_key: string) => void;
}

function CopyMessageButton({ content, label }: { content: string; label: string }) {
  const [notice, setNotice] = useState("");
  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(""), 3000);
    return () => window.clearTimeout(timer);
  }, [notice]);

  return <>
    <button aria-label={label} className="message-event__inspect" title={label} type="button"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(content);
          setNotice("Copied");
        } catch {
          setNotice("Copy unavailable");
        }
      }}>
      <svg aria-hidden="true" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6">
        <rect x="8" y="8" width="12" height="13" rx="2" />
        <path d="M15 5V4a1 1 0 0 0-1-1H5a2 2 0 0 0-2 2v10a1 1 0 0 0 1 1h1" />
      </svg>
    </button>
    {notice ? <span className="message-byline__notice" role="status">{notice}</span> : null}
  </>;
}

export function MessageCard({ event, session_key, button_id, is_selected, on_select }: MessageCardProps) {
  const status = useTranslationStatus();
  const engine = useTranslationEngine();
  const translation = useMessageTranslation(event, session_key, engine);
  const role = event.role ?? "unknown";
  const presentation = role === "user" ? "bubble" : role === "assistant" ? "transcript" : "technical";
  const hasImages = !event.is_hidden && !!event.images?.length;
  const showText = event.is_hidden || !!event.summary.trim() || !hasImages;
  const usesMarkdown = !event.is_hidden && (role === "user" || role === "assistant");
  const display_text = translation.content || event.title;
  const inline_images = useMemo(() => session_key && usesMarkdown && !translation.showing_translation
    ? bindMarkdownImages(display_text, event.images ?? []) : new Map(),
  [display_text, event.images, session_key, usesMarkdown, translation.showing_translation]);
  const inline_metadata = new Set(inline_images.values());
  const tray_images = event.images?.filter((image) => !inline_metadata.has(image)) ?? [];
  const imageContent = hasImages && tray_images.length ? session_key
    ? <MessageImages session_key={session_key} event_key={event.event_key} revision={event.content_revision} images={tray_images} />
    : <span className="message-image__status">Image unavailable</span> : null;
  const canTranslate = Boolean(status && session_key && role === "assistant" && !event.is_hidden && event.summary.trim());
  const sender = role === "assistant" ? providerLabel(event.provider) || "Assistant" : role;
  const fullMessage = event.summary_truncated && !event.is_hidden;
  const canCopy = usesMarkdown && !!event.summary.trim() && typeof navigator.clipboard?.writeText === "function";
  const response = role === "assistant" && event.delivery === "final" && event.phase === "finished" && !event.is_hidden
    ? event.response_metadata : undefined;
  const model = response?.model;
  const model_source = model?.origin === "request" ? "Requested model" : "Reported model";
  const fast = response?.fast_mode === "reported" ? "Fast"
    : response?.fast_mode === "requested" ? "Fast requested" : undefined;

  return (
    <article className="message-event transcript-message" aria-label={`${sender} message`}
      data-kind={event.type} data-presentation={presentation} data-role={role} data-selected={is_selected}>
      <div className="message-event__surface">
        <header className="message-byline">
          <span className="message-byline__sender">{sender}</span>
          {event.delivery === "commentary" ? <span className="message-byline__phase">Progress update</span> : null}
          {event.phase && event.phase !== "finished" ? <span className="message-byline__phase">{event.phase}</span> : null}
          {event.timestamp ? <time title={formatTimestamp(event.timestamp)}>{formatTimestamp(event.timestamp, "time")}</time> : null}
          <span className="message-byline__actions">
            {canCopy ? <CopyMessageButton content={display_text} label={fullMessage && !translation.translated
              ? "Copy message preview" : translation.showing_translation ? "Copy translation" : "Copy message"} /> : null}
            <button aria-label={`Inspect ${role} message`} className="message-event__inspect" id={button_id}
              onClick={() => on_select(event.event_key)} title={fullMessage ? "View full message" : "Inspect message"} type="button">
              {fullMessage ? "View full message" : <InspectorIcon width={15} height={15} />}
            </button>
          </span>
        </header>
        {event.type === "question_reply" && !event.is_hidden ? <span className="message-event__answer-heading">Recorded answers</span> : null}
        {role === "user" ? imageContent : null}
        {showText ? usesMarkdown ? (
          <div aria-busy={translation.loading} lang={translation.showing_translation ? "zh-Hans" : undefined}>
            <MarkdownContent class_name="message-event__text" content={display_text} image_context={session_key && inline_images.size ? {
              session_key, event_key: event.event_key, revision: event.content_revision ?? null, bindings: inline_images,
            } : undefined} />
          </div>
        ) : (
          <div className="message-event__text message-event__text--plain">
            {event.is_hidden ? "Hidden extension message" : event.summary || event.title}
          </div>
        ) : null}
        {role !== "user" ? imageContent : null}
        {translation.error ? (
          <div className="message-translation__error" role="alert">
            <span>{translation.error}</span>
            {translation.can_open_settings ? (
              <>
                {" "}
                <button
                  className="message-translation__settings"
                  disabled={translation.settings_loading}
                  onClick={() => { void translation.openSettings(); }}
                  type="button"
                >
                  {translation.settings_loading ? "Opening Translation Settings…" : "Open Translation Settings"}
                </button>
              </>
            ) : null}
            {translation.settings_error ? (
              <p className="message-translation__settings-error">
                {translation.settings_error} Open System Settings → General → Language &amp; Region → Translation Languages.
              </p>
            ) : null}
          </div>
        ) : null}
        {model || fast || canTranslate ? (
          <div className="message-event__actions">
            {model || fast ? (
              <span className="message-event__response" role="group" aria-label="Response details">
                {model ? (
                  <span title={[model_source, model.model.model_provider].filter(Boolean).join(" · ")}>
                    {model.model.model_id}
                  </span>
                ) : null}
                {fast ? <span>{fast}</span> : null}
              </span>
            ) : null}
            {canTranslate ? (
              <>
                {translation.loading ? (
                  <>
                    <span className="message-translation__status" role="status">
                      {!translation.progress?.resume ? <span className="inline-spinner" aria-hidden="true" /> : null}
                      {translation.progress?.message ?? "Translating…"}
                    </span>
                    {translation.progress?.resume ? (
                      <button className="message-event__inspect" onClick={translation.progress.resume} type="button">Continue translation</button>
                    ) : null}
                    <button className="message-event__inspect" onClick={translation.cancel} type="button">Cancel</button>
                  </>
                ) : translation.translated ? (
                  <>
                    <span className="message-translation__status">{translation.showing_translation ? `简体中文 · ${engine?.label}` : "Original"}</span>
                    <button className="message-event__inspect" onClick={translation.toggle} type="button">
                      {translation.showing_translation ? "Show original" : "Show translation"}
                    </button>
                  </>
                ) : (
                  <button
                    className="message-event__inspect"
                    disabled={!status?.available}
                    onClick={() => { void translation.translate(); }}
                    title={status?.available ? engine?.description : status?.reason ?? undefined}
                    type="button"
                  >
                    {translation.error ? "Retry translation" : "Translate → 简体中文"}
                  </button>
                )}
              </>
            ) : null}
          </div>
        ) : null}
      </div>
    </article>
  );
}
