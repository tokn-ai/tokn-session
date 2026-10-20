import { createUuid } from "./id";
import { useCallback, useEffect, useRef, useState } from "react";
import { getSessionInputStatus, submitSessionInput } from "./tauri";
import { errorMessage } from "./state";
import type { SessionInputStatus } from "./types";
import { captureTransport, viewerStorageScope } from "./transport";
import { addImageDrafts, disposeImageDrafts, imageLimits, releaseImageUploads, uploadImageDrafts, type ImageDraft, type ImageUploadReceipt } from "./imageDrafts";

const DEFAULT_MAX_LENGTH = 16_384;
const UNCONFIRMED_MESSAGE = "Delivery not confirmed. Check the conversation before sending again.";

interface InputState {
  draft: string;
  images: ImageDraft[];
  availability: SessionInputStatus | null;
  checking: boolean;
  check_id: number;
  sending: boolean;
  delivery_uncertain: boolean;
  notice: string | null;
}

function emptyInput(): InputState {
  return {
    draft: "", images: [], availability: null, checking: true, check_id: 0,
    sending: false, delivery_uncertain: false, notice: null,
  };
}

/** Keep drafts and in-flight results attached to their original session. */
export function useSessionInput(session_key: string | null, on_accepted?: (session_key: string) => void, refresh_key = 0) {
  const scope = viewerStorageScope();
  const current_scope = useRef(scope);
  current_scope.current = scope;
  const scopedKey = (key: string) => JSON.stringify([scope, key]);
  const states = useRef(new Map<string, InputState>());
  const check_sequence = useRef(0);
  const mounted = useRef(false);
  const acceptedHandler = useRef(on_accepted);
  acceptedHandler.current = on_accepted;
  const [, render] = useState(0);

  const update = useCallback((key: string, apply: (state: InputState) => InputState) => {
    states.current.set(key, apply(states.current.get(key) ?? emptyInput()));
    if (mounted.current) render((revision) => revision + 1);
  }, []);

  const checkAvailability = useCallback(async (key: string, target_session: string) => {
    const checkId = ++check_sequence.current;
    update(key, (state) => ({ ...state, checking: true, check_id: checkId }));
    let availability: SessionInputStatus;
    try {
      availability = await getSessionInputStatus({ session_key: target_session });
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
    return () => {
      mounted.current = false;
      for (const value of states.current.values()) disposeImageDrafts(value.images);
      states.current.clear();
    };
  }, []);

  useEffect(() => {
    if (session_key) void checkAvailability(scopedKey(session_key), session_key);
  }, [session_key, scope, checkAvailability, refresh_key]);

  const state = (session_key && states.current.get(scopedKey(session_key))) || emptyInput();
  const maxLength = state.availability?.max_length ?? DEFAULT_MAX_LENGTH;
  const length = Array.from(state.draft).length;

  async function send() {
    if (!session_key) return;
    const key = scopedKey(session_key);
    const source_session = session_key;
    // Read the ref, not the render snapshot: two clicks in one frame must send once.
    const current = states.current.get(key);
    if (!current || current.sending || current.delivery_uncertain || current.checking
      || !current.availability?.available || (!current.draft.trim() && !current.images.length)
      || (current.images.length > 0 && !imageLimits(current.availability))
      || Array.from(current.draft).length > current.availability.max_length) return;
    const text = current.draft;
    const images = current.images;
    const source_scope = scope;
    update(key, (value) => ({ ...value, sending: true, notice: null }));
    let requestId: string;
    try {
      requestId = createUuid();
    } catch (error) {
      // Preparing an ID cannot deliver anything, so the draft remains editable.
      update(key, (value) => ({
        ...value, sending: false,
        notice: withDetail("Message was not sent. Could not prepare the message.", errorMessage(error)),
      }));
      return;
    }
    let accepted = false;
    let provider_attempted = false;
    const transport = images.length ? captureTransport() : null;
    const uploads: ImageUploadReceipt[] = [];
    let disconnected = false;
    const releaseUploads = () => transport ? releaseImageUploads(uploads, transport.release) : Promise.resolve();
    const unlisten = transport?.on_close(() => { disconnected = true; void releaseUploads(); });
    try {
      let image_ids: string[] = [];
      if (transport) image_ids = await uploadImageDrafts(images, { type: "session", session_key: source_session }, transport.invoke,
        () => mounted.current && current_scope.current === source_scope && !disconnected, (receipt) => uploads.push(receipt));
      if (current_scope.current !== source_scope || disconnected || images.length && !mounted.current) throw new Error("The selected machine changed. Message was not sent.");
      provider_attempted = true;
      const request = { session_key: source_session, request_id: requestId, text, ...(image_ids.length ? { image_ids } : {}) };
      const result = transport ? await submitSessionInput(request, transport.invoke) : await submitSessionInput(request);
      const matchingResponse = result.request_id === requestId;
      if (matchingResponse && result.status === "accepted") {
        disposeImageDrafts(images);
        update(key, (value) => ({
          ...value, sending: false, images: value.images === images ? [] : value.images,
          draft: value.draft === text ? "" : value.draft,
          notice: result.message || "Message sent.",
        }));
        accepted = true;
      } else if (matchingResponse && result.status === "not_sent") {
        update(key, (value) => ({ ...value, sending: false, notice: result.message || "Message was not sent. Try again." }));
      } else {
        update(key, (value) => ({
          ...value, sending: false, delivery_uncertain: true,
          notice: withDetail(UNCONFIRMED_MESSAGE, matchingResponse
            ? result.message
            : "The server response did not match this message."),
        }));
      }
    } catch (error) {
      update(key, (value) => ({
        ...value, sending: false, delivery_uncertain: provider_attempted,
        notice: withDetail(provider_attempted ? UNCONFIRMED_MESSAGE : "Message was not sent. Could not upload images.", errorMessage(error)),
      }));
    } finally {
      unlisten?.();
      // Retained provider images survive release. Unused uploads are removed
      // after every outcome, including a host switch before provider input.
      await releaseUploads();
    }
    // A refresh failure cannot change the provider's delivery acknowledgement.
    if (accepted && mounted.current) {
      try {
        await acceptedHandler.current?.(source_session);
      } catch {
        update(key, (value) => ({ ...value, notice: "Message sent. The conversation could not refresh." }));
      }
    }
  }

  return {
    ...state,
    image_limits: imageLimits(state.availability),
    max_length: maxLength,
    length,
    can_send: !!session_key && !state.checking && !!state.availability?.available
      && !state.sending && !state.delivery_uncertain && (!!state.draft.trim() || state.images.length > 0)
      && (!state.images.length || !!imageLimits(state.availability)) && length <= maxLength,
    addImages(files: File[]) {
      if (!session_key) return;
      update(scopedKey(session_key), (value) => {
        const limits = imageLimits(value.availability);
        if (value.sending || value.delivery_uncertain || !limits) return value;
        const result = addImageDrafts(value.images, files, limits);
        return { ...value, images: result.images, notice: result.error };
      });
    },
    removeImage(draft_id: string) {
      if (!session_key) return;
      update(scopedKey(session_key), (value) => {
        if (value.sending || value.delivery_uncertain) return value;
        disposeImageDrafts(value.images.filter((image) => image.draft_id === draft_id));
        return { ...value, images: value.images.filter((image) => image.draft_id !== draft_id), notice: null };
      });
    },
    setDraft(text: string) {
      if (session_key) update(scopedKey(session_key), (value) => value.sending || value.delivery_uncertain
        ? value : { ...value, draft: text, notice: null });
    },
    editMessage() {
      if (session_key) update(scopedKey(session_key), (value) => ({ ...value, delivery_uncertain: false, notice: null }));
    },
    retryAvailability() {
      if (session_key) void checkAvailability(scopedKey(session_key), session_key);
    },
    send,
  };
}

function withDetail(message: string, detail: string): string {
  const reason = detail.trim();
  return reason && reason !== message ? `${message} Reason: ${reason}` : message;
}
