import { useCallback, useEffect, useRef, useState } from "react";
import { HubClient, HubError, hubErrorMessage } from "../lib/hub";
import {
  accountSession, browserAccountClient, hubAccountAvailability, logoutAccount,
  refreshBrowserAccount, requestAccountCode, verifyAccountCode,
  type AccountSession, type HubAccountAvailability,
} from "../lib/hubAccount";
import "./HubAccount.css";
import { AuthLayout } from "./AuthLayout";
import { VerificationCodeInput } from "./VerificationCodeInput";
import { AccountDeviceApproval } from "./AccountDeviceApproval";
import { AccountKeys } from "./AccountKeys";

const ACCOUNT_CHANGED = "Your Hub account changed in another tab. Review the active account before continuing.";
const SESSION_EXPIRED = "Your Hub session expired. Sign in again.";

export function HubAccount() {
  const [availability, setAvailability] = useState<HubAccountAvailability>();
  const [check_error, setCheckError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  const [session, setSession] = useState<AccountSession>();
  const [signing_out, setSigningOut] = useState(false);
  const [sign_out_error, setSignOutError] = useState<string>();
  const [notice, setNotice] = useState<string>();
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
    const hide = () => { lock(); setSession(undefined); };
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

  if (!session) return <AuthLayout navigation={<a href="/">Open your machines <span aria-hidden="true">↗</span></a>}
    footer="Your machines. Your sessions. One place.">
    {availability === "available" ? <AccountSignIn user_code={user_code} notice={notice} on_signed_in={signedIn} />
      : <form className="auth-form" onSubmit={(event) => { event.preventDefault(); setAttempt((value) => value + 1); }}>
        <header className="auth-heading"><p className="auth-eyebrow">Hub account</p><h1>Welcome to Tokn</h1></header>
        {check_error ? <><p className="auth-feedback" role="alert">{check_error}</p><button className="auth-primary">Retry connection</button></>
          : availability === "email_unconfigured" ? <p className="auth-description">Ask your Hub administrator to set up email delivery for account sign-in.</p>
            : availability === "unsupported" ? <p className="auth-description">Account sign-in is unavailable on this Hub.</p>
              : <p className="auth-description" role="status">Connecting…</p>}
      </form>}
  </AuthLayout>;

  return <main className="hub-home hub-account" key={session.user_id}><div className="hub-content">
    <header className="hub-header">
      <div><p className="hub-eyebrow">Tokn Hub</p><h1>Hub account</h1><p>Manage your account and connected tools.</p></div>
      <div className="hub-actions"><a href="/">Open your machines</a>
        <button type="button" onClick={() => { void logout(); }} disabled={signing_out}>{signing_out ? "Signing out…" : "Sign out"}</button>
      </div>
    </header>
    <section className="account-section" aria-labelledby="account-profile-title">
      <h2 id="account-profile-title">Profile</h2>
      <div className="account-profile-summary">
        <span className="account-avatar" aria-hidden="true">{session.username.slice(0, 1).toUpperCase()}</span>
        <div><strong className="account-username">{session.username}</strong><p>Your Hub account</p></div>
      </div>
      <details className="account-profile-details"><summary>Account details</summary>
        <dl><dt>Account ID</dt><dd><code>{session.user_id}</code></dd></dl>
      </details>
      {notice && <p role="status">{notice}</p>}
      {sign_out_error && <p className="hub-error" role="alert">{sign_out_error}</p>}
    </section>
    <AccountDeviceApproval client={session.client} initial_code={user_code}
      approved={approved} on_approved={() => { setUserCode(""); setApproved(true); }}
      on_reset={() => setApproved(false)} on_failure={onFailure} />
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
  const [code, setCode] = useState<string[]>([]);
  const [username, setUsername] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [sent_notice, setSentNotice] = useState<string>();
  const input = useRef<HTMLInputElement>(null);
  const attempt = useRef<HubClient>(undefined);
  useEffect(() => { input.current?.focus(); }, [step]);
  useEffect(() => {
    const cancel = () => attempt.current?.close();
    window.addEventListener("pagehide", cancel);
    return () => { window.removeEventListener("pagehide", cancel); cancel(); };
  }, []);

  async function submit(send_code: boolean) {
    if (attempt.current) return;
    if (!send_code && !/^[0-9]{6}$/.test(code.join(""))) return;
    const client = browserAccountClient();
    attempt.current = client;
    setBusy(true);
    setError(undefined);
    setSentNotice(undefined);
    try {
      if (send_code) {
        await requestAccountCode(client, email.trim());
        if (client.signal.aborted) return;
        setCode([]);
        if (step !== "email") setSentNotice("A new code has been sent. Check your inbox.");
        setStep("code");
      } else {
        const result = await verifyAccountCode(client, email.trim(), code.join(""), step === "username" ? username.trim() : undefined);
        if (client.signal.aborted) { result?.client.close(); return; }
        if (result) {
          setCode([]);
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
    setCode([]);
    setError(undefined);
    setSentNotice(undefined);
  }

  return <form className="auth-form" onSubmit={(event) => { event.preventDefault(); void submit(step === "email"); }}>
    <header className="auth-heading">
      <p className="auth-eyebrow">Hub account</p>
      <h1>{step === "email" ? "Welcome to Tokn" : step === "code" ? "Check your inbox" : "Make it yours"}</h1>
      <p className="auth-description" id="account-description">{step === "email"
        ? "Sign in or create an account to bring your machines together."
        : step === "code" ? <>Enter the 6-digit code sent to <strong>{email.trim()}</strong>.</>
          : "Choose a username for your account and machine addresses."}</p>
    </header>
    {user_code && <p className="auth-feedback">Sign in to approve the CLI request for <strong>{user_code}</strong>.</p>}
    {notice && <p className="auth-feedback" role="status">{notice}</p>}
    {step === "email" ? <div className="auth-field">
      <label htmlFor="account-email">Email address</label>
      <input ref={input} key="email" id="account-email" type="email" autoComplete="email" autoCapitalize="none" spellCheck={false}
        placeholder="you@example.com" aria-describedby="account-email-hint" required maxLength={254} value={email} disabled={busy} onChange={(event) => setEmail(event.target.value)} />
      <p className="auth-hint" id="account-email-hint">We’ll email you a 6-digit sign-in code.</p>
    </div> : step === "code" ? <VerificationCodeInput kind="email" value={code} disabled={busy} input_ref={input} on_change={setCode} /> : <div className="auth-field">
      <label htmlFor="account-username">Username</label>
      <input ref={input} key="username" id="account-username" autoComplete="username" autoCapitalize="none" spellCheck={false} required maxLength={63}
        aria-describedby="account-username-hint" pattern="[a-z0-9](?:[a-z0-9\-]{0,61}[a-z0-9])?" value={username} disabled={busy} onChange={(event) => setUsername(event.target.value)} />
      <p className="auth-hint" id="account-username-hint">Use lowercase letters, numbers, and internal hyphens.</p>
    </div>}
    {sent_notice && <p className="auth-feedback" role="status">{sent_notice}</p>}
    {error && <p className="auth-feedback" role="alert">{error}</p>}
    <button className="auth-primary" disabled={busy}>
      {busy ? "Please wait…" : step === "email" ? "Send email code" : step === "code" ? "Verify email code" : "Create account"}
      {!busy && <span aria-hidden="true">→</span>}
    </button>
    {step !== "email" && <div className="auth-secondary-actions">
      <button type="button" disabled={busy} onClick={changeEmail}>Change email</button>
      <button type="button" disabled={busy} onClick={() => { void submit(true); }}>Send another code</button>
    </div>}
    {step === "email" && <p className="auth-note">No password to remember. Just your email.</p>}
  </form>;
}
