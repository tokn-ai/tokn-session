import { useEffect, useRef, useState, type ReactNode } from "react";
import { hubErrorMessage, type HubClient } from "../lib/hub";
import { browserAccountClient, type AccountSession } from "../lib/hubAccount";
import {
  completeGitHubAccount, loadGitHubConnection, signUpWithGitHub, startGitHubAccount, type AccountDeviceRequest,
} from "../lib/hubGitHubAccount";
import type { AccountFailure } from "./AccountDialog";

type GitHubReturn = { kind: "idle" | "completing" | "linked" | "link_required" }
  | { kind: "error"; message: string } | { kind: "signup"; suggested_username: string };

function readGitHubReturn(): GitHubReturn {
  const markers = new URLSearchParams(window.location.search).getAll("github");
  if (markers.length === 1 && markers[0] === "complete") return { kind: "completing" };
  if (markers.length === 1 && markers[0] === "failed") return { kind: "error", message: "GitHub sign-in didn’t finish. Try again." };
  return { kind: "idle" };
}

function clearReturnMarker() {
  const url = new URL(window.location.href);
  if (!url.searchParams.has("github")) return;
  url.searchParams.delete("github");
  window.history.replaceState(window.history.state, "", url);
}

// Keep one request per callback document, even when a focus refresh changes
// account state or React re-runs mount effects. The cookie identifies the flow.
export function useGitHubAccountReturn({ ready, github_configured, account_id, on_signed_in }: {
  ready: boolean;
  github_configured: boolean;
  account_id?: string;
  on_signed_in: (session: AccountSession) => void;
}) {
  const [result, setResult] = useState(readGitHubReturn);
  const active = useRef<HubClient>(undefined);
  useEffect(() => {
    const cancel = () => active.current?.close();
    window.addEventListener("pagehide", cancel);
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, []);
  useEffect(() => {
    if (result.kind === "error" && new URLSearchParams(window.location.search).get("github") === "failed") clearReturnMarker();
    if (result.kind !== "completing" || !ready) return;
    if (!github_configured) {
      clearReturnMarker();
      setResult({ kind: "error", message: "GitHub sign-in is unavailable on this Hub." });
      return;
    }
    // Defer until the first real mounted effect survives StrictMode cleanup.
    let mounted = true;
    queueMicrotask(() => {
      if (!mounted || active.current) return;
      const client = browserAccountClient(account_id);
      active.current = client;
      void completeGitHubAccount(client).then((reply) => {
        if (client.signal.aborted) {
          if (reply.status === "signed_in") reply.session.client.close();
          return;
        }
        // A reload during username selection can rehydrate the pending signup.
        if (reply.status !== "signup_required") clearReturnMarker();
        switch (reply.status) {
          case "signed_in": setResult({ kind: "idle" }); on_signed_in(reply.session); break;
          case "signup_required": setResult({ kind: "signup", suggested_username: reply.suggested_username }); break;
          default: setResult({ kind: reply.status });
        }
      }).catch((error) => {
        if (!client.signal.aborted) {
          clearReturnMarker();
          setResult({ kind: "error", message: hubErrorMessage(error) });
        }
      }).finally(() => { client.close(); });
    });
    return () => { mounted = false; };
  }, [account_id, github_configured, on_signed_in, ready, result.kind]);
  return { result, clear: () => { clearReturnMarker(); setResult({ kind: "idle" }); } };
}

export function GitHubStart({ mode, account_id, device_request, disabled = false, on_failure }: {
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

  async function start() {
    if (active.current || disabled) return;
    const client = browserAccountClient(account_id);
    active.current = client;
    setBusy(true);
    setError(undefined);
    try {
      const authorization_url = await startGitHubAccount(client, mode, device_request);
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

  return <div className="account-github-action">
    <button className={mode === "sign_in" ? "auth-primary" : undefined} type="button" disabled={disabled || busy}
      onClick={() => { void start(); }}>{busy ? "Opening GitHub…" : mode === "link" ? "Connect GitHub" : "Continue with GitHub"}</button>
    {error && <p className={mode === "sign_in" ? "auth-feedback" : "account-error"} role="alert">{error}</p>}
  </div>;
}

export function GitHubConnection({ account_id, linked, disabled, on_failure }: {
  account_id: string; linked: boolean; disabled: boolean; on_failure: AccountFailure;
}) {
  const [connection, setConnection] = useState<string | null>();
  const [error, setError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    const client = browserAccountClient(account_id);
    const cancel = () => client.close();
    window.addEventListener("pagehide", cancel);
    setError(undefined);
    void loadGitHubConnection(client).then((login) => {
      if (!client.signal.aborted) setConnection(login);
    }).catch((error) => { if (!client.signal.aborted) on_failure(error, setError); });
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, [account_id, attempt, on_failure]);
  return <section className="account-section" aria-labelledby="account-github-title">
    <div className="account-section-heading">
      <div className="account-section-copy">
        <h2 id="account-github-title">GitHub</h2>
        <p>{connection ? <>Connected as <strong>{connection}</strong>.</> : "Connect your GitHub account as another way to sign in."}</p>
      </div>
      {connection === null && <GitHubStart mode="link" account_id={account_id} disabled={disabled} on_failure={on_failure} />}
    </div>
    {linked && <p role="status">GitHub connected.</p>}
    {error ? <div className="account-feedback"><p className="account-error" role="alert">{error}</p>
      <button type="button" onClick={() => setAttempt((previous) => previous + 1)}>Retry connection check</button></div>
      : connection === undefined && <p role="status">Checking GitHub connection…</p>}
  </section>;
}

export function GitHubSignup({ suggested_username, children, on_cancel, on_signed_in, on_email }: {
  suggested_username: string;
  children?: ReactNode;
  on_cancel: () => void;
  on_signed_in: (session: AccountSession) => void;
  on_email?: () => void;
}) {
  const [username, setUsername] = useState(suggested_username);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
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
      const session = await signUpWithGitHub(client, username.trim());
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
      <p className="auth-description">Your GitHub account is verified. Choose a username for this Hub and its machine addresses.</p></header>
    {on_email && <p className="auth-feedback">Already have a Hub account? Sign in with an email code first, then choose Connect GitHub in your account.</p>}
    {children}
    <div className="auth-field">
      <label htmlFor="github-account-username">Username</label>
      <input ref={input} id="github-account-username" autoComplete="username" autoCapitalize="none" spellCheck={false} required maxLength={63}
        aria-describedby="github-account-username-hint" value={username} disabled={busy} onChange={(event) => { setUsername(event.target.value); setError(undefined); }} />
      <p className="auth-hint" id="github-account-username-hint">Use lowercase letters, numbers, and internal hyphens.</p>
    </div>
    {error && <p role="alert" className="auth-feedback">{error}</p>}
    <button className="auth-primary" disabled={!valid || busy}>{busy ? "Creating account…" : "Create account"}</button>
    <div className="auth-secondary-actions">
      <button type="button" disabled={busy} onClick={on_cancel}>Back to sign in</button>
      {on_email && <button type="button" disabled={busy} onClick={on_email}>Use an email code</button>}
    </div>
  </form>;
}
