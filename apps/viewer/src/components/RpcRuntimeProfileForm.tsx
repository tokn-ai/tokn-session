import { useId, useRef, useState } from "react";
import { rpcRuntimeDraft, rpcRuntimeInput, RPC_PROVIDER_LABELS, type RpcRuntimeDraft } from "../lib/rpcRuntimeDraft";
import type { RpcProvider, RpcRuntimeProfile, RpcRuntimeProfileInput, RpcRuntimeProfileValidation } from "../lib/types";

export function RpcRuntimeProfileForm({ profile, revision, busy, on_save, on_validate, on_cancel }: {
  profile?: RpcRuntimeProfile;
  revision: number;
  busy: boolean;
  on_save: (input: RpcRuntimeProfileInput, revision: number) => Promise<void>;
  on_validate: (input: RpcRuntimeProfileInput) => Promise<RpcRuntimeProfileValidation>;
  on_cancel: () => void;
}) {
  const id = useId();
  const [draft, setDraft] = useState(() => rpcRuntimeDraft(profile));
  const [error, setError] = useState<string | null>(null);
  const [validation, setValidation] = useState<RpcRuntimeProfileValidation | null>(null);
  const changes = useRef(0);
  function change(partial: Partial<RpcRuntimeDraft>) {
    changes.current += 1; setDraft((before) => ({ ...before, ...partial })); setError(null); setValidation(null);
  }
  async function submit(validate: boolean) {
    const before = changes.current;
    try {
      const input = rpcRuntimeInput(draft);
      if (!input.name) throw new Error("Enter a profile name.");
      if (validate) {
        const result = await on_validate(input);
        if (before === changes.current) setValidation(result);
      } else await on_save(input, revision);
    } catch (failure) { setError(failure instanceof Error ? failure.message : String(failure)); }
  }
  function textField(key: "name" | "executable" | "working_directory" | "endpoint" | "token" | "username" | "password", label: string, help?: string, secret = false) {
    return <div className="rpc-profile-form__field"><label htmlFor={`${id}-${key}`}>{label}</label>
      <input id={`${id}-${key}`} type={secret ? "password" : "text"} value={draft[key]} onChange={(event) => change({ [key]: event.currentTarget.value })} autoComplete="off" spellCheck={false} aria-describedby={help ? `${id}-${key}-help` : undefined} />
      {help && <p id={`${id}-${key}-help`}>{help}</p>}</div>;
  }
  return <form className="rpc-profile-form settings-page__card" onSubmit={(event) => { event.preventDefault(); void submit(false); }}>
    <h3>{profile ? "Edit runtime profile" : "New runtime profile"}</h3>
    <fieldset disabled={busy}>
      {textField("name", "Profile name")}
      <div className="rpc-profile-form__columns">
        <div className="rpc-profile-form__field"><label htmlFor={`${id}-provider`}>Agent</label><select id={`${id}-provider`} value={draft.provider} onChange={(event) => {
          const provider = event.currentTarget.value as RpcProvider;
          change({ provider, credential_action: "none", token: "", username: "", password: "", ...(provider === "pi" ? { mode: "managed" as const } : {}) });
        }}>{Object.entries(RPC_PROVIDER_LABELS).map(([value, label]) => <option value={value} key={value}>{label}</option>)}</select></div>
        <div className="rpc-profile-form__field"><label htmlFor={`${id}-mode`}>Connection</label><select id={`${id}-mode`} value={draft.mode} onChange={(event) => change({ mode: event.currentTarget.value as RpcRuntimeDraft["mode"] })}>
          <option value="managed">Launch on this machine</option><option value="attached" disabled={draft.provider === "pi"}>Connect to an endpoint</option>
        </select></div>
      </div>
      {draft.provider === "pi" && <p className="settings-page__hint">Pi requires a managed process; existing Pi process pipes cannot be attached through an endpoint.</p>}
      {draft.mode === "managed" ? <>
        {textField("executable", "Executable", "Absolute executable path on the selected machine.")}
        {(draft.provider === "codex" || draft.provider === "pi") && <div className="rpc-profile-form__field"><label htmlFor={`${id}-arguments`}>Arguments</label><textarea id={`${id}-arguments`} value={draft.arguments} rows={3} spellCheck={false} onChange={(event) => change({ arguments: event.currentTarget.value })} /><p>JSON array of arguments. The host adds the provider’s RPC launch options.</p></div>}
      </> : textField("endpoint", "RPC endpoint", draft.provider === "codex" ? "A Codex app-server ws:// or wss:// address." : "The agent’s HTTP or HTTPS API address.")}
      {(draft.mode === "managed" || draft.provider === "open_code") && textField("working_directory", "Working directory", "Required absolute project folder path on the selected machine.")}
      {draft.mode === "managed" ? <>
        <div className="rpc-profile-form__field"><label htmlFor={`${id}-environment-action`}>Environment variables</label><select id={`${id}-environment-action`} value={draft.environment_action} onChange={(event) => change({ environment_action: event.currentTarget.value as RpcRuntimeDraft["environment_action"] })}>
          <option value="keep">Keep saved values</option><option value="replace">Replace saved values</option><option value="clear">Clear saved values</option>
        </select><p>{profile?.environment_keys.length ? `Saved keys: ${profile.environment_keys.join(", ")}. Values stay on the host.` : "Values are private to the host and are never returned."}</p></div>
        {draft.environment_action === "replace" && <div className="rpc-profile-form__field"><label htmlFor={`${id}-environment`}>Replacement environment</label><textarea id={`${id}-environment`} value={draft.environment} rows={4} spellCheck={false} autoComplete="off" onChange={(event) => change({ environment: event.currentTarget.value })} /><p>JSON object of variable names and string values. Replaces the complete saved environment.</p></div>}
      </> : <>
        <div className="rpc-profile-form__field"><label htmlFor={`${id}-credentials`}>Authentication</label><select id={`${id}-credentials`} value={draft.credential_action} onChange={(event) => change({ credential_action: event.currentTarget.value as RpcRuntimeDraft["credential_action"] })}>
          <option value="keep">Keep saved credentials</option><option value="none">Clear credentials</option>{draft.provider === "open_code" ? <option value="basic">Replace username and password</option> : <option value="bearer">Replace bearer token</option>}
        </select><p>{profile?.credential_kind ? `Saved ${profile.credential_kind} credentials remain private to the host.` : "No saved credentials."}</p></div>
        {draft.credential_action === "bearer" && textField("token", "Bearer token", undefined, true)}
        {draft.credential_action === "basic" && <>{textField("username", "Username")}{textField("password", "Password", undefined, true)}</>}
      </>}
      <label className="rpc-profile-form__checkbox"><input type="checkbox" checked={draft.is_default} onChange={(event) => change({ is_default: event.currentTarget.checked })} />Default profile for {RPC_PROVIDER_LABELS[draft.provider]}</label>
    </fieldset>
    {error && <p className="settings-page__error" role="alert">{error}</p>}
    {validation && <div className="rpc-profile-form__validation" role="status">{validation.valid ? "Profile is valid on this machine." : "Check the profile settings."}
      {validation.issues.length > 0 && <ul>{validation.issues.map((issue, index) => <li key={`${issue.field}:${index}`}>{issue.field}: {issue.message}</li>)}</ul>}</div>}
    <div className="settings-page__actions"><span>Saving applies to the next launch or connection.</span><button type="button" disabled={busy} onClick={on_cancel}>Cancel</button><button type="button" disabled={busy} onClick={() => { void submit(true); }}>Validate on host</button><button className="settings-page__primary" type="submit" disabled={busy}>{busy ? "Working…" : "Save profile"}</button></div>
  </form>;
}
