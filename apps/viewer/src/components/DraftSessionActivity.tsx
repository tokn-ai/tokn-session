import { useLayoutEffect, useRef } from "react";
import { useSessionControl } from "../lib/useSessionControl";
import type { SessionLiveOutput as LiveOutput, SessionSummary } from "../lib/types";
import { SessionControlSelector } from "./SessionControlSelector";
import { SessionInteractions } from "./SessionInteractions";
import { SessionLiveOutput } from "./SessionLiveOutput";

/** Use only the host-verified saved session, while its original draft is retained. */
export function DraftSessionActivity({ session, revision, on_check }: {
  session: SessionSummary;
  revision: number;
  on_check: () => void;
}) {
  const control = useSessionControl(session, revision, on_check);
  const output = control.status?.live_output;
  return <section className="draft-session-activity" aria-label="Session activity">
    <h2>Session activity</h2>
    <p>Your first message is still being confirmed. You can respond to requests here.</p>
    <SessionControlSelector control={control} running={session.is_running} />
    <SessionInteractions session={session} refresh_key={revision} on_accepted={on_check} />
    {output?.parts.length ? <DraftLivePreview key={output.prompt_id} output={output} /> : null}
  </section>;
}

// A retained draft shares its page with the creation form. Bound this preview
// so long-running output doesn't push the draft's recovery controls out of reach.
function DraftLivePreview({ output }: { output: LiveOutput }) {
  const viewport = useRef<HTMLDivElement>(null);
  const following = useRef(true);
  useLayoutEffect(() => {
    if (following.current && viewport.current) viewport.current.scrollTop = viewport.current.scrollHeight;
  }, [output]);
  return <div className="draft-session-activity__preview" ref={viewport} role="region" aria-label="Live preview in draft"
    tabIndex={0} onScroll={({ currentTarget }) => {
      following.current = currentTarget.scrollHeight - currentTarget.scrollTop - currentTarget.clientHeight <= 12;
    }}>
    <SessionLiveOutput output={output} />
  </div>;
}
