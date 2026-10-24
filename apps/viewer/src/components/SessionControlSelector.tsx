import type { SessionControlController } from "../lib/useSessionControl";
import type { SessionTransport } from "../lib/types";
import "./SessionControlSelector.css";

export function SessionControlSelector({ control, input_busy = false, running = false }: {
  control: SessionControlController; input_busy?: boolean; running?: boolean;
}) {
  if (!control.enabled || control.status && !control.status.can_select && !control.status.can_cancel) return null;
  const busy = control.checking || control.changing || control.stopping;
  const selectable = control.status?.can_select ?? control.provider === "codex";
  return <div className="session-control" aria-label="Session control">
    {selectable ? <label className="session-control__choice"><span>Control</span>
      <select aria-label="Codex connection" value={control.status?.transport ?? "rpc"}
        disabled={busy || input_busy || !control.status} onChange={(event) => { void control.choose(event.target.value as SessionTransport); }}>
        <option value="rpc">RPC</option><option value="desktop">Codex Desktop</option>
      </select>
    </label> : null}
    {control.status?.transport === "rpc" && control.status.can_cancel && control.status.available && running ? <button type="button" className="text-button"
      disabled={busy} onClick={() => { void control.cancel(); }}>{control.stopping ? "Stopping…" : "Stop"}</button> : null}
    <span className="session-control__status" role="status">
      {control.checking ? "Checking control…" : control.changing ? "Changing control…" : control.error ?? control.notice ?? (!control.status?.available ? control.status?.message : null)}
    </span>
    {(control.error || !control.status?.available) && !busy && !input_busy ? <button type="button" className="text-button" onClick={() => { void control.refresh(); }}>Retry control</button> : null}
  </div>;
}
