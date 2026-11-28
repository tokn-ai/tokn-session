import type { SessionControlController } from "../lib/useSessionControl";
import type { SessionTransport } from "../lib/types";
import "./SessionControlSelector.css";

export function SessionControlSelector({ control, input_busy = false, running = false, shared_connection }: {
  control: SessionControlController; input_busy?: boolean; running?: boolean;
  shared_connection?: { status_id?: string; checking: boolean; retry: (button: HTMLButtonElement) => void };
}) {
  if (!control.enabled || control.status && !control.status.can_select && !control.status.can_cancel) return null;
  const busy = control.checking || control.changing || control.stopping;
  const selectable = control.status?.can_select ?? control.provider === "codex";
  const can_stop = control.status?.transport === "rpc" && control.status.can_cancel
    && control.status.available && (control.status.is_running ?? running);
  const status_text = control.checking ? "Checking control…" : control.changing ? "Changing control…"
    : control.error ?? control.notice ?? (!control.status?.available && !shared_connection?.checking ? control.status?.message : null);
  const show_retry = (shared_connection || control.error || !control.status?.available) && !busy && !input_busy;
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
    {status_text && <span className="session-control__status" id={shared_connection?.status_id} role="status">{status_text}</span>}
    {show_retry ? <button type="button" className="text-button" aria-disabled={shared_connection?.checking || undefined}
      onClick={(event) => {
        if (shared_connection?.checking) return;
        if (shared_connection) shared_connection.retry(event.currentTarget);
        else void control.refresh();
      }}>
      {shared_connection ? "Retry connection" : "Retry control"}
    </button> : null}
  </div>;
}
