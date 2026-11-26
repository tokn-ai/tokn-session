import { useId, useMemo } from "react";
import { bindMarkdownImages } from "../lib/markdownImages";
import type { EventSummary } from "../lib/types";
import { formatTimestamp, providerLabel } from "../lib/state";
import { useMessageTranslation } from "../lib/useMessageTranslation";
import { MessageImages } from "./MessageImages";
import { MarkdownContent } from "./MarkdownContent";
import { CopyTextButton } from "./CopyTextButton";
import { messageExcerpt, useMessageBody } from "../lib/useMessageBody";
import { ChevronIcon, InspectorIcon } from "./Icons";
import { useTranslationEngine, useTranslationStatus } from "./TranslationProvider";
import "./Transcript.css";

interface MessageCardProps {
  event: EventSummary;
  session_key?: string;
  button_id: string;
  is_selected: boolean;
  on_select: (event_key: string) => void;
}

export function MessageCard({ event, session_key, button_id, is_selected, on_select }: MessageCardProps) {
  const status = useTranslationStatus();
  const engine = useTranslationEngine();
  const contentId = useId();
  const body = useMessageBody(event, session_key);
  const translation = useMessageTranslation(event, body.source, session_key, engine);
  const role = event.role ?? "unknown";
  const presentation = role === "user" ? "bubble" : role === "assistant" ? "transcript" : "technical";
  const hasImages = !event.is_hidden && !!event.images?.length;
  const showText = event.is_hidden || !!event.summary.trim() || !hasImages;
  const usesMarkdown = !event.is_hidden && (role === "user" || role === "assistant");
  let excerpt = event.summary;
  if (event.type === "message" && usesMarkdown && (session_key || !event.summary_truncated)) {
    if (role === "user") excerpt = messageExcerpt(event.summary, 800, 8);
    else if (event.summary_truncated) excerpt = messageExcerpt(event.summary, 1600, 16);
  }
  const translatedText = translation.showing_translation ? translation.content : null;
  const expandedText = body.current?.status === "expanded" ? body.current.text
    : body.current?.status !== "collapsed" && !translatedText ? translation.content : null;
  const expanded = expandedText !== null;
  const limited = body.current?.status === "limited" ? body.current : null;
  const readError = body.current?.status === "error" ? body.current.message : limited?.message;
  const availableText = limited?.expanded ? event.summary : excerpt;
  const previewExpanded = !!limited?.expanded && excerpt !== event.summary;
  const loading = body.current?.status === "loading";
  const previewUnavailable = limited !== null && excerpt === event.summary;
  const disclosureDisabled = loading || previewUnavailable;
  const toggleLabel = loading ? "Loading full message…" : expanded || previewExpanded ? "Show less"
    : previewUnavailable ? "Full message unavailable" : limited ? "Show available preview"
    : readError ? "Retry full message" : role === "user" ? "Read full prompt" : "Read full reply";
  const display_text = translatedText ?? expandedText ?? (availableText || event.title);
  const inlineDisclosure = usesMarkdown && event.type === "message" && !translatedText
    && (event.summary_truncated ? !!session_key : excerpt !== event.summary);
  const preview = !expanded && !translatedText && event.summary_truncated;
  const partialMarkdown = preview || (!expanded && !translatedText && availableText !== event.summary);
  const inspectFullMessage = preview && !event.is_hidden && !inlineDisclosure;
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
  const canCopy = usesMarkdown && !!event.summary.trim();
  const copyText = preview ? display_text : translatedText ?? expandedText ?? event.summary;
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
          <span className="message-byline__sender" data-provider={role === "assistant" ? event.provider : undefined}>{sender}</span>
          {event.delivery === "commentary" ? <span className="message-byline__phase">Progress update</span> : null}
          {event.phase && event.phase !== "finished" ? <span className="message-byline__phase">{event.phase}</span> : null}
          {event.timestamp ? <time title={formatTimestamp(event.timestamp)}>{formatTimestamp(event.timestamp, "time")}</time> : null}
          <span className="message-byline__actions">
            {canCopy ? <CopyTextButton class_name="message-event__inspect" text={copyText} label={preview
              ? "Copy message preview" : translation.showing_translation ? "Copy translation" : "Copy message"} /> : null}
            <button aria-label={`Inspect ${role} message`} className="message-event__inspect" id={button_id}
              onClick={() => on_select(event.event_key)} title={inspectFullMessage ? "View full message" : "Inspect message"} type="button">
              {inspectFullMessage ? "View full message" : <InspectorIcon width={15} height={15} />}
            </button>
          </span>
        </header>
        {event.type === "question_reply" && !event.is_hidden ? <span className="message-event__answer-heading">Recorded answers</span> : null}
        {role === "user" ? imageContent : null}
        {showText ? usesMarkdown ? (
          <div id={contentId} aria-busy={translation.loading || loading} lang={translation.showing_translation ? "zh-Hans" : undefined}>
            <MarkdownContent class_name="message-event__text" content={display_text} partial={partialMarkdown} image_context={session_key && inline_images.size ? {
              session_key, event_key: event.event_key, revision: event.content_revision ?? null, bindings: inline_images,
            } : undefined} />
          </div>
        ) : (
          <div className="message-event__text message-event__text--plain">
            {event.is_hidden ? "Hidden extension message" : event.summary || event.title}
          </div>
        ) : null}
        {inlineDisclosure && <button className="message-disclosure" aria-controls={contentId} aria-expanded={expanded || previewExpanded}
          aria-disabled={disclosureDisabled || undefined} aria-describedby={readError ? `${contentId}-notice` : undefined}
          type="button" onClick={() => {
            if (disclosureDisabled) return;
            if (expanded || previewExpanded) body.collapse();
            else void body.expand();
          }}>
          {toggleLabel}
          <ChevronIcon width={14} height={14} />
        </button>}
        {readError && <p className="message-body-error" id={`${contentId}-notice`} role="alert">{readError}</p>}
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
