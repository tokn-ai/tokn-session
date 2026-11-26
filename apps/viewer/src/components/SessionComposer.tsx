import { useId, useLayoutEffect, useRef, useState } from "react";
import { ImageAttachmentPreviews, ImageAttachmentPicker, imagePaste, imageDrop } from "./ImageAttachments";
import { RefreshIcon, SendIcon } from "./Icons";
import { ComposerToolbar } from "./ComposerToolbar";
import { useSessionInput } from "../lib/useSessionInput";
import type { SessionSummary } from "../lib/types";
import { providerLabel } from "../lib/state";
import type { SessionModelController } from "../lib/useSessionModel";
import { SessionModelPicker } from "./SessionModelPicker";
import { SessionControlSelector } from "./SessionControlSelector";
import type { SessionControlController } from "../lib/useSessionControl";

export function SessionComposer({ session, on_accepted, input_refresh_key, model, control }: {
  session: SessionSummary | null;
  on_accepted?: (session_key: string) => void;
  input_refresh_key?: number;
  model?: SessionModelController;
  control?: SessionControlController;
}) {
  const input = useSessionInput(session?.session_key ?? null, (session_key) => {
    if (session?.session_key === session_key && control) void control.refresh();
    return on_accepted?.(session_key);
  }, input_refresh_key, model, control);
  const inputId = useId();
  const textarea = useRef<HTMLTextAreaElement>(null);
  const retry_focus = useRef<string | null>(null);
  const [focused_session, setFocusedSession] = useState<string | null>(null);
  const expanded = !!input.draft || !!input.images.length || (!!session && focused_session === session.session_key);
  const read_only = input.availability?.available === false;
  const show_editor = !read_only || !!input.draft || !!input.images.length || input.delivery_uncertain;

  useLayoutEffect(() => {
    if (!session || retry_focus.current !== session.session_key) {
      retry_focus.current = null;
      return;
    }
    if (input.checking) return;
    retry_focus.current = null;
    if (input.availability?.available && document.activeElement === document.body) {
      textarea.current?.focus({ preventScroll: true });
    }
  }, [input.checking, input.availability?.available, session?.session_key]);

  useLayoutEffect(() => {
    const field = textarea.current;
    if (!field) return;
    function resize() {
      if (!field) return;
      field.style.height = "auto";
      field.style.height = `${field.scrollHeight}px`;
    }
    resize();
    window.addEventListener("resize", resize);
    return () => window.removeEventListener("resize", resize);
  }, [input.draft, expanded, session?.session_key, show_editor]);
  if (!session) return null;

  const disabled = input.checking || !input.availability?.available
    || input.sending || input.delivery_uncertain;
  const overLimit = input.length > input.max_length;
  const guide_available = input.availability?.delivery_modes?.includes("guide") === true;
  const has_start = input.availability?.delivery_modes?.includes("start") === true;
  const advisory_send = guide_available && !has_start;
  const provider = providerLabel(session.provider);
  const picker = model && <SessionModelPicker key={session.session_key} model={model} provider={provider}
    input_busy={input.sending || input.delivery_uncertain} selected_model={input.model_draft}
    on_select={input.setModel} on_clear={input.clearModel} />;
  const status_text = input.sending ? input.resuming ? `Resuming ${provider} and sending your message…` : "Sending your message…"
    : input.notice ?? input.validation_notice ?? (input.checking ? "Checking message availability…" : input.availability?.message);
  const connection = control && <SessionControlSelector control={control} input_busy={input.sending || input.delivery_uncertain}
    running={session.is_running} />;
  const foldConnection = !show_editor && control?.status?.can_select && !(control.status.is_running ?? session.is_running)
    && !control.changing && !control.stopping;

  return (
    <form
      aria-label="Send a message to this session"
      className="session-composer"
      data-expanded={expanded}
      onPaste={(event) => { if (!disabled && input.image_limits) imagePaste(event, input.addImages); }}
      onDragOver={(event) => { if (!disabled && input.image_limits && event.dataTransfer.types.includes("Files")) event.preventDefault(); }}
      onDrop={(event) => { if (!disabled && input.image_limits) imageDrop(event, input.addImages); }}
      onBlur={(event) => {
        const in_model_panel = event.relatedTarget instanceof Element && event.relatedTarget.closest(".session-model__panel");
        if (!event.currentTarget.contains(event.relatedTarget) && !in_model_panel) setFocusedSession(null);
      }}
      onSubmit={(event) => {
        event.preventDefault();
        void input.send();
      }}
    >
      <div className="session-composer__content">
        {!foldConnection && connection}
        {show_editor && <>
          <label className="sr-only" htmlFor={inputId}>Message this session</label>
          <div className="session-composer__box">
            <ImageAttachmentPreviews images={input.images} disabled={disabled} on_remove={input.removeImage} />
            <textarea
              id={inputId}
              ref={textarea}
              aria-describedby={`${inputId}-status ${inputId}-hint`}
              aria-invalid={overLimit || undefined}
              disabled={disabled}
              onFocus={() => setFocusedSession(session.session_key)}
              onChange={(event) => input.setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && (event.metaKey || event.ctrlKey)
                  && !event.nativeEvent.isComposing && event.keyCode !== 229) {
                  event.preventDefault();
                  void input.send();
                }
              }}
              placeholder={`Message ${provider}…`}
              rows={expanded ? 3 : 1}
              value={input.draft}
            />
            <ComposerToolbar options={<>
              <ImageAttachmentPicker image_count={input.images.length} limits={input.image_limits} disabled={disabled} on_add={input.addImages} />
              {picker ?? <span className="session-composer__provider">{provider}</span>}
            </>}>
              {guide_available && has_start ? <button type="button" className="text-button"
                disabled={!input.can_guide} title="Start if idle, guide active work when possible, or queue a follow-up."
                onClick={() => { void input.send("guide"); }}>Guide</button> : null}
              {input.availability?.delivery_modes?.includes("queue") ? <button type="button" className="text-button"
                disabled={!input.can_queue} title="Add a follow-up, or start immediately when idle."
                onClick={() => { void input.send("queue"); }}>Queue</button> : null}
              <button
                aria-label={input.sending ? input.resuming ? `Resuming ${provider}…` : "Sending…" : "Send"}
                className="session-composer__send"
                disabled={!input.can_send}
                title={advisory_send ? "Start if idle, guide active work when possible, or queue a follow-up." : "Send message (⌘ / Ctrl + Enter)"}
                type="submit"
              >
                {input.sending ? <span aria-hidden="true" className="inline-spinner" /> : <SendIcon />}
              </button>
            </ComposerToolbar>
          </div>
          <div className={expanded || overLimit ? "session-composer__actions" : "sr-only"}>
            <span id={`${inputId}-hint`} className={overLimit ? "session-composer__limit" : undefined}>
              {overLimit
                ? `${input.length.toLocaleString()} / ${input.max_length.toLocaleString()} characters`
                : `⌘ / Ctrl + Enter to send · Enter for a new line${advisory_send
                  ? " · Send may guide active work or become a follow-up"
                  : guide_available ? " · Guide may become a follow-up" : ""}`}
            </span>
          </div>
        </>}
        <div className={read_only ? "composer-read-only" : "session-composer__status"} id={`${inputId}-status`} role="status">
          {read_only && <strong>Read-only session</strong>}
          {status_text && <span>{status_text}</span>}
          {read_only && !input.checking && input.availability?.message && input.availability.message !== status_text
            ? <span>{input.availability.message}</span> : null}
          {input.delivery_uncertain ? (
            <button className="text-button" onClick={() => {
              input.editMessage();
              requestAnimationFrame(() => textarea.current?.focus());
            }} type="button">Edit message</button>
          ) : null}
          {read_only ? (
            <button className="text-button composer-read-only__retry" title="Retry availability" disabled={input.checking} onClick={(event) => {
              if (document.activeElement === event.currentTarget) retry_focus.current = session.session_key;
              input.retryAvailability();
              if (model) void model.refresh();
            }} type="button">
              <RefreshIcon /><span>Retry availability</span>
            </button>
          ) : null}
        </div>
        {!show_editor && (picker || foldConnection) ? <details className="composer-read-only__model" key={session.session_key}>
          <summary>{foldConnection ? (picker ? "Connection and model" : "Connection details") : "Model details"}</summary>
          {foldConnection && connection}
          {picker}
        </details> : null}
      </div>
    </form>
  );
}
