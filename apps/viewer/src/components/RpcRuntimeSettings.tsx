import { useId, useState } from "react";
import { RPC_PROVIDER_LABELS } from "../lib/rpcRuntimeDraft";
import { isDesktop } from "../lib/transport";
import type { RpcRuntimeProfile } from "../lib/types";
import { useRpcRuntimes } from "../lib/useRpcRuntimes";
import { RpcRuntimeProfileForm } from "./RpcRuntimeProfileForm";
import { RpcRuntimeAccessSettings } from "./RpcRuntimeAccessSettings";
import "./RpcRuntimeSettings.css";

interface RpcRuntimeSettingsProps {
  active: boolean; connected: boolean; machine_key: string; machine_label: string; heading_id: string;
}

export function RpcRuntimeSettings(props: RpcRuntimeSettingsProps) {
  return <RpcRuntimeSettingsForMachine key={props.machine_key} {...props} />;
}

function RpcRuntimeSettingsForMachine({ active, connected, machine_key, machine_label, heading_id }: RpcRuntimeSettingsProps) {
  const state = useRpcRuntimes(active, connected);
  const id = useId();
  const [editor, setEditor] = useState<{ profile?: RpcRuntimeProfile; revision: number } | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const ignore = () => {};
  return <div className="rpc-runtime-settings">
    <div className="settings-page__section-heading"><span className="settings-page__scope">{machine_label}</span><h2 id={heading_id}>Agent runtimes</h2><p>Configure agent processes and RPC connections on the selected machine.</p></div>
    {machine_key === "local" && isDesktop() && <RpcRuntimeAccessSettings active={active} />}
    {!connected ? <div className="settings-page__empty"><p>Connect to a machine to manage its agent runtimes.</p></div> : <>
      <div className="rpc-runtime-settings__toolbar"><p>Saved changes apply to the next launch or connection.</p><button type="button" disabled={state.loading || !!state.busy} onClick={() => { void state.refresh(true); }}>Refresh</button><button type="button" disabled={!state.profiles || !!state.busy || !!editor} onClick={() => setEditor({ revision: state.profiles!.revision })}>Add profile</button></div>
      {state.error && <div className="settings-page__empty" role="alert"><p>{state.error}</p><button type="button" disabled={state.loading || !!state.busy} onClick={() => { void state.refresh(true); }}>Retry agent runtimes</button></div>}
      {state.live_error && <p className="settings-page__notice" role="status">{state.live_error}</p>}
      {state.loading && <p className="settings-page__hint" role="status">{state.profiles ? "Refreshing agent runtimes…" : "Loading agent runtimes…"}</p>}
      {editor && <RpcRuntimeProfileForm key={editor.profile?.profile_id ?? "new"} profile={editor.profile} revision={editor.revision} busy={!!state.busy}
        on_cancel={() => setEditor(null)} on_validate={state.validate} on_save={async (input, revision) => { await state.save(input, revision); setEditor(null); }} />}
      {!editor && state.profiles?.profiles.length === 0 && <div className="settings-page__empty"><p>No agent runtime profiles yet.</p><p>Add a profile to launch an agent here or connect to its existing RPC server.</p></div>}
      {state.profiles && state.profiles.profiles.length > 0 && <div className="rpc-runtime-settings__profiles" aria-label="Runtime profiles">
        {state.profiles.profiles.map((profile) => {
          const runtimes = state.inventory?.runtimes.filter((runtime) => runtime.profile_id === profile.profile_id) ?? [];
          const failure = state.start_failures.get(profile.profile_id);
          const failed_current = failure?.expected_revision === state.profiles!.revision;
          return <article className="rpc-runtime-profile" key={profile.profile_id} aria-labelledby={`${id}-${profile.profile_id}`}>
            <div className="rpc-runtime-profile__heading"><div><h3 id={`${id}-${profile.profile_id}`}>{profile.name}</h3><p>{RPC_PROVIDER_LABELS[profile.provider]} · {profile.mode === "managed" ? "Managed process" : "Attached connection"}</p></div>{profile.is_default && <span className="settings-page__scope">Default</span>}
              <button type="button" disabled={!!state.busy || !!editor} onClick={() => setEditor({ profile, revision: state.profiles!.revision })} aria-label={`Edit ${profile.name}`}>Edit</button>
              <button type="button" disabled={!!state.busy || !!editor || runtimes.length > 0} title={runtimes.length ? "Stop or detach the runtime before deleting its profile." : undefined} onClick={() => setDeleting(profile.profile_id)} aria-label={`Delete ${profile.name}`}>Delete</button>
              {!runtimes.length && <button type="button" disabled={!!state.busy || !!editor || !state.inventory} onClick={() => { void state.start(profile.profile_id).catch(ignore); }} aria-label={`${profile.mode === "managed" ? "Start" : "Connect"} ${profile.name}`}>{state.busy?.startsWith(`start:${profile.profile_id}:`) ? "Connecting…" : profile.mode === "managed" ? "Start" : "Connect"}</button>}
              {!runtimes.length && failed_current && failure?.can_start_new && <button type="button" disabled={!!state.busy || !!editor || state.loading} onClick={() => { void state.startNewAttempt(profile.profile_id).catch(ignore); }} aria-label={`New ${profile.mode === "managed" ? "launch" : "connection"} attempt for ${profile.name}`}>New {profile.mode === "managed" ? "launch" : "connection"} attempt</button>}
            </div>
            <p className="rpc-runtime-profile__address">{profile.mode === "managed" ? profile.executable : profile.endpoint}</p>
            {!runtimes.length && failed_current && <p className="settings-page__hint">{profile.mode === "managed" ? "Start" : "Connect"} retries the previous request. Refresh status to check whether a new attempt is available.</p>}
            {deleting === profile.profile_id && <div className="rpc-runtime-profile__confirmation"><p>Delete “{profile.name}” and its saved credentials?</p><button type="button" disabled={!!state.busy} onClick={() => setDeleting(null)}>Keep profile</button><button type="button" disabled={!!state.busy} onClick={() => { void state.remove(profile.profile_id).then(() => setDeleting(null)).catch(ignore); }}>Delete profile</button></div>}
          </article>;
        })}
      </div>}
      {state.inventory && <div className="rpc-runtime-settings__inventory"><h3>Connected runtimes</h3>
        {state.inventory.runtimes.length === 0 && <p className="settings-page__hint">No agent runtimes connected.</p>}
        {state.inventory.runtimes.map((runtime) => <article className="rpc-runtime-instance" key={runtime.instance_id} aria-labelledby={`${id}-runtime-${runtime.instance_id}`}>
          <div className="rpc-runtime-profile__heading"><div><h4 id={`${id}-runtime-${runtime.instance_id}`}>{runtime.name}</h4><p>{RPC_PROVIDER_LABELS[runtime.provider]} · {runtime.ownership === "managed" ? "Host-managed" : "External server"} · {runtime.session_count} {runtime.session_count === 1 ? "session" : "sessions"}</p></div><span className="rpc-runtime-instance__status" data-status={runtime.status}>{runtime.status.replace(/_/g, " ")}</span>
            <button type="button" disabled={!!state.busy || runtime.status === "closing" || runtime.status === "cleanup_failed"} aria-label={`${runtime.ownership === "managed" ? "Stop" : "Detach"} ${runtime.name}`} onClick={() => { void state.close(runtime).catch(ignore); }}>{state.busy === `close:${runtime.runtime_id}:${runtime.instance_id}` ? "Closing…" : runtime.ownership === "managed" ? "Stop" : "Detach"}</button>
          </div>
          {runtime.last_error && <p className="settings-page__error" role="alert">{runtime.last_error}</p>}
          <details><summary>Runtime details</summary><dl><div><dt>Runtime</dt><dd>{runtime.runtime_id}</dd></div><div><dt>Instance</dt><dd>{runtime.instance_id}</dd></div><div><dt>Profile revision</dt><dd>{runtime.profile_revision}</dd></div></dl><p>{runtime.ownership === "managed" ? "Stopping interrupts this runtime’s active work." : "Detaching leaves the external server running."}</p><p>Supported operations: {Object.entries(runtime.capabilities.operations).filter(([, capability]) => capability.supported).map(([operation]) => operation.replace(/_/g, " ")).join(", ") || "none"}.</p></details>
        </article>)}
      </div>}
    </>}
  </div>;
}
