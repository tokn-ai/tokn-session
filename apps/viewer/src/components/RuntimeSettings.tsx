import { useId } from "react";
import { useViewerRuntimes, type RuntimeActionState } from "../lib/useViewerRuntimes";
import type { RuntimeInfo } from "../lib/types";
import { ActivityIcon, ConnectionIcon, RefreshIcon, RestartIcon, ServerIcon, TechnicalIcon, TerminalIcon } from "./Icons";
import "./RuntimeSettings.css";

const EXECUTION_LABELS = { subprocess: "Subprocess", embedded: "In-process", external: "External connection" };
const RUNTIME_ICONS = { pi: TerminalIcon, reader: ActivityIcon, hosting: ServerIcon, hub_connection: ConnectionIcon, background_api: ServerIcon, background_connector: ConnectionIcon };
function statusLabel(runtime: RuntimeInfo): string {
  if (runtime.kind === "pi" && runtime.status === "stopped" && runtime.session_key) return "Saved";
  return runtime.status.replace(/_/g, " ").replace(/^./, (letter) => letter.toUpperCase());
}

function statusTone(runtime: RuntimeInfo, restarting: boolean): string {
  if (restarting) return "transition";
  if (["failed", "error", "unavailable"].includes(runtime.status)) return "danger";
  if (["starting", "connecting", "reconnecting", "retrying", "restarting", "stopping", "loading", "pending"].includes(runtime.status)) return "transition";
  if (runtime.kind === "pi" && runtime.status === "stopped" && runtime.session_key && runtime.ownership === "viewer") return "saved";
  if (["live", "running", "online", "idle"].includes(runtime.status)) {
    return runtime.ownership === "external" || runtime.kind === "hub_connection" ? "info" : "success";
  }
  if (["open", "registered", "background", "local", "external", "loaded", "connected"].includes(runtime.status)) return "info";
  return "neutral";
}

function RuntimeRow({ runtime, action, on_restart }: {
  runtime: RuntimeInfo; action?: RuntimeActionState; on_restart: () => void;
}) {
  const id = useId();
  const restartable = runtime.ownership === "viewer" && runtime.can_restart;
  const restarting = action?.busy === true;
  const RuntimeIcon = RUNTIME_ICONS[runtime.kind] ?? TechnicalIcon;
  const fields = [
    ["Process ID", runtime.pid === null ? null : String(runtime.pid)],
    ["Started", runtime.started_at_ms === null ? null : new Date(runtime.started_at_ms).toLocaleString()],
    ["Folder", runtime.working_directory], ["Endpoint", runtime.endpoint],
    ["Session", runtime.session_id], ["Project ID", runtime.project_id],
    ["Runtime ID", runtime.runtime_id], ["Instance ID", runtime.instance_id],
  ].filter(([, value]) => value !== null);
  return <article className="runtime-row" aria-labelledby={`${id}-name`} aria-busy={restarting || undefined}>
    <div className="runtime-row__heading">
      <span className="runtime-row__icon" data-kind={runtime.kind} aria-hidden="true"><RuntimeIcon /></span>
      <h3 id={`${id}-name`} title={runtime.name}>{runtime.name}</h3>
      <span className="runtime-row__status" data-tone={statusTone(runtime, restarting)} data-status={restarting ? "restarting" : runtime.status} title={restarting ? "Restarting" : runtime.status}>
        <span className="runtime-row__status-dot" aria-hidden="true" />{restarting ? "Restarting" : statusLabel(runtime)}
      </span>
      {runtime.ownership === "external" && runtime.status !== "external" && <span className="runtime-row__external">External</span>}
      {restartable && <button type="button" disabled={action?.busy}
        aria-label={`${action?.busy ? "Restarting" : "Restart"} ${runtime.name}`}
        onClick={on_restart}><RestartIcon className={`runtime-control-icon${restarting ? " runtime-control-icon--spinning" : ""}`} />{restarting ? "Restarting…" : "Restart"}</button>}
    </div>
    {restartable && runtime.kind === "pi" && runtime.status === "running" && <p className="runtime-row__warning">Restart interrupts the current turn.</p>}
    {runtime.last_error && <p className="settings-page__error" role="alert"><strong>Last error: </strong>{runtime.last_error}</p>}
    {action?.error && <p className="settings-page__error" role="alert"><strong>Restart: </strong>{action.error}</p>}
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
      {state.desktop && <button className="runtime-settings__refresh" aria-label="Refresh runtimes" type="button" disabled={state.loading} onClick={() => { void state.refresh(); }}>
        <RefreshIcon className={`runtime-control-icon${state.loading ? " runtime-control-icon--spinning" : ""}`} />Refresh
      </button>}
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
