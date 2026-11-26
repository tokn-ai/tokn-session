import type { SessionControlController } from "../lib/useSessionControl";
import type { SessionTransport } from "../lib/types";
import "./SessionControlSelector.css";

export function SessionControlSelector({ control, input_busy = false, running = false }: {
  control: SessionControlController; input_busy?: boolean; running?: boolean;
}) {
  if (!control.enabled || control.status && !control.status.can_select && !control.status.can_cancel) return null;
  const busy = control.checking || control.changing || control.stopping;
  const selectable = control.status?.can_select ?? control.provider === "codex";
  const can_stop = control.status?.transport === "rpc" && control.status.can_cancel
    && control.status.available && (control.status.is_running ?? running);
  const status_text = control.checking ? "Checking control…" : control.changing ? "Changing control…"
    : control.error ?? control.notice ?? (!control.status?.available ? control.status?.message : null);
  const show_retry = (control.error || !control.status?.available) && !busy && !input_busy;
  if (!selectable && !can_stop && !status_text && !show_retry) return null;
  return <div className="session-control" role="group" aria-label="Session control">
    {selectable ? <label className="session-control__choice"><span>Connection</span>
      <select aria-label="Codex connection" value={control.status?.transport ?? "rpc"}
        disabled={busy || input_busy || !control.status} onChange={(event) => { void control.choose(event.target.value as SessionTransport); }}>
        <option value="rpc">RPC</option><option value="desktop">Codex Desktop</option>
      </select>
    </label> : null}
    {can_stop ? <button type="button" className="session-control__stop"
      disabled={busy} onClick={() => { void control.cancel(); }}>{control.stopping ? "Stopping…" : "Stop"}</button> : null}
    {status_text && <span className="session-control__status" role="status">{status_text}</span>}
    {show_retry ? <button type="button" className="text-button" onClick={() => { void control.refresh(); }}>Retry control</button> : null}
  </div>;
}
