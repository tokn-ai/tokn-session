import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createUuid } from "./id";
import { errorMessage } from "./state";
import { listSessionInteractions, respondSessionInteraction } from "./tauri";
import { captureTransport, viewerStorageScope } from "./transport";
import type { SessionInteractionResponse, SessionInteractionsResponse } from "./types";

export interface InteractionSubmission {
  status: "sending" | "accepted" | "uncertain" | "not_sent";
  message: string;
}
interface InteractionView {
  owner: string;
  snapshot: SessionInteractionsResponse | null;
  checking: boolean;
  error: string | null;
  live_error: string | null;
}
const emptyView = (owner: string): InteractionView => ({ owner, snapshot: null, checking: false, error: null, live_error: null });

/** Live callbacks belong to one machine, session and runtime instance. */
export function useSessionInteractions(session_key: string | null, refresh_key = 0, on_accepted?: (session_key: string) => void) {
  const scope = viewerStorageScope();
  const transport = useMemo(captureTransport, [scope]);
  const owner = JSON.stringify([scope, session_key]);
  const current_owner = useRef(owner);
  current_owner.current = owner;
  const active = useRef(false);
  const sequence = useRef(0);
  const submissions = useRef(new Map<string, InteractionSubmission>());
  const [view, setView] = useState(() => emptyView(owner));
  const [, render] = useState(0);
  const value = view.owner === owner ? view : emptyView(owner);
  const latest = useRef(value);
  latest.current = value;
  const accepted_handler = useRef(on_accepted);
  accepted_handler.current = on_accepted;

  const update = useCallback((apply: (before: InteractionView) => InteractionView) => {
    if (active.current && current_owner.current === owner) {
      setView((before) => apply(before.owner === owner ? before : emptyView(owner)));
    }
  }, [owner]);
  const refresh = useCallback(async () => {
    if (!session_key || !active.current || current_owner.current !== owner) return;
    const read = ++sequence.current;
    update((before) => ({ ...before, checking: true, error: null }));
    try {
      const snapshot = await listSessionInteractions({ session_key }, transport.invoke);
      if (read !== sequence.current || current_owner.current !== owner) return;
      if (snapshot.session_key !== session_key || snapshot.interactions.length > 0 && (!snapshot.runtime_id || !snapshot.instance_id)) {
        throw new Error("Pending requests identify another session or an unavailable runtime.");
      }
      const prefix = `${JSON.stringify([owner]).slice(0, -1)},`;
      const pending = new Set(snapshot.interactions.map((interaction) => JSON.stringify([
        owner, snapshot.runtime_id, snapshot.instance_id, interaction.interaction_id,
      ])));
      for (const key of submissions.current.keys()) {
        if (key.startsWith(prefix) && !pending.has(key)) submissions.current.delete(key);
      }
      update((before) => ({ ...before, snapshot, checking: false, error: null }));
    } catch (error) {
      if (read === sequence.current) update((before) => ({ ...before, checking: false, error: errorMessage(error) }));
    }
  }, [owner, session_key, transport, update]);

  useEffect(() => {
    active.current = true;
    const stop = transport.on_close(() => {
      ++sequence.current;
      update((before) => ({ ...before, snapshot: null, checking: false, error: "Machine disconnected. Reconnect to answer pending requests." }));
    });
    return () => { active.current = false; ++sequence.current; stop(); };
  }, [owner, transport, update]);
  useEffect(() => {
    if (!session_key) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void transport.listen("rpc-runtimes-changed", () => { if (!cancelled) void refresh(); }).then((stop) => {
      if (cancelled) stop();
      else { unlisten = stop; update((before) => ({ ...before, live_error: null })); void refresh(); }
    }).catch(() => {
      if (!cancelled) {
        update((before) => ({ ...before, live_error: "Live request updates are unavailable. Refresh requests to check their status." }));
        void refresh();
      }
    });
    const focus = () => { if (!cancelled) void refresh(); };
    window.addEventListener("focus", focus);
    return () => { cancelled = true; unlisten?.(); window.removeEventListener("focus", focus); };
  }, [refresh, session_key, transport, update]);
  useEffect(() => { void refresh(); }, [refresh, refresh_key]);

  const interactionKey = (interaction_id: string) => JSON.stringify([
    owner, value.snapshot?.runtime_id, value.snapshot?.instance_id, interaction_id,
  ]);
  const submission = (interaction_id: string) => submissions.current.get(interactionKey(interaction_id));
  async function respond(interaction_id: string, response: SessionInteractionResponse) {
    const snapshot = latest.current.snapshot;
    if (!session_key || !active.current || current_owner.current !== owner || latest.current.checking || latest.current.error
      || !snapshot?.runtime_id || !snapshot.instance_id || snapshot !== value.snapshot
      || !snapshot.interactions.some((interaction) => interaction.interaction_id === interaction_id && !interaction.response_pending)) return;
    const key = interactionKey(interaction_id);
    const previous = submissions.current.get(key);
    if (previous && previous.status !== "not_sent") return;
    const setSubmission = (next: InteractionSubmission) => {
      submissions.current.delete(key);
      submissions.current.set(key, next);
      if (submissions.current.size > 128) {
        for (const [old_key, previous] of submissions.current) {
          if (previous.status !== "sending") { submissions.current.delete(old_key); break; }
        }
      }
      if (active.current && current_owner.current === owner) render((revision) => revision + 1);
    };
    let request_id: string;
    try { request_id = createUuid(); }
    catch (error) { setSubmission({ status: "not_sent", message: `Could not prepare the response. ${errorMessage(error)}` }); return; }
    setSubmission({ status: "sending", message: "Submitting response…" });
    let accepted = false;
    try {
      const result = await respondSessionInteraction({
        session_key, request_id, runtime_id: snapshot.runtime_id, instance_id: snapshot.instance_id, interaction_id, response,
      }, transport.invoke);
      if (result.request_id !== request_id || !["accepted", "not_sent"].includes(result.status)) {
        setSubmission({ status: "uncertain", message: "Response delivery is not confirmed. Refresh requests to check its status." });
      } else if (result.status === "accepted") {
        setSubmission({ status: "accepted", message: "Response sent. Waiting for the request to close." });
        accepted = true;
      } else {
        setSubmission({ status: "not_sent", message: result.message || "Response was not sent. Your answer is preserved." });
      }
    } catch (error) {
      setSubmission({ status: "uncertain", message: `Response delivery is not confirmed. ${errorMessage(error)}` });
    }
    if (active.current && current_owner.current === owner) {
      void refresh();
      if (accepted) accepted_handler.current?.(session_key);
    }
  }

  return { ...value, interactionKey, submission, respond, refresh };
}
