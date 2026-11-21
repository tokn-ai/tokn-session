import { useEffect, useRef, useState, type ReactNode } from "react";
import { HubClient, hubErrorMessage } from "../lib/hub";
import {
  browserAccountClient, requestAccountCode, verifyAccountCode, type AccountSession,
} from "../lib/hubAccount";
import { ACCOUNT_PROVIDER_LABELS, type AccountDeviceRequest, type AccountProvider } from "../lib/hubAccountFederation";
import { type BrowserHubAccount } from "../lib/useBrowserHubAccount";
import "./HubAccount.css";
import { AuthLayout } from "./AuthLayout";
import { VerificationCodeInput } from "./VerificationCodeInput";
import { AccountDeviceApproval, DeviceApprovalDialog } from "./AccountDeviceApproval";
import { AccountKeys } from "./AccountKeys";
import { ApellLinkPrompt, FederatedConnection, FederatedSignup, FederatedStart, useFederatedAccountReturn } from "./AccountFederation";
import { HubPageLink } from "./HubNavigation";
import { HubAccountMenu } from "./HubAccountMenu";
import { HubAccountProfile } from "./HubAccountProfile";

export function HubAccount({ account }: { account: BrowserHubAccount }) {
  return <AccountPage key={account.boundary} account={account} />;
}

function readDeviceRequest(): AccountDeviceRequest | undefined {
  const params = new URLSearchParams(window.location.search);
  const code = params.get("user_code");
  if (!code || !/^[A-Za-z0-9-]{1,32}$/.test(code)) return;
  if (params.get("device") !== "viewer") return { user_code: code, purpose: "cli" };
  const normalized = code.toUpperCase();
  if (/^[A-Z0-9]{4}-[A-Z0-9]{4}$/.test(normalized)) return { user_code: normalized, purpose: "viewer" };
}

function AccountPage({ account }: { account: BrowserHubAccount }) {
  const { availability, check_error, session, notice, signedIn, signing_out, sign_out_error, logout, onFailure } = account;
  const capabilities = availability === "unsupported" ? undefined : availability;
  const apell_issuer = capabilities?.apell_issuer;
  const github_configured = capabilities?.github_configured === true;
  const email_configured = capabilities?.email_configured === true;
  const providers: AccountProvider[] = [];
  if (apell_issuer) providers.push({ id: "apell", issuer: apell_issuer });
  if (github_configured) providers.push({ id: "github" });
  const federation = useFederatedAccountReturn({ ready: availability !== undefined && !check_error,
    apell_configured: !!apell_issuer, github_configured,
    account_id: session?.user_id, on_signed_in: signedIn });
  const { result } = federation;
  const [method, setMethod] = useState<"provider" | "email">();
  const [device_request, setDeviceRequest] = useState(readDeviceRequest);
  const [cli_approved, setCliApproved] = useState(false);
  const [viewer_approved, setViewerApproved] = useState(false);
  const account_back = useRef<HTMLAnchorElement>(null);
  const use_email = email_configured && (!providers.length || method === "email" || result.kind === "link_required");
  const provider_label = result.kind !== "idle" ? ACCOUNT_PROVIDER_LABELS[result.provider] : undefined;
  const provider_feedback = result.kind === "error" ? result.message
    : result.kind === "link_required" ? (email_configured
      ? `This email already belongs to a Hub account. Sign in with an email code, then choose Connect ${provider_label} in your account.`
      : providers.length > 1
        ? `This email already belongs to a Hub account. Sign in using your existing method, then choose Connect ${provider_label} in your account.`
        : `This email already belongs to a Hub account. Ask your Hub administrator to restore email sign-in so you can connect ${provider_label}.`) : undefined;
  if (!session || result.kind === "completing") {
    const device_notice = <DeviceSignInNotice request={device_request} />;
    let content: ReactNode;
    if (check_error || result.kind === "completing" || (!providers.length && !email_configured)) {
      content = <form className="auth-form" onSubmit={(event) => { event.preventDefault(); account.retry(); }}>
        <header className="auth-heading"><p className="auth-eyebrow">Hub account</p><h1>Welcome to Tokn</h1></header>
        {check_error ? <><p className="auth-feedback" role="alert">{check_error}</p><button className="auth-primary">Retry connection</button></>
          : result.kind === "completing" ? <p className="auth-description" role="status">Finishing {provider_label} sign-in…</p>
            : capabilities ? <p className="auth-description">Ask your Hub administrator to set up email delivery for account sign-in.</p>
              : availability === "unsupported" ? <p className="auth-description">Account sign-in is unavailable on this Hub.</p>
                : <p className="auth-description" role="status">Connecting…</p>}
        {provider_feedback && <p className="auth-feedback" role="alert">{provider_feedback}</p>}
      </form>;
    } else if (result.kind === "signup") {
      content = <FederatedSignup provider={result.provider} suggested_username={result.suggested_username} on_cancel={federation.clear}
        on_signed_in={(next) => { federation.clear(); signedIn(next); }}
        on_email={email_configured ? () => { federation.clear(); setMethod("email"); } : undefined}>{device_notice}</FederatedSignup>;
    } else if (result.kind === "confirm_apell_link") {
      content = <ApellLinkPrompt target={result.target} on_cancel={federation.clear}
        on_signed_in={(next) => { federation.clear(); signedIn(next); }}
        on_email={email_configured ? () => { federation.clear(); setMethod("email"); } : undefined}>{device_notice}</ApellLinkPrompt>;
    } else if (use_email) {
      content = <AccountSignIn device_notice={device_notice} notice={notice} provider_feedback={provider_feedback}
        on_signed_in={(next) => { federation.clear(); signedIn(next); }}
        provider_alternative={providers.length ? {
          label: providers.length === 1 ? `Use ${ACCOUNT_PROVIDER_LABELS[providers[0].id]} instead` : "Other sign-in options",
          select: () => { federation.clear(); setMethod("provider"); },
        } : undefined} />;
    } else {
      content = <div className="auth-form">
        <header className="auth-heading"><p className="auth-eyebrow">Hub account</p><h1>Welcome to Tokn</h1>
          <p className="auth-description">Sign in or create an account to bring your machines together.</p></header>
        {device_notice}
        {notice && <p className="auth-feedback" role="status">{notice}</p>}
        {provider_feedback && <p className="auth-feedback" role="alert">{provider_feedback}</p>}
        {providers.map((provider, index) => <FederatedStart key={provider.id} provider={provider} mode="sign_in"
          primary={index === 0} device_request={device_request} />)}
        {email_configured && <div className="auth-secondary-actions account-sign-in-alternate">
          <button type="button" onClick={() => { federation.clear(); setMethod("email"); }}>Use an email code</button>
        </div>}
      </div>;
    }
    return <AuthLayout navigation={<HubPageLink href="/"><span aria-hidden="true">←</span> Back to Machines</HubPageLink>}
      footer="Your machines. Your sessions. One place.">{content}</AuthLayout>;
  }

  return <main className="hub-home hub-account" key={session.user_id}><div className="hub-content">
    <div className="account-navigation">
      <HubPageLink ref={account_back} className="account-back" href="/"><span aria-hidden="true">←</span> Back to Machines</HubPageLink>
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
      <HubAccountProfile {...session} />
      {notice && <p role="status">{notice}</p>}
      {provider_feedback && <p className="account-error" role="alert">{provider_feedback}</p>}
    </section>
    {providers.map((provider) => <FederatedConnection key={provider.id} provider={provider} account_id={session.user_id} disabled={signing_out}
      linked={result.kind === "linked" && result.provider === provider.id} on_failure={onFailure} />)}
    <AccountDeviceApproval client={session.client} initial_code={device_request?.purpose === "cli" ? device_request.user_code : ""}
      approved={cli_approved} on_approved={() => { setDeviceRequest(undefined); setCliApproved(true); }}
      on_reset={() => setCliApproved(false)} on_failure={onFailure} />
    {device_request?.purpose === "viewer" && <DeviceApprovalDialog client={session.client} purpose="viewer"
      initial_code={device_request.user_code} approved={viewer_approved} on_approved={() => setViewerApproved(true)}
      on_failure={onFailure} on_close={() => setDeviceRequest(undefined)} return_focus={account_back} />}
    <AccountKeys client={session.client} key_list={account.key_list} on_failure={onFailure} />
  </div></main>;
}

function DeviceSignInNotice({ request }: { request?: AccountDeviceRequest }) {
  return request ? <p className="auth-feedback">Sign in to approve the {request.purpose === "viewer" ? "Tokn Sessions sign-in" : "CLI request"} for <strong>{request.user_code}</strong>.</p> : null;
}

function AccountSignIn({ device_notice, notice, provider_feedback, provider_alternative, on_signed_in }: {
  device_notice?: ReactNode;
  notice?: string;
  provider_feedback?: string;
  provider_alternative?: { label: string; select: () => void };
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
    {device_notice}
    {notice && <p className="auth-feedback" role="status">{notice}</p>}
    {provider_feedback && <p className="auth-feedback" role="alert">{provider_feedback}</p>}
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
    {step === "email" && provider_alternative && <div className="auth-secondary-actions account-sign-in-alternate">
      <button type="button" disabled={busy} onClick={provider_alternative.select}>{provider_alternative.label}</button>
    </div>}
    {step === "email" && <p className="auth-note">No password to remember. Just your email.</p>}
  </form>;
}
