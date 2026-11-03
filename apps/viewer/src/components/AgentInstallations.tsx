import { useId, useState } from "react";
import { RPC_PROVIDER_LABELS } from "../lib/rpcRuntimeDraft";
import type { AgentInstallation, RpcProvider, RuntimeInstallJob } from "../lib/types";
import { installationJobActive, useAgentInstallations } from "../lib/useAgentInstallations";
import "./AgentInstallations.css";

const job_labels: Record<RuntimeInstallJob["state"], string> = {
  resolving: "Checking latest release…", downloading: "Downloading…", installing: "Installing…",
  verifying: "Verifying…", completed: "Ready", failed: "Download failed", cancelled: "Download cancelled",
};

function transferSize(bytes: number): string {
  return bytes < 1024 * 1024 ? `${Math.max(1, Math.round(bytes / 1024))} KB` : `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function DownloadProgress({ job, label }: { job: RuntimeInstallJob; label: string }) {
  const total = job.total_bytes && job.total_bytes > 0 ? job.total_bytes : null;
  const value = total ? Math.min(100, Math.round(job.downloaded_bytes / total * 100)) : undefined;
  return <div className="agent-installation__progress" role="status">
    <span>{job_labels[job.state]}</span>
    <progress aria-label={`${label} download progress`} max={100} value={value} />
    {job.downloaded_bytes > 0 && <small>{transferSize(job.downloaded_bytes)}{total ? ` of ${transferSize(total)}` : ""}</small>}
  </div>;
}

export function AgentInstallations({ active, connected, machine_label, runtime_error }: { active: boolean; connected: boolean; machine_label: string; runtime_error?: string | null }) {
  const state = useAgentInstallations(active, connected);
  const id = useId();
  const [removing, setRemoving] = useState<{ provider: RpcProvider; generation: string } | null>(null);
  return <section className="agent-installations" aria-labelledby={`${id}-title`}>
    <div className="agent-installations__heading"><div><h3 id={`${id}-title`}>Agent downloads</h3>
      <p>Install the latest release on {machine_label}. One version per agent, updated when you choose.</p></div>
      <button type="button" disabled={state.loading || !!state.busy || !connected} onClick={() => { void state.refresh(true); }} aria-label="Refresh agent downloads">Refresh</button>
    </div>
    {state.error && state.error !== runtime_error && <div className="agent-installations__error" role="alert"><p>{state.error}</p><button type="button" disabled={state.loading || !!state.busy} onClick={() => { void state.refresh(true); }}>Retry download status</button></div>}
    {state.live_error && <p className="settings-page__notice" role="status">{state.live_error}</p>}
    {state.loading && !state.inventory && <p className="settings-page__hint" role="status">Checking installed agents…</p>}
    {state.inventory && <div className="agent-installations__grid">
      {state.inventory.agents.map((agent) => {
        const label = RPC_PROVIDER_LABELS[agent.provider];
        const running = installationJobActive(agent.job);
        const disabled = !active || !connected || !!state.busy || agent.in_use || running;
        const reason = agent.in_use && !running ? "Stop this agent’s active runtimes, or wait for another installation to finish." : agent.unavailable_reason;
        return <article className="agent-installation" key={agent.provider} aria-labelledby={`${id}-${agent.provider}`}>
          <div className="agent-installation__heading"><h4 id={`${id}-${agent.provider}`}>{label}</h4><span data-installed={!!agent.version}>{agent.version ? `v${agent.version}` : "Not installed"}</span></div>
          <p className="agent-installation__detail">{agent.requires_node ? "Uses this host’s Node/npm" : "Native executable"}{agent.in_use && !running ? " · In use" : ""}</p>
          {reason && <p className="agent-installation__notice">{reason}</p>}
          {agent.job && running && <DownloadProgress job={agent.job} label={label} />}
          {agent.job?.state === "failed" && <p className="agent-installation__error" role="alert">{agent.job.error || "The download failed. Try again."}</p>}
          {agent.job?.state === "cancelled" && <p className="agent-installation__notice">Download cancelled.{agent.version ? " The installed version is ready to use." : ""}</p>}
          <div className="agent-installation__actions">
            {agent.job && running ? <button type="button" disabled={!!state.busy || !connected} onClick={() => { void state.cancel(agent.job!); }} aria-label={`Cancel ${label} download`}>{state.busy === agent.provider ? "Cancelling…" : "Cancel"}</button>
              : <button type="button" disabled={disabled || !agent.available} title={reason ?? undefined} onClick={() => { void state.start(agent.provider); }} aria-label={`${agent.version ? "Update" : "Download"} ${label}`}>{state.busy === agent.provider ? "Starting…" : agent.version ? "Update" : "Download"}</button>}
            {agent.version && agent.generation && !running && <button type="button" disabled={disabled} title={agent.in_use ? reason ?? undefined : undefined} onClick={() => setRemoving({ provider: agent.provider, generation: agent.generation! })} aria-label={`Remove ${label}`}>Remove</button>}
          </div>
          {removing?.provider === agent.provider && <RemoveConfirmation agent={agent} label={label} busy={!!state.busy} on_cancel={() => setRemoving(null)} on_remove={() => {
            void state.remove(removing.provider, removing.generation).then((removed) => { if (removed) setRemoving(null); });
          }} />}
        </article>;
      })}
    </div>}
    <p className="agent-installations__footnote">Downloads stay in Tokn’s data folder on this machine. Saved profiles and executable overrides take priority. Installing an agent does not sign in to its model provider.</p>
  </section>;
}

function RemoveConfirmation({ agent, label, busy, on_cancel, on_remove }: { agent: AgentInstallation; label: string; busy: boolean; on_cancel: () => void; on_remove: () => void }) {
  return <div className="agent-installation__confirmation"><p>Remove the downloaded {label} agent? Saved sessions and agent settings are kept.</p>
    <div className="agent-installation__actions"><button type="button" disabled={busy} onClick={on_cancel}>Keep {label}</button><button type="button" disabled={busy || agent.in_use || installationJobActive(agent.job)} onClick={on_remove}>Remove downloaded {label}</button></div>
  </div>;
}
