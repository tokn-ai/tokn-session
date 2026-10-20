import { useId } from "react";
import { useViewerRuntimes, type RuntimeActionState } from "../lib/useViewerRuntimes";
import type { RuntimeInfo } from "../lib/types";
import "./RuntimeSettings.css";

const EXECUTION_LABELS = { subprocess: "Subprocess", embedded: "In-process", external: "External connection" };
function statusLabel(runtime: RuntimeInfo): string {
  if (runtime.kind === "pi" && runtime.status === "stopped" && runtime.session_key) return "Saved";
  return runtime.status.replace(/_/g, " ").replace(/^./, (letter) => letter.toUpperCase());
}

function RuntimeRow({ runtime, action, on_restart }: {
  runtime: RuntimeInfo; action?: RuntimeActionState; on_restart: () => void;
}) {
  const id = useId();
  const restartable = runtime.ownership === "viewer" && runtime.can_restart;
  const fields = [
    ["Process ID", runtime.pid === null ? null : String(runtime.pid)],
    ["Started", runtime.started_at_ms === null ? null : new Date(runtime.started_at_ms).toLocaleString()],
    ["Folder", runtime.working_directory], ["Endpoint", runtime.endpoint],
    ["Session", runtime.session_id], ["Project ID", runtime.project_id],
    ["Runtime ID", runtime.runtime_id], ["Instance ID", runtime.instance_id],
  ].filter(([, value]) => value !== null);
  return <article className="runtime-row" aria-labelledby={`${id}-name`}>
    <div className="runtime-row__heading">
      <h3 id={`${id}-name`} title={runtime.name}>{runtime.name}</h3>
      <span className="runtime-row__status" data-status={runtime.status} title={runtime.status}>{statusLabel(runtime)}</span>
      {runtime.ownership === "external" && runtime.status !== "external" && <span className="runtime-row__external">External</span>}
      {restartable && <button type="button" disabled={action?.busy}
        aria-label={`${action?.busy ? "Restarting" : "Restart"} ${runtime.name}`}
        onClick={on_restart}>{action?.busy ? "Restarting…" : "Restart"}</button>}
    </div>
    {restartable && runtime.kind === "pi" && runtime.status === "running" && <p className="runtime-row__warning">Restart interrupts the current turn.</p>}
    {runtime.last_error && <p className="settings-page__error" role="alert"><strong>Last error: </strong>{runtime.last_error}</p>}
    {action?.error && <p className="settings-page__error" role="alert"><strong>Restart: </strong>{action.error}</p>}
    {action?.message && <p className="runtime-row__message" role="status">{action.message}</p>}
    <details className="runtime-row__details"><summary>Details</summary><div className="runtime-row__metadata">
      <p className="runtime-row__ownership">{runtime.ownership === "viewer" ? "Viewer-managed" : "Externally managed"} · {EXECUTION_LABELS[runtime.execution_kind]}</p>
      {runtime.detail && <p className="runtime-row__detail">{runtime.detail}</p>}
      {!restartable && <p className="runtime-row__reason">{runtime.restart_disabled_reason ?? "This runtime cannot be restarted by this viewer."}</p>}
      <dl>{fields.map(([label, value]) => <div key={label}><dt>{label}</dt><dd>{value}</dd></div>)}</dl>
    </div></details>
  </article>;
}

export function RuntimeSettings({ active, heading_id }: { active: boolean; heading_id?: string }) {
  const fallback_id = useId();
  const state = useViewerRuntimes(active);
  return <div className="runtime-settings">
    <header className="runtime-settings__header">
      <h2 id={heading_id ?? fallback_id}>Runtimes</h2><span>This viewer</span>
      {state.desktop && <button className="runtime-settings__refresh" aria-label="Refresh runtimes" type="button" disabled={state.loading} onClick={() => { void state.refresh(); }}>Refresh</button>}
    </header>
    {!state.desktop ? <p className="runtime-settings__notice">Runtime management is available in this computer’s desktop app.</p> : <>
      {state.error && <div className="runtime-settings__notice" role="alert">
        <strong>{state.inventory ? "Status may be outdated." : "Runtimes unavailable."}</strong>
        <p>{state.error}</p><button aria-label="Retry runtime inventory" type="button" disabled={state.loading} onClick={() => { void state.refresh(); }}>Retry</button>
      </div>}
      {state.live_error && <p className="runtime-settings__notice" role="status">{state.live_error}</p>}
      {state.loading && <p className="runtime-settings__loading" role="status">{state.inventory ? "Refreshing runtime status…" : "Loading runtimes…"}</p>}
      {!state.inventory && !state.error && !state.loading && <p role="status">Loading runtimes…</p>}
      {state.inventory?.runtimes.length === 0 && <div className="settings-page__empty"><p>No runtimes are known to this viewer.</p></div>}
      {state.inventory?.runtimes.map((runtime) => <RuntimeRow key={runtime.runtime_id} runtime={runtime}
        action={state.actions.get(runtime.runtime_id)} on_restart={() => { void state.restart(runtime); }} />)}
      <p className="runtime-settings__footer">Managed runtimes stop on quit.</p>
    </>}
  </div>;
}
