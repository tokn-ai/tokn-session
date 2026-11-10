import { useCallback, useEffect, useRef, useState } from "react";
import { HubClient, HubError, hubErrorMessage } from "../lib/hub";
import {
  accountSession, approveAccountDevice, browserAccountClient, createAccountKey, hubAccountAvailability,
  loadAccountKeys, loadAccountMachines, logoutAccount, MACHINE_READ, MACHINE_REGISTER,
  refreshBrowserAccount, requestAccountCode, revokeAccountKey, verifyAccountCode,
  type AccountKey, type AccountKeyScope, type AccountSession, type HubAccountAvailability,
} from "../lib/hubAccount";
import type { ResolvedMachine } from "../lib/types";
import { HubAccess } from "./HubAccess";
import "./HubAccount.css";

const ACCOUNT_CHANGED = "Your Hub account changed in another tab. Review the active account before continuing.";
const SESSION_EXPIRED = "Your Hub session expired. Sign in again.";

export function HubAccountLink() {
  const [supported, setSupported] = useState(false);
  useEffect(() => {
    const client = browserAccountClient();
    let available = false;
    let refreshing = false;
    function refresh() {
      if (!available || refreshing || client.signal.aborted) return;
      refreshing = true;
      void refreshBrowserAccount(client).catch(() => {}).finally(() => { refreshing = false; });
    }
    window.addEventListener("focus", refresh);
    void hubAccountAvailability(client).then((availability) => {
      if (client.signal.aborted) return;
      available = availability !== "unsupported";
      setSupported(available);
      refresh();
    }).catch(() => {});
    return () => { window.removeEventListener("focus", refresh); client.close(); };
  }, []);
  return supported ? <a href="/account">Hub account</a> : null;
}

export function HubAccount() {
  const [availability, setAvailability] = useState<HubAccountAvailability>();
  const [check_error, setCheckError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  const [session, setSession] = useState<AccountSession>();
  const [signing_out, setSigningOut] = useState(false);
  const [sign_out_error, setSignOutError] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const [selected_machine, setSelectedMachine] = useState<ResolvedMachine>();
  const [user_code, setUserCode] = useState(() => {
    const value = new URLSearchParams(window.location.search).get("user_code");
    return value && /^[A-Za-z0-9-]{1,32}$/.test(value) ? value : "";
  });
  const [approved, setApproved] = useState(false);
  const active = useRef<AccountSession>(undefined);

  const signedOut = useCallback((message?: string) => {
    active.current?.client.close();
    active.current = undefined;
    setSession(undefined);
    setApproved(false);
    setSignOutError(undefined);
    setNotice(message);
  }, []);

  useEffect(() => {
    const client = browserAccountClient();
    let refreshing = false;
    async function restoreSession() {
      if (refreshing || client.signal.aborted) return;
      refreshing = true;
      const previous = active.current;
      try {
        const identity = await refreshBrowserAccount(client);
        if (client.signal.aborted || active.current !== previous) return;
        if (!identity) {
          if (previous) signedOut(SESSION_EXPIRED);
          return;
        }
        if (identity.user_id === previous?.user_id) return;
        if (previous) signedOut(ACCOUNT_CHANGED);
        const restored = accountSession(identity);
        active.current = restored;
        setSession(restored);
      } finally {
        refreshing = false;
      }
    }
    const refreshOnFocus = () => { if (active.current) void restoreSession().catch(() => {}); };
    setCheckError(undefined);
    const cancel = () => client.close();
    window.addEventListener("focus", refreshOnFocus);
    window.addEventListener("pagehide", cancel);
    void hubAccountAvailability(client).then(async (next) => {
      if (next !== "unsupported") await restoreSession();
      if (!client.signal.aborted) setAvailability(next);
    }).catch((error: unknown) => {
      if (!client.signal.aborted) setCheckError(hubErrorMessage(error));
    });
    return () => {
      window.removeEventListener("focus", refreshOnFocus);
      window.removeEventListener("pagehide", cancel);
      cancel();
    };
  }, [attempt, signedOut]);

  useEffect(() => {
    const lock = () => { active.current?.client.close(); active.current = undefined; };
    const hide = () => { lock(); setSession(undefined); setSelectedMachine(undefined); };
    const restore = (event: PageTransitionEvent) => { if (event.persisted) window.location.reload(); };
    window.addEventListener("pagehide", hide);
    window.addEventListener("pageshow", restore);
    return () => {
      window.removeEventListener("pagehide", hide);
      window.removeEventListener("pageshow", restore);
      lock();
    };
  }, []);

  function signedIn(next: AccountSession) {
    active.current = next;
    setNotice(undefined);
    setSignOutError(undefined);
    setSession(next);
  }

  async function logout() {
    if (!session || signing_out) return;
    const { client } = session;
    setSigningOut(true);
    setSignOutError(undefined);
    try {
      await logoutAccount(client);
      if (!client.signal.aborted) signedOut();
    } catch (error) {
      onFailure(error, setSignOutError);
    } finally {
      setSigningOut(false);
    }
  }

  function onFailure(error: unknown, report: (message: string) => void) {
    if (session?.client.signal.aborted) return;
    if (error instanceof HubError && error.status === 409 && error.details?.account_session_changed === true) {
      signedOut(ACCOUNT_CHANGED);
      setAvailability(undefined);
      setAttempt((value) => value + 1);
    } else if (error instanceof HubError && error.status === 401) {
      signedOut(SESSION_EXPIRED);
    } else report(hubErrorMessage(error));
  }

  if (selected_machine) return <HubAccess initial_machine={selected_machine}
    account_control={<button type="button" onClick={() => setSelectedMachine(undefined)}>Back to Hub account</button>} />;

  if (!session) return <main className="machine-connect hub-login hub-account-login">
    {availability === "available" ? <AccountSignIn user_code={user_code} notice={notice} on_signed_in={signedIn} />
      : <form onSubmit={(event) => { event.preventDefault(); setAttempt((value) => value + 1); }}>
        <h1>Hub account</h1>
        <a href="/">Open your machines</a>
        {check_error ? <><p role="alert">{check_error}</p><button>Retry connection</button></>
          : availability === "email_unconfigured" ? <p>Ask your Hub administrator to set up email delivery for account sign-in.</p>
            : availability === "unsupported" ? <p>Account sign-in is unavailable on this Hub.</p>
              : <p role="status">Connecting…</p>}
      </form>}
  </main>;

  return <main className="hub-home hub-account" key={session.user_id}><div className="hub-content">
    <header className="hub-header">
      <div><p className="hub-eyebrow">Tokn Hub</p><h1>Hub account</h1></div>
      <div className="hub-actions"><a href="/">Open your machines</a>
        <button type="button" onClick={() => { void logout(); }} disabled={signing_out}>{signing_out ? "Signing out…" : "Sign out"}</button>
      </div>
    </header>
    <section className="account-section" aria-labelledby="account-profile-title">
      <h2 id="account-profile-title">Profile</h2>
      <p>Signed in as <strong>{session.username}</strong></p>
      <p>Account ID <code>{session.user_id}</code></p>
      {notice && <p role="status">{notice}</p>}
      {sign_out_error && <p className="hub-error" role="alert">{sign_out_error}</p>}
    </section>
    <AccountDeviceApproval client={session.client} initial_code={user_code}
      approved={approved} on_approved={() => { setUserCode(""); setApproved(true); }}
      on_reset={() => setApproved(false)} on_failure={onFailure} />
    <AccountMachines session={session} on_select={setSelectedMachine} on_failure={onFailure} />
    <AccountKeys client={session.client} on_failure={onFailure} />
  </div></main>;
}

function AccountSignIn({ user_code, notice, on_signed_in }: {
  user_code: string;
  notice?: string;
  on_signed_in: (session: AccountSession) => void;
}) {
  const [step, setStep] = useState<"email" | "code" | "username">("email");
  const [email, setEmail] = useState("");
  const [code, setCode] = useState("");
  const [username, setUsername] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const attempt = useRef<HubClient>(undefined);
  useEffect(() => {
    const cancel = () => attempt.current?.close();
    window.addEventListener("pagehide", cancel);
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, []);

  async function submit(send_code: boolean) {
    if (attempt.current) return;
    const client = browserAccountClient();
    attempt.current = client;
    setBusy(true);
    setError(undefined);
    try {
      if (send_code) {
        await requestAccountCode(client, email.trim());
        if (client.signal.aborted) return;
        setCode("");
        setStep("code");
      } else {
        const result = await verifyAccountCode(client, email.trim(), code.trim(), step === "username" ? username.trim() : undefined);
        if (client.signal.aborted) { result?.client.close(); return; }
        if (result) {
          setCode("");
          on_signed_in(result);
        } else setStep("username");
      }
    } catch (error) {
      if (!client.signal.aborted) setError(hubErrorMessage(error));
    } finally {
      if (!client.signal.aborted) setBusy(false);
      attempt.current = undefined;
      client.close();
    }
  }

  function changeEmail() {
    setStep("email");
    setUsername("");
    setCode("");
    setError(undefined);
  }

  return <form onSubmit={(event) => { event.preventDefault(); void submit(step === "email"); }}>
    <p className="hub-eyebrow">Tokn Hub</p>
    <h1>Hub account</h1>
    {user_code && <p>Sign in to approve the CLI request for <strong>{user_code}</strong>.</p>}
    {notice && <p role="status">{notice}</p>}
    {step === "email" ? <>
      <p>Sign in or create an account with a code sent to your email.</p>
      <label htmlFor="account-email">Email address</label>
      <input id="account-email" type="email" autoComplete="email" required maxLength={254} value={email} disabled={busy} onChange={(event) => setEmail(event.target.value)} />
    </> : <>
      <p>{step === "code" ? <>Enter the code sent to <strong>{email}</strong>.</> : "Choose the username that will prefix your machine addresses."}</p>
      {step === "code" ? <>
        <label htmlFor="account-email-code">Email code</label>
        <input id="account-email-code" inputMode="numeric" autoComplete="one-time-code" required pattern="[0-9]{6}"
          maxLength={6} value={code} disabled={busy} onChange={(event) => setCode(event.target.value)} />
      </> : <>
        <label htmlFor="account-username">Username</label>
        <input id="account-username" autoComplete="username" spellCheck={false} required maxLength={63}
          pattern="[a-z0-9](?:[a-z0-9\-]{0,61}[a-z0-9])?" value={username} disabled={busy} onChange={(event) => setUsername(event.target.value)} />
        <p className="machine-hint">Use lowercase letters, numbers, and internal hyphens.</p>
      </>}
    </>}
    {error && <p role="alert">{error}</p>}
    <button disabled={busy}>{busy ? "Please wait…" : step === "email" ? "Send email code" : step === "code" ? "Verify email code" : "Create account"}</button>
    {step !== "email" && <div className="hub-actions">
      <button className="account-secondary" type="button" disabled={busy} onClick={changeEmail}>Change email</button>
      <button className="account-secondary" type="button" disabled={busy} onClick={() => { void submit(true); }}>Send another code</button>
    </div>}
    <a href="/">Open your machines</a>
  </form>;
}

type AccountFailure = (error: unknown, report: (message: string) => void) => void;

function AccountDeviceApproval({ client, initial_code, approved, on_approved, on_reset, on_failure }: {
  client: HubClient;
  initial_code: string;
  approved: boolean;
  on_approved: () => void;
  on_reset: () => void;
  on_failure: AccountFailure;
}) {
  const [code, setCode] = useState(initial_code);
  const [open, setOpen] = useState(initial_code !== "" || approved);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  async function approve() {
    setBusy(true);
    setError(undefined);
    try {
      await approveAccountDevice(client, code.trim());
      if (client.signal.aborted) return;
      setCode("");
      on_approved();
    } catch (error) { on_failure(error, setError); }
    finally { if (!client.signal.aborted) setBusy(false); }
  }

  return <details className="account-section" open={open} onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>Approve a CLI login</summary>
    {approved ? <div className="account-form">
      <p role="status">CLI login approved. Return to your terminal.</p>
      <button type="button" onClick={on_reset}>Approve another login</button>
    </div> : <form className="account-form"
      onSubmit={(event) => { event.preventDefault(); void approve(); }}>
      <p>Check that this code matches the one shown in your terminal.</p>
      <label htmlFor="account-device-code">CLI device code</label>
      <input id="account-device-code" autoComplete="off" spellCheck={false} required maxLength={32} value={code}
        onChange={(event) => setCode(event.target.value)} disabled={busy} />
      {error && <p className="hub-error" role="alert">{error}</p>}
      <button type="submit" disabled={busy || !code.trim()}>{busy ? "Approving…" : "Approve CLI login"}</button>
    </form>}
  </details>;
}

function AccountMachines({ session, on_select, on_failure }: {
  session: AccountSession;
  on_select: (machine: ResolvedMachine) => void;
  on_failure: AccountFailure;
}) {
  const [machines, setMachines] = useState<ResolvedMachine[]>();
  const [error, setError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setError(undefined);
    void loadAccountMachines(session.client, session.username).then((next) => {
      if (current && !session.client.signal.aborted) setMachines(next);
    }).catch((error: unknown) => { if (current) on_failure(error, setError); });
    return () => { current = false; };
  // The account session owns the request and re-renders do not restart it.
  }, [session, attempt]);

  return <section className="account-section" aria-labelledby="account-machines-title">
    <div className="account-section-header">
      <h2 id="account-machines-title">Your machines</h2>
      <button type="button" disabled={machines === undefined && !error} onClick={() => {
        setMachines(undefined); setAttempt((value) => value + 1);
      }}>Refresh machines</button>
    </div>
    <p>Pair with a machine’s authenticator code. Open paired machines with their passkeys.</p>
    {error ? <p className="hub-error" role="alert">{error}</p> : !machines ? <p role="status">Loading machines…</p>
      : machines.length === 0 ? <p>No machines registered to this account.</p>
      : <ul className="hub-hosts" aria-label="Account machines">{machines.map((machine) => <li className="hub-host" key={machine.host_id}>
        <div className="hub-host-details">
          <h3>{machine.machine_address}</h3>{machine.name !== machine.machine_address && <p>{machine.name}</p>}
          <p className={machine.online ? "hub-presence is-online" : "hub-presence"}>{machine.online ? "Online" : "Offline"}</p>
        </div>
        <button type="button" disabled={!machine.online} onClick={() => on_select(machine)}>Connect to {machine.machine_address}</button>
      </li>)}</ul>}
  </section>;
}

const SCOPE_LABELS = { [MACHINE_READ]: "Read machines", [MACHINE_REGISTER]: "Register machines" };

function AccountKeys({ client, on_failure }: { client: HubClient; on_failure: AccountFailure }) {
  const [keys, setKeys] = useState<AccountKey[]>();
  const [name, setName] = useState("");
  const [scopes, setScopes] = useState<AccountKeyScope[]>([MACHINE_READ]);
  const [created, setCreated] = useState<Pick<AccountKey, "key_id"> & { token: string }>();
  const [copy_notice, setCopyNotice] = useState<string>();
  const [revoke_id, setRevokeId] = useState<string>();
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    void loadAccountKeys(client).then((next) => {
      if (current && !client.signal.aborted) setKeys(next);
    }).catch((error: unknown) => { if (current) on_failure(error, setError); });
    return () => { current = false; };
  }, [client, attempt]);

  async function mutate(operation: () => Promise<void>) {
    setBusy(true);
    setError(undefined);
    try {
      await operation();
      if (!client.signal.aborted) setAttempt((value) => value + 1);
    } catch (error) { on_failure(error, setError); }
    finally { if (!client.signal.aborted) setBusy(false); }
  }

  return <section className="account-section" aria-labelledby="account-keys-title">
    <h2 id="account-keys-title">API keys</h2>
    <p>Use a scoped key to read or register your machines from scripts.</p>
    <form className="account-form" onSubmit={(event) => {
      event.preventDefault();
      void mutate(async () => {
        const key = await createAccountKey(client, name.trim(), scopes);
        if (!client.signal.aborted) { setCreated(key); setName(""); setCopyNotice(undefined); }
      });
    }}>
      <label htmlFor="account-key-name">Key name</label>
      <input id="account-key-name" required maxLength={80} value={name} disabled={busy} onChange={(event) => setName(event.target.value)} />
      <fieldset className="hub-auth-method" disabled={busy}>
        <legend>Permissions</legend>
        {([MACHINE_READ, MACHINE_REGISTER] as const).map((scope) => <label key={scope}>
          <input type="checkbox" checked={scopes.includes(scope)} onChange={(event) => {
            setScopes(event.target.checked ? [...scopes, scope] : scopes.filter((selected) => selected !== scope));
          }} />{SCOPE_LABELS[scope]}
        </label>)}
      </fieldset>
      <button type="submit" disabled={busy || !name.trim() || scopes.length === 0}>Create API key</button>
    </form>
    {created && <div className="account-key-value">
      <p>Copy this API key now. It’s shown only once.</p>
      <code>{created.token}</code>
      <div className="hub-actions">
        {navigator.clipboard?.writeText && <button type="button" onClick={() => {
          void navigator.clipboard.writeText(created.token).then(() => setCopyNotice("API key copied."))
            .catch(() => setCopyNotice("Select the key and copy it manually."));
        }}>Copy API key</button>}
        <button type="button" onClick={() => setCreated(undefined)}>Done</button>
      </div>
      {copy_notice && <p role="status">{copy_notice}</p>}
    </div>}
    {error && <p className="hub-error" role="alert">{error}</p>}
    {!keys ? error ? <button type="button" onClick={() => { setError(undefined); setAttempt((value) => value + 1); }}>Retry loading keys</button>
      : <p role="status">Loading keys…</p>
      : keys.length === 0 ? <p>No API keys yet.</p>
      : <ul className="hub-hosts" aria-label="Account API keys">{keys.map((key) => <li className="hub-host" key={key.key_id}>
        <div className="hub-host-details">
          <h3>{key.name}</h3><p>{key.scopes.map((scope) => SCOPE_LABELS[scope]).join(", ")}</p>
          <p>Expires {new Date(key.expires_at * 1000).toLocaleDateString()}</p>
        </div>
        {revoke_id === key.key_id ? <div className="hub-actions">
          <button type="button" disabled={busy} onClick={() => { void mutate(async () => {
            await revokeAccountKey(client, key.key_id);
            if (!client.signal.aborted) {
              setRevokeId(undefined);
              if (created?.key_id === key.key_id) setCreated(undefined);
            }
          }); }}>Confirm revoke</button>
          <button type="button" disabled={busy} onClick={() => setRevokeId(undefined)}>Cancel</button>
        </div> : <button type="button" disabled={busy} onClick={() => setRevokeId(key.key_id)}>Revoke {key.name}</button>}
      </li>)}</ul>}
  </section>;
}
