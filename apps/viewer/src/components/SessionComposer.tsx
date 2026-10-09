import { useId, useLayoutEffect, useRef, useState } from "react";
import { SendIcon } from "./Icons";
import { useSessionInput } from "../lib/useSessionInput";
import type { SessionSummary } from "../lib/types";
import { providerLabel } from "../lib/state";

export function SessionComposer({ session, on_accepted }: {
  session: SessionSummary | null;
  on_accepted?: (session_key: string) => void;
}) {
  const input = useSessionInput(session?.session_key ?? null, on_accepted);
  const inputId = useId();
  const textarea = useRef<HTMLTextAreaElement>(null);
  const [focused_session, setFocusedSession] = useState<string | null>(null);
  const expanded = !!input.draft || (!!session && focused_session === session.session_key);

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
  }, [input.draft, expanded, session?.session_key]);
  if (!session) return null;

  const disabled = input.checking || !input.availability?.available
    || input.sending || input.delivery_uncertain;
  const overLimit = input.length > input.max_length;

  return (
    <form
      aria-label="Send a message to this session"
      className="session-composer"
      data-expanded={expanded}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setFocusedSession(null);
      }}
      onSubmit={(event) => {
        event.preventDefault();
        void input.send();
      }}
    >
      <div className="session-composer__content">
        <label className="sr-only" htmlFor={inputId}>Message this session</label>
        <div className="session-composer__box">
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
            placeholder={!input.checking && !input.availability?.available ? "Read-only session" : "Message this session…"}
            rows={expanded ? 3 : 1}
            value={input.draft}
          />
          <div className="session-composer__footer">
            <span className="session-composer__provider">{providerLabel(session.provider)}</span>
            <button
              aria-label={input.sending ? "Sending…" : "Send"}
              className="session-composer__send"
              disabled={!input.can_send}
              title="Send message (⌘ / Ctrl + Enter)"
              type="submit"
            >
              {input.sending ? <span aria-hidden="true" className="inline-spinner" /> : <SendIcon />}
            </button>
          </div>
        </div>
        <div className={expanded || overLimit ? "session-composer__actions" : "sr-only"}>
          <span id={`${inputId}-hint`} className={overLimit ? "session-composer__limit" : undefined}>
            {overLimit
              ? `${input.length.toLocaleString()} / ${input.max_length.toLocaleString()} characters`
              : "⌘ / Ctrl + Enter to send · Enter for a new line"}
          </span>
        </div>
        <div className="session-composer__status" id={`${inputId}-status`} role="status">
          {input.notice ? <span>{input.notice}</span> : input.checking
            ? <span>Checking message availability…</span>
            : input.availability?.message ? <span>{input.availability.message}</span> : null}
          {input.delivery_uncertain ? (
            <button className="text-button" onClick={() => {
              input.editMessage();
              requestAnimationFrame(() => textarea.current?.focus());
            }} type="button">Edit message</button>
          ) : null}
          {!input.checking && !input.availability?.available ? (
            <button className="text-button" onClick={input.retryAvailability} type="button">
              Retry availability
            </button>
          ) : null}
        </div>
      </div>
    </form>
  );
}
