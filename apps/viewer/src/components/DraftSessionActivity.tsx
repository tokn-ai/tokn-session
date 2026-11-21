import { useSessionControl } from "../lib/useSessionControl";
import type { SessionSummary } from "../lib/types";
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
  return <section className="draft-session-activity" aria-label="Session activity">
    <h2>Session activity</h2>
    <p>Your first message is still being confirmed. You can respond to requests here.</p>
    <SessionControlSelector control={control} running={session.is_running} />
    <SessionInteractions session={session} refresh_key={revision} on_accepted={on_check} />
    <SessionLiveOutput output={control.status?.live_output} />
  </section>;
}
