import { useEffect, useRef, useState, type RefObject } from "react";
import type { HubClient } from "../lib/hub";
import {
  createAccountKey, loadAccountKeys, MACHINE_READ, MACHINE_REGISTER, revokeAccountKey,
  type AccountKey, type AccountKeyScope,
} from "../lib/hubAccount";
import { AccountDialog, type AccountFailure } from "./AccountDialog";

const SCOPE_LABELS = { [MACHINE_READ]: "Read machines", [MACHINE_REGISTER]: "Register machines" };
type KeyDialog = { kind: "create" } | { kind: "revoke"; key: AccountKey };

export function AccountKeys({ client, on_failure }: { client: HubClient; on_failure: AccountFailure }) {
  const [keys, setKeys] = useState<AccountKey[]>();
  const [error, setError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  const [dialog, setDialog] = useState<KeyDialog>();
  const create_trigger = useRef<HTMLButtonElement>(null);
  const revoke_trigger = useRef<HTMLElement | null>(null);
  const read_generation = useRef(0);

  useEffect(() => {
    let current = true;
    const generation = ++read_generation.current;
    void loadAccountKeys(client).then((next) => {
      if (current && generation === read_generation.current && !client.signal.aborted) setKeys(next);
    }).catch((error: unknown) => {
      if (current && generation === read_generation.current && !client.signal.aborted) on_failure(error, setError);
    });
    return () => { current = false; };
  }, [client, attempt]);

  function refresh() {
    // Retire old reads immediately, before React cleans up their effects.
    read_generation.current++;
    setError(undefined);
    setAttempt((value) => value + 1);
  }

  return <section className="account-section" aria-labelledby="account-keys-title">
    <div className="account-section-heading">
      <div className="account-section-copy">
        <h2 id="account-keys-title">API keys</h2>
        <p>Give your scripts access to read or register machines.</p>
      </div>
      <button ref={create_trigger} className="account-primary" type="button" aria-haspopup="dialog"
        onClick={() => setDialog({ kind: "create" })}>Create API key</button>
    </div>
    {error && <div className="account-feedback">
      <p className="account-error" role="alert">{error}</p>
      <button type="button" onClick={refresh}>Retry loading keys</button>
    </div>}
    {!keys ? !error && <p role="status">Loading keys…</p>
      : keys.length === 0 ? <div className="account-empty"><p>No API keys yet.</p><p>Create a key when you’re ready to connect a script.</p></div>
        : <ul className="account-key-list" aria-label="Account API keys">{keys.map((key) => <li className="account-key-row" key={key.key_id}>
          <div className="account-key-details">
            <h3>{key.name}</h3>
            <p className="account-key-scopes">{key.scopes.map((scope) => SCOPE_LABELS[scope]).join(" · ")}</p>
            <p>Expires {new Date(key.expires_at * 1000).toLocaleDateString()}</p>
          </div>
          <button type="button" aria-label={`Revoke ${key.name}`} aria-haspopup="dialog" onClick={(event) => {
            revoke_trigger.current = event.currentTarget;
            setDialog({ kind: "revoke", key });
          }}>Revoke</button>
        </li>)}</ul>}
    {dialog?.kind === "create" && <CreateKeyDialog client={client} return_focus={create_trigger}
      on_close={() => setDialog(undefined)} on_failure={on_failure} on_created={(key) => {
        // Only metadata reaches the overview; the one-time secret stays in the dialog.
        setKeys((previous) => [key, ...(previous ?? []).filter((entry) => entry.key_id !== key.key_id)]);
        refresh();
      }} />}
    {dialog?.kind === "revoke" && <RevokeKeyDialog client={client} account_key={dialog.key} return_focus={revoke_trigger}
      on_close={() => setDialog(undefined)} on_failure={on_failure} on_revoked={() => {
        revoke_trigger.current = create_trigger.current;
        setKeys((previous) => previous?.filter((key) => key.key_id !== dialog.key.key_id));
        setDialog(undefined);
        refresh();
      }} />}
  </section>;
}

function CreateKeyDialog({ client, return_focus, on_close, on_failure, on_created }: {
  client: HubClient;
  return_focus: RefObject<HTMLElement | null>;
  on_close: () => void;
  on_failure: AccountFailure;
  on_created: (key: AccountKey) => void;
}) {
  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<AccountKeyScope[]>([MACHINE_READ]);
  const [created, setCreated] = useState<{ token: string }>();
  const [copy_notice, setCopyNotice] = useState<string>();
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const name_input = useRef<HTMLInputElement>(null);
  const done_button = useRef<HTMLButtonElement>(null);
  const submitting = useRef(false);

  useEffect(() => {
    if (created) done_button.current?.focus();
  }, [created]);

  async function create() {
    if (submitting.current || !name.trim() || scopes.length === 0) return;
    submitting.current = true;
    setBusy(true);
    setError(undefined);
    try {
      const { token, ...metadata } = await createAccountKey(client, name.trim(), scopes);
      if (client.signal.aborted) return;
      setCreated({ token });
      on_created(metadata);
    } catch (error) {
      if (!client.signal.aborted) on_failure(error, setError);
    } finally {
      submitting.current = false;
      if (!client.signal.aborted) setBusy(false);
    }
  }

  return <AccountDialog title={created ? "API key created" : "Create API key"}
    description={created ? "Copy your key before closing this window. You won’t be able to see it again." : "Name this key and choose what it can access."}
    busy={busy} on_close={on_close} return_focus={return_focus} initial_focus={name_input}>
    {created ? <>
      <div className="account-key-value"><code>{created.token}</code></div>
      {copy_notice && <p className="account-feedback" role="status">{copy_notice}</p>}
      <div className="account-dialog-actions">
        {navigator.clipboard?.writeText && <button type="button" onClick={() => {
          void navigator.clipboard.writeText(created.token).then(() => setCopyNotice("API key copied."))
            .catch(() => setCopyNotice("Select the key and copy it manually."));
        }}>Copy API key</button>}
        <button ref={done_button} className="account-primary" type="button" onClick={on_close}>Done</button>
      </div>
    </> : <form className="account-form" onSubmit={(event) => { event.preventDefault(); void create(); }}>
      <label htmlFor="account-key-name">Key name</label>
      <input ref={name_input} id="account-key-name" required maxLength={80} value={name} disabled={busy}
        placeholder="For example, deployment script" onChange={(event) => setName(event.target.value)} />
      <fieldset className="account-permissions" disabled={busy}>
        <legend>Permissions</legend>
        {([MACHINE_READ, MACHINE_REGISTER] as const).map((scope) => <label key={scope}>
          <input type="checkbox" checked={scopes.includes(scope)} onChange={(event) => {
            setScopes(event.target.checked ? [...scopes, scope] : scopes.filter((selected) => selected !== scope));
          }} />{SCOPE_LABELS[scope]}
        </label>)}
      </fieldset>
      {error && <p className="account-error" role="alert">{error}</p>}
      <div className="account-dialog-actions">
        <button type="button" disabled={busy} onClick={on_close}>Cancel</button>
        <button className="account-primary" type="submit" disabled={busy || !name.trim() || scopes.length === 0}>
          {busy ? "Creating…" : "Create API key"}
        </button>
      </div>
    </form>}
  </AccountDialog>;
}

function RevokeKeyDialog({ client, account_key, return_focus, on_close, on_failure, on_revoked }: {
  client: HubClient;
  account_key: AccountKey;
  return_focus: RefObject<HTMLElement | null>;
  on_close: () => void;
  on_failure: AccountFailure;
  on_revoked: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const cancel_button = useRef<HTMLButtonElement>(null);
  const submitting = useRef(false);

  async function revoke() {
    if (submitting.current) return;
    submitting.current = true;
    setBusy(true);
    setError(undefined);
    try {
      await revokeAccountKey(client, account_key.key_id);
      if (!client.signal.aborted) on_revoked();
    } catch (error) {
      if (!client.signal.aborted) on_failure(error, setError);
    } finally {
      submitting.current = false;
      if (!client.signal.aborted) setBusy(false);
    }
  }

  return <AccountDialog title="Revoke API key" description="Scripts using this key will lose access to your machines."
    busy={busy} on_close={on_close} return_focus={return_focus} initial_focus={cancel_button}>
    <p>Revoke <strong>{account_key.name}</strong>?</p>
    {error && <p className="account-error" role="alert">{error}</p>}
    <div className="account-dialog-actions">
      <button ref={cancel_button} type="button" disabled={busy} onClick={on_close}>Cancel</button>
      <button className="account-danger" type="button" disabled={busy} onClick={() => { void revoke(); }}>
        {busy ? "Revoking…" : "Confirm revoke"}
      </button>
    </div>
  </AccountDialog>;
}
