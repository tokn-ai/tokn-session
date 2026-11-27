import { useId, useMemo, useRef } from "react";
import { bindMarkdownImages } from "../lib/markdownImages";
import type { EventSummary } from "../lib/types";
import { formatRecordedDuration, formatTimestamp, providerLabel } from "../lib/state";
import { useMessageTranslation } from "../lib/useMessageTranslation";
import { MessageImages } from "./MessageImages";
import { MarkdownContent } from "./MarkdownContent";
import { CopyTextButton } from "./CopyTextButton";
import { messageExcerpt, useMessageBody } from "../lib/useMessageBody";
import { ChevronIcon, CompletedGoalIcon, GoalIcon, InspectorIcon, TranslateIcon } from "./Icons";
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
  const translationButton = useRef<HTMLButtonElement>(null);
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
  const canTranslate = Boolean(status?.available && session_key && role === "assistant" && !event.is_hidden && event.summary.trim());
  const translationAction = translation.loading ? "Translating…"
    : translation.translated ? translation.showing_translation ? "Show original" : "Show translation"
    : translation.error ? "Retry translation" : "Translate → 简体中文";
  const sender = role === "assistant" ? providerLabel(event.provider) || "Assistant" : role;
  const canCopy = usesMarkdown && !!event.summary.trim();
  const copyText = preview ? display_text : translatedText ?? expandedText ?? event.summary;
  const response = role === "assistant" && event.delivery === "final" && event.phase === "finished" && !event.is_hidden
    ? event.response_metadata : undefined;
  const model = response?.model;
  const model_source = model?.origin === "request" ? "Requested model" : "Reported model";
  const fast = response?.fast_mode === "reported" ? "Fast"
    : response?.fast_mode === "requested" ? "Fast requested" : undefined;
  const phase = event.phase && event.phase !== "finished" ? event.phase : null;
  const progress = event.delivery === "commentary";
  const goalPrompt = !event.is_hidden && role === "user" && event.goal?.kind === "prompt";
  const completion = !event.is_hidden && role === "assistant" && event.goal?.kind === "completed" ? event.goal : null;
  const goalDuration = completion && formatRecordedDuration(completion.duration_ms);
  const timestamp = event.timestamp && <time dateTime={event.timestamp} title={formatTimestamp(event.timestamp)}>{formatTimestamp(event.timestamp, "time")}</time>;

  return (
    <article className="message-event transcript-message" aria-label={`${sender} message`}
      data-kind={event.type} data-presentation={presentation} data-role={role} data-selected={is_selected} data-goal-prompt={goalPrompt || undefined}>
      <div className="message-event__surface">
        {(presentation === "technical" || progress || phase) && <header className="message-phase">
          {presentation === "technical" && <span>{sender}</span>}
          {progress && <span>Progress update</span>}
          {phase && <span>{phase}</span>}
        </header>}
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
      </div>
      <footer className="message-utilities" data-read-action={inspectFullMessage || undefined}>
        {goalPrompt && <span className="message-goal-label"><GoalIcon />Sent as goal</span>}
        <div className="message-utilities__buttons">
          {canCopy && <CopyTextButton class_name="message-event__inspect" text={copyText} label={preview
            ? "Copy message preview" : translation.showing_translation ? "Copy translation" : "Copy message"} />}
          <button aria-label={`Inspect ${role} message`} className="message-event__inspect" id={button_id}
            onClick={() => on_select(event.event_key)} title={inspectFullMessage ? "View full message" : "Inspect message"} type="button">
            {inspectFullMessage ? "View full message" : <InspectorIcon width={15} height={15} />}
          </button>
          {canTranslate && <button ref={translationButton} type="button"
            className="message-event__inspect message-translation__action"
            data-showing-translation={translation.showing_translation || undefined}
            aria-label={translationAction} aria-controls={contentId} aria-disabled={translation.loading || undefined}
            title={translation.translated && engine ? `${translationAction} · ${engine.label}`
              : translation.loading || translation.error ? translationAction : engine?.description}
            onClick={() => {
              if (translation.loading) return;
              if (translation.translated) translation.toggle();
              else void translation.translate();
            }}>
            <TranslateIcon width={15} height={15} />
          </button>}
        </div>
        {completion && <span className="message-goal-completion">
          <span className="message-goal-label"><CompletedGoalIcon />{goalDuration ? `Goal achieved in ${goalDuration}` : "Goal achieved"}</span>
          {timestamp}
        </span>}
        {model || fast ? <span className="message-event__response" role="group" aria-label="Response details">
          {model && <span title={[model_source, model.model.model_provider].filter(Boolean).join(" · ")}>{model.model.model_id}</span>}
          {fast && <span>{fast}</span>}
        </span> : null}
        {(role === "assistant" || event.timestamp) && <span className="message-byline">
          {role === "assistant" && <span className="message-byline__sender" data-provider={event.provider}>{sender}</span>}
          {!completion && timestamp}
        </span>}
        {canTranslate && translation.loading && (
          <div className="message-utilities__translation">
            <span className="message-translation__status" role="status">
              {!translation.progress?.resume && <span className="inline-spinner" aria-hidden="true" />}
              {translation.progress?.message ?? translationAction}
            </span>
            {translation.progress?.resume && <button className="message-event__inspect" type="button" onClick={() => {
              translation.progress?.resume?.();
              translationButton.current?.focus();
            }}>Continue translation</button>}
            <button className="message-event__inspect" type="button" onClick={() => {
              translation.cancel();
              translationButton.current?.focus();
            }}>Cancel</button>
          </div>
        )}
      </footer>
    </article>
  );
}
