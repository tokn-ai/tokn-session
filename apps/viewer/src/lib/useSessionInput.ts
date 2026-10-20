import { createUuid } from "./id";
import { useCallback, useEffect, useRef, useState } from "react";
import { getSessionInputStatus, submitSessionInput } from "./tauri";
import { errorMessage } from "./state";
import type { SessionInputStatus } from "./types";

const DEFAULT_MAX_LENGTH = 16_384;
const UNCONFIRMED_MESSAGE = "Delivery not confirmed. Check the conversation before sending again.";

interface InputState {
  draft: string;
  availability: SessionInputStatus | null;
  checking: boolean;
  check_id: number;
  sending: boolean;
  resuming: boolean;
  delivery_uncertain: boolean;
  notice: string | null;
}

function emptyInput(): InputState {
  return {
    draft: "", availability: null, checking: true, check_id: 0,
    sending: false, resuming: false, delivery_uncertain: false, notice: null,
  };
}

/** Keep drafts and in-flight results attached to their original session. */
export function useSessionInput(session_key: string | null, on_accepted?: (session_key: string) => void, refresh_key = 0) {
  const states = useRef(new Map<string, InputState>());
  const mounted = useRef(false);
  const acceptedHandler = useRef(on_accepted);
  acceptedHandler.current = on_accepted;
  const [, render] = useState(0);

  const update = useCallback((key: string, apply: (state: InputState) => InputState) => {
    states.current.set(key, apply(states.current.get(key) ?? emptyInput()));
    if (mounted.current) render((revision) => revision + 1);
  }, []);

  const checkAvailability = useCallback(async (key: string) => {
    const checkId = (states.current.get(key)?.check_id ?? 0) + 1;
    update(key, (state) => ({ ...state, checking: true, check_id: checkId }));
    let availability: SessionInputStatus;
    try {
      availability = await getSessionInputStatus({ session_key: key });
    } catch (error) {
      availability = {
        available: false,
        message: withDetail(
          "Message input is unavailable on this connection. Retry to check availability.",
          errorMessage(error),
        ),
        max_length: DEFAULT_MAX_LENGTH,
      };
    }
    update(key, (state) => state.check_id === checkId
      ? { ...state, checking: false, availability }
      : state);
  }, [update]);

  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  useEffect(() => {
    if (session_key) void checkAvailability(session_key);
  }, [session_key, checkAvailability, refresh_key]);

  const state = (session_key && states.current.get(session_key)) || emptyInput();
  const maxLength = state.availability?.max_length ?? DEFAULT_MAX_LENGTH;
  const length = Array.from(state.draft).length;

  async function send() {
    if (!session_key) return;
    const key = session_key;
    // Read the ref, not the render snapshot: two clicks in one frame must send once.
    const current = states.current.get(key);
    if (!current || current.sending || current.delivery_uncertain || current.checking
      || !current.availability?.available || !current.draft.trim()
      || Array.from(current.draft).length > current.availability.max_length) return;
    const text = current.draft;
    update(key, (value) => ({ ...value, sending: true, resuming: current.availability?.resume_required === true, notice: null }));
    let requestId: string;
    try {
      requestId = createUuid();
    } catch (error) {
      // Preparing an ID cannot deliver anything, so the draft remains editable.
      update(key, (value) => ({
        ...value, sending: false, resuming: false,
        notice: withDetail("Message was not sent. Could not prepare the message.", errorMessage(error)),
      }));
      return;
    }
    let accepted = false;
    try {
      const result = await submitSessionInput({ session_key: key, request_id: requestId, text });
      const matchingResponse = result.request_id === requestId;
      if (matchingResponse && result.status === "accepted") {
        update(key, (value) => ({
          ...value, sending: false, resuming: false,
          draft: value.draft === text ? "" : value.draft,
          availability: value.availability?.resume_required && value.check_id === current.check_id
            ? { ...value.availability, resume_required: false, message: "" } : value.availability,
          notice: result.message || "Message sent.",
        }));
        accepted = true;
      } else if (matchingResponse && result.status === "not_sent") {
        update(key, (value) => ({ ...value, sending: false, resuming: false, notice: result.message || "Message was not sent. Try again." }));
        // Resume can lose a process lease or fail its saved-folder checks after
        // the initial capability read. Recheck without resending the draft.
        if (current.availability.resume_required) void checkAvailability(key);
      } else {
        update(key, (value) => ({
          ...value, sending: false, resuming: false, delivery_uncertain: true,
          notice: withDetail(UNCONFIRMED_MESSAGE, matchingResponse
            ? result.message
            : "The server response did not match this message."),
        }));
      }
    } catch (error) {
      update(key, (value) => ({
        ...value, sending: false, resuming: false, delivery_uncertain: true,
        notice: withDetail(UNCONFIRMED_MESSAGE, errorMessage(error)),
      }));
    }
    // A refresh failure cannot change the provider's delivery acknowledgement.
    if (accepted && mounted.current) {
      try {
        await acceptedHandler.current?.(key);
      } catch {
        update(key, (value) => ({ ...value, notice: "Message sent. The conversation could not refresh." }));
      }
    }
  }

  return {
    ...state,
    max_length: maxLength,
    length,
    can_send: !!session_key && !state.checking && !!state.availability?.available
      && !state.sending && !state.delivery_uncertain && !!state.draft.trim() && length <= maxLength,
    setDraft(text: string) {
      if (session_key) update(session_key, (value) => value.sending || value.delivery_uncertain
        ? value : { ...value, draft: text, notice: null });
    },
    editMessage() {
      if (session_key) update(session_key, (value) => ({ ...value, delivery_uncertain: false, notice: null }));
    },
    retryAvailability() {
      if (session_key) void checkAvailability(session_key);
    },
    send,
  };
}

function withDetail(message: string, detail: string): string {
  const reason = detail.trim();
  return reason && reason !== message ? `${message} Reason: ${reason}` : message;
}
