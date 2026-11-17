import { useEffect, useRef, useState } from "react";
import { HubClient, hubErrorMessage } from "../lib/hub";
import {
  browserAccountClient, requestAccountCode, verifyAccountCode, type AccountSession,
} from "../lib/hubAccount";
import { type BrowserHubAccount } from "../lib/useBrowserHubAccount";
import "./HubAccount.css";
import { AuthLayout } from "./AuthLayout";
import { VerificationCodeInput } from "./VerificationCodeInput";
import { AccountDeviceApproval } from "./AccountDeviceApproval";
import { AccountKeys } from "./AccountKeys";
import { HubPageLink } from "./HubNavigation";
import { HubAccountMenu } from "./HubAccountMenu";

export function HubAccount({ account }: { account: BrowserHubAccount }) {
  return <AccountPage key={account.boundary} account={account} />;
}

function AccountPage({ account }: { account: BrowserHubAccount }) {
  const { availability, check_error, session, notice, signedIn, signing_out, sign_out_error, logout, onFailure } = account;
  const [user_code, setUserCode] = useState(() => {
    const value = new URLSearchParams(window.location.search).get("user_code");
    return value && /^[A-Za-z0-9-]{1,32}$/.test(value) ? value : "";
  });
  const [approved, setApproved] = useState(false);
  if (!session) return <AuthLayout navigation={<HubPageLink href="/"><span aria-hidden="true">←</span> Back to Machines</HubPageLink>}
    footer="Your machines. Your sessions. One place.">
    {availability === "available" && !check_error ? <AccountSignIn user_code={user_code} notice={notice} on_signed_in={signedIn} />
      : <form className="auth-form" onSubmit={(event) => { event.preventDefault(); account.retry(); }}>
        <header className="auth-heading"><p className="auth-eyebrow">Hub account</p><h1>Welcome to Tokn</h1></header>
        {check_error ? <><p className="auth-feedback" role="alert">{check_error}</p><button className="auth-primary">Retry connection</button></>
          : availability === "email_unconfigured" ? <p className="auth-description">Ask your Hub administrator to set up email delivery for account sign-in.</p>
            : availability === "unsupported" ? <p className="auth-description">Account sign-in is unavailable on this Hub.</p>
              : <p className="auth-description" role="status">Connecting…</p>}
      </form>}
  </AuthLayout>;

  return <main className="hub-home hub-account" key={session.user_id}><div className="hub-content">
    <div className="account-navigation">
      <HubPageLink className="account-back" href="/"><span aria-hidden="true">←</span> Back to Machines</HubPageLink>
      <div className="hub-actions">
        <HubAccountMenu username={session.username} signing_out={signing_out} sign_out_error={sign_out_error}
          profile_active on_sign_out={() => { void logout(); }} />
      </div>
    </div>
    <header className="hub-header">
      <div><p className="hub-eyebrow">Tokn Sessions</p><h1>Your account</h1><p>Manage your profile and connected tools.</p></div>
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
    </section>
    <AccountDeviceApproval client={session.client} initial_code={user_code}
      approved={approved} on_approved={() => { setUserCode(""); setApproved(true); }}
      on_reset={() => setApproved(false)} on_failure={onFailure} />
    <AccountKeys client={session.client} key_list={account.key_list} on_failure={onFailure} />
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
