import { useEffect, useRef, useState, type ReactNode } from "react";
import { hubErrorMessage, type HubClient } from "../lib/hub";
import { browserAccountClient, type AccountSession } from "../lib/hubAccount";
import {
  ACCOUNT_PROVIDER_LABELS, completeFederatedAccount, confirmApellAccountLink, loadProviderConnection, signUpWithProvider,
  startFederatedAccount, type AccountDeviceRequest, type AccountProvider, type AccountProviderId,
  type ApellLinkConfirmation,
} from "../lib/hubAccountFederation";
import type { AccountFailure } from "./AccountDialog";

type AccountReturn = { kind: "idle" } | ({ provider: AccountProviderId } & (
  { kind: "completing" | "linked" | "link_required" }
  | { kind: "error"; message: string }
  | { kind: "signup"; suggested_username: string }
)) | { kind: "confirm_apell_link"; provider: "apell"; target: ApellLinkConfirmation };

function readAccountReturn(): AccountReturn {
  const params = new URLSearchParams(window.location.search);
  const results: AccountReturn[] = [];
  for (const provider of ["apell", "github"] as const) {
    const markers = params.getAll(provider);
    if (markers.length !== 1) continue;
    if (markers[0] === "complete") results.push({ kind: "completing", provider });
    if (markers[0] === "failed") results.push({ kind: "error", provider, message: `${ACCOUNT_PROVIDER_LABELS[provider]} sign-in didn’t finish. Try again.` });
  }
  return results.length === 1 ? results[0] : { kind: "idle" };
}

function clearReturnMarker(provider: AccountProviderId) {
  const url = new URL(window.location.href);
  if (!url.searchParams.has(provider)) return;
  url.searchParams.delete(provider);
  window.history.replaceState(window.history.state, "", url);
}

// Keep one request per callback document, even when a focus refresh changes
// account state or React re-runs mount effects. The cookie identifies the flow.
export function useFederatedAccountReturn({ ready, apell_configured, github_configured, account_id, on_signed_in }: {
  ready: boolean;
  apell_configured: boolean;
  github_configured: boolean;
  account_id?: string;
  on_signed_in: (session: AccountSession) => void;
}) {
  const [result, setResult] = useState(readAccountReturn);
  const provider = result.kind === "idle" ? undefined : result.provider;
  const configured = provider === "apell" ? apell_configured : github_configured;
  const active = useRef<HubClient>(undefined);
  useEffect(() => {
    const cancel = () => active.current?.close();
    window.addEventListener("pagehide", cancel);
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, []);
  useEffect(() => {
    if (!provider) return;
    if (result.kind === "error" && new URLSearchParams(window.location.search).get(provider) === "failed") clearReturnMarker(provider);
    if (result.kind !== "completing" || !ready) return;
    if (!configured) {
      clearReturnMarker(provider);
      setResult({ kind: "error", provider, message: `${ACCOUNT_PROVIDER_LABELS[provider]} sign-in is unavailable on this Hub.` });
      return;
    }
    // Defer until the first real mounted effect survives StrictMode cleanup.
    let mounted = true;
    queueMicrotask(() => {
      if (!mounted || active.current) return;
      const client = browserAccountClient(account_id);
      active.current = client;
      void completeFederatedAccount(client, provider).then((reply) => {
        if (client.signal.aborted) {
          if (reply.status === "signed_in") reply.session.client.close();
          return;
        }
        // Keep the callback marker while a pending decision can be resumed on reload.
        const target = reply.status === "link_required" ? reply.apell_link : undefined;
        if (reply.status !== "signup_required" && !target) clearReturnMarker(provider);
        switch (reply.status) {
          case "signed_in": setResult({ kind: "idle" }); on_signed_in(reply.session); break;
          case "signup_required": setResult({ kind: "signup", provider, suggested_username: reply.suggested_username }); break;
          case "link_required": setResult(target
            ? { kind: "confirm_apell_link", provider: "apell", target }
            : { kind: "link_required", provider }); break;
          default: setResult({ kind: reply.status, provider });
        }
      }).catch((error) => {
        if (!client.signal.aborted) {
          clearReturnMarker(provider);
          setResult({ kind: "error", provider, message: hubErrorMessage(error) });
        }
      }).finally(() => { client.close(); });
    });
    return () => { mounted = false; };
  }, [account_id, configured, on_signed_in, provider, ready, result.kind]);
  return { result, clear: () => { if (provider) clearReturnMarker(provider); setResult({ kind: "idle" }); } };
}

export function FederatedStart({ provider, mode, account_id, device_request, primary = false, disabled = false, on_failure }: {
  provider: AccountProvider;
  primary?: boolean;
  mode: "sign_in" | "link";
  account_id?: string;
  device_request?: AccountDeviceRequest;
  disabled?: boolean;
  on_failure?: AccountFailure;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const active = useRef<HubClient>(undefined);
  useEffect(() => {
    const cancel = () => active.current?.close();
    window.addEventListener("pagehide", cancel);
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, []);

  const label = ACCOUNT_PROVIDER_LABELS[provider.id];

  async function start() {
    if (active.current || disabled) return;
    const client = browserAccountClient(account_id);
    active.current = client;
    setBusy(true);
    setError(undefined);
    try {
      const authorization_url = await startFederatedAccount(client, provider, mode, device_request);
      if (!client.signal.aborted) window.location.assign(authorization_url);
    } catch (error) {
      if (!client.signal.aborted) {
        if (on_failure) on_failure(error, setError);
        else setError(hubErrorMessage(error));
        setBusy(false);
      }
      active.current = undefined;
    } finally { client.close(); }
  }

  return <div className="account-provider-action">
    <button className={mode === "sign_in" ? (primary ? "auth-primary" : "account-provider-secondary") : undefined} type="button" disabled={disabled || busy}
      onClick={() => { void start(); }}>{busy ? `Opening ${label}…` : mode === "link" ? `Connect ${label}` : `Continue with ${label}`}</button>
    {error && <p className={mode === "sign_in" ? "auth-feedback" : "account-error"} role="alert">{error}</p>}
  </div>;
}

export function FederatedConnection({ provider, account_id, linked, disabled, on_failure }: {
  provider: AccountProvider;
  account_id: string; linked: boolean; disabled: boolean; on_failure: AccountFailure;
}) {
  const [connection, setConnection] = useState<{ login: string | null } | null>();
  const provider_id = provider.id;
  const label = ACCOUNT_PROVIDER_LABELS[provider_id];
  const title_id = `account-${provider_id}-title`;
  const [error, setError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    const client = browserAccountClient(account_id);
    const cancel = () => client.close();
    window.addEventListener("pagehide", cancel);
    setError(undefined);
    void loadProviderConnection(client, provider_id).then((value) => {
      if (!client.signal.aborted) setConnection(value);
    }).catch((error) => { if (!client.signal.aborted) on_failure(error, setError); });
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, [account_id, attempt, linked, on_failure, provider_id]);
  return <section className="account-section" aria-labelledby={title_id}>
    <div className="account-section-heading">
      <div className="account-section-copy">
        <h2 id={title_id}>{label}</h2>
        <p className="account-provider-connection">{connection ? (connection.login ? <>Connected as <strong>{connection.login}</strong>.</> : "Connected for sign-in.")
          : `Connect ${label} as another way to sign in.`}</p>
      </div>
      {connection === null && <FederatedStart provider={provider} mode="link" account_id={account_id} disabled={disabled} on_failure={on_failure} />}
    </div>
    {linked && <p role="status">{label} connected.</p>}
    {error ? <div className="account-feedback"><p className="account-error" role="alert">{error}</p>
      <button type="button" onClick={() => setAttempt((previous) => previous + 1)}>Retry connection check</button></div>
      : connection === undefined && <p role="status">Checking {label} connection…</p>}
  </section>;
}

export function ApellLinkPrompt({ target, children, on_cancel, on_email, on_signed_in }: {
  target: ApellLinkConfirmation;
  children?: ReactNode;
  on_cancel: () => void;
  on_email?: () => void;
  on_signed_in: (session: AccountSession) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const heading = useRef<HTMLHeadingElement>(null);
  const active = useRef<HubClient>(undefined);
  useEffect(() => {
    heading.current?.focus();
    const cancel = () => active.current?.close();
    window.addEventListener("pagehide", cancel);
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, []);

  async function confirm() {
    if (active.current) return;
    const client = browserAccountClient();
    active.current = client;
    setBusy(true);
    setError(undefined);
    try {
      const session = await confirmApellAccountLink(client, target.confirmation);
      if (client.signal.aborted) session.client.close();
      else on_signed_in(session);
    } catch (error) { if (!client.signal.aborted) setError(hubErrorMessage(error)); }
    finally {
      if (!client.signal.aborted) setBusy(false);
      active.current = undefined;
      client.close();
    }
  }

  return <div className="auth-form">
    <header className="auth-heading">
      <p className="auth-eyebrow">Hub account</p>
      <h1 ref={heading} tabIndex={-1}>Connect to your Hub account?</h1>
      <p className="auth-description">Your verified Apell email matches the Hub account <strong>{target.username}</strong>. Connect your accounts to sign in and keep your machines.</p>
    </header>
    {children}
    {error && <p className="auth-feedback" role="alert">{error}</p>}
    <button className="auth-primary" type="button" disabled={busy} onClick={() => { void confirm(); }}>
      {busy ? "Connecting…" : "Connect Apell Account"}
    </button>
    <div className="auth-secondary-actions">
      <button type="button" disabled={busy} onClick={on_cancel}>Back to sign in</button>
      {on_email && <button type="button" disabled={busy} onClick={on_email}>Use an email code</button>}
    </div>
  </div>;
}

export function FederatedSignup({ provider, suggested_username, children, on_cancel, on_signed_in, on_email }: {
  provider: AccountProviderId;
  suggested_username: string;
  children?: ReactNode;
  on_cancel: () => void;
  on_signed_in: (session: AccountSession) => void;
  on_email?: () => void;
}) {
  const [username, setUsername] = useState(suggested_username);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const label = ACCOUNT_PROVIDER_LABELS[provider];
  const input = useRef<HTMLInputElement>(null);
  const active = useRef<HubClient>(undefined);
  const valid = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(username.trim());
  useEffect(() => {
    input.current?.focus();
    const cancel = () => active.current?.close();
    window.addEventListener("pagehide", cancel);
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, []);
  async function signup() {
    if (!valid || active.current) return;
    const client = browserAccountClient();
    active.current = client;
    setBusy(true);
    setError(undefined);
    try {
      const session = await signUpWithProvider(client, provider, username.trim());
      if (client.signal.aborted) session.client.close();
      else on_signed_in(session);
    } catch (error) { if (!client.signal.aborted) setError(hubErrorMessage(error)); }
    finally {
      if (!client.signal.aborted) setBusy(false);
      active.current = undefined;
      client.close();
    }
  }
  return <form className="auth-form" onSubmit={(event) => { event.preventDefault(); void signup(); }}>
    <header className="auth-heading"><p className="auth-eyebrow">Hub account</p><h1>Choose your Hub username</h1>
      <p className="auth-description">Choose a username for this Hub and its machine addresses.</p></header>
    <p className="auth-feedback">Already have a Hub account? Sign in using your current method, then choose Connect {label} in your Hub account.</p>
    {children}
    <div className="auth-field">
      <label htmlFor="provider-account-username">Username</label>
      <input ref={input} id="provider-account-username" autoComplete="username" autoCapitalize="none" spellCheck={false} required maxLength={63}
        aria-describedby="provider-account-username-hint" value={username} disabled={busy} onChange={(event) => { setUsername(event.target.value); setError(undefined); }} />
      <p className="auth-hint" id="provider-account-username-hint">Use lowercase letters, numbers, and internal hyphens.</p>
    </div>
    {error && <p role="alert" className="auth-feedback">{error}</p>}
    <button className="auth-primary" disabled={!valid || busy}>{busy ? "Creating account…" : "Create account"}</button>
    <div className="auth-secondary-actions">
      <button type="button" disabled={busy} onClick={on_cancel}>Back to sign in</button>
      {on_email && <button type="button" disabled={busy} onClick={on_email}>Use an email code</button>}
    </div>
  </form>;
}
