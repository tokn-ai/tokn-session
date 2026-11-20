import { useEffect, useRef, useState, type RefObject } from "react";
import type { AccountIdentity } from "../lib/hubAccount";
import { AccountDialog } from "./AccountDialog";
import { HubAccountEntry } from "./HubAccountEntry";
import { HubAccountProfile } from "./HubAccountProfile";

interface SignInState {
  challenge?: { verification_url: string; user_code: string };
  starting?: boolean;
  sign_in_error?: string;
  on_start_sign_in: () => void;
  on_open_approval?: () => Promise<void>;
}

interface NativeHubAccountEntryProps extends SignInState {
  session?: AccountIdentity;
  signing_out?: boolean;
  sign_out_error?: string;
  on_sign_out: () => void;
  on_cancel_sign_in: () => void;
}

interface AccountDialogState {
  kind: "sign_in" | "profile";
  return_focus: RefObject<HTMLElement | null>;
}

export function NativeHubAccountEntry({
  session, signing_out, sign_out_error, on_sign_out,
  challenge, starting, sign_in_error, on_start_sign_in, on_cancel_sign_in, on_open_approval,
}: NativeHubAccountEntryProps) {
  const [dialog, setDialog] = useState<AccountDialogState>();
  const pending_sign_in = useRef(false);
  const cancel_sign_in = useRef(on_cancel_sign_in);

  useEffect(() => { cancel_sign_in.current = on_cancel_sign_in; }, [on_cancel_sign_in]);
  useEffect(() => () => cancelPendingSignIn(), []);
  useEffect(() => {
    if (session && dialog?.kind === "sign_in") {
      pending_sign_in.current = false;
      setDialog(undefined);
    }
    if (!session && dialog?.kind === "profile") setDialog(undefined);
  }, [session, dialog]);

  function cancelPendingSignIn() {
    if (!pending_sign_in.current) return;
    pending_sign_in.current = false;
    cancel_sign_in.current();
  }

  return <>
    <HubAccountEntry session={session} signing_out={signing_out} sign_out_error={sign_out_error} on_sign_out={on_sign_out}
      on_sign_in={(return_focus) => {
        pending_sign_in.current = true;
        setDialog({ kind: "sign_in", return_focus });
        on_start_sign_in();
      }}
      on_profile={(return_focus) => setDialog({ kind: "profile", return_focus })} />
    {dialog?.kind === "sign_in" && !session && <NativeHubSignIn challenge={challenge} starting={starting}
      sign_in_error={sign_in_error} on_start_sign_in={on_start_sign_in} return_focus={dialog.return_focus}
      on_open_approval={on_open_approval}
      on_close={() => { setDialog(undefined); cancelPendingSignIn(); }} />}
    {dialog?.kind === "profile" && session && <AccountDialog title="Profile" return_focus={dialog.return_focus}
      on_close={() => setDialog(undefined)}><HubAccountProfile {...session} /></AccountDialog>}
  </>;
}

export function NativeHubSignIn({
  challenge, starting, sign_in_error, on_start_sign_in, on_close, return_focus, on_open_approval,
}: SignInState & { on_close: () => void; return_focus: RefObject<HTMLElement | null> }) {
  const [copy_notice, setCopyNotice] = useState<{ url: string; message: string }>();
  const [open_error, setOpenError] = useState<string>();
  const address = challenge?.verification_url;
  const clipboard = navigator.clipboard;
  const can_copy = typeof clipboard?.writeText === "function";

  async function copyAddress() {
    if (!address || !can_copy) return;
    try {
      await clipboard.writeText(address);
      setCopyNotice({ url: address, message: "Approval address copied." });
    } catch {
      setCopyNotice({ url: address, message: "Select the approval address to copy it manually." });
    }
  }

  async function openApproval() {
    setOpenError(undefined);
    try { await on_open_approval?.(); }
    catch { setOpenError("Could not open your browser. Copy the approval address to open it manually."); }
  }

  return <AccountDialog title="Sign in to Hub" description="Sign in to see the machines registered to your account."
    return_focus={return_focus} on_close={on_close}>
    {challenge ? <div className="account-key-value">
      <p>Open the approval address in your browser. Sign in and approve the request if this code matches.</p>
      <div><p>Device code</p><code>{challenge.user_code}</code></div>
      <div><p>Approval address</p><code>{challenge.verification_url}</code></div>
      {on_open_approval && <button type="button" onClick={() => { void openApproval(); }}>Open in browser</button>}
      {can_copy && <button type="button" onClick={() => { void copyAddress(); }}>Copy approval address</button>}
      {open_error && <p className="account-error" role="alert">{open_error}</p>}
      {copy_notice && copy_notice.url === address && <p role="status">{copy_notice.message}</p>}
      {!sign_in_error && <p role="status">Waiting for browser approval…</p>}
    </div> : starting && <p role="status">Requesting an approval code…</p>}
    {sign_in_error && <p className="account-error" role="alert">{sign_in_error}</p>}
    <div className="account-dialog-actions">
      <button type="button" onClick={on_close}>Cancel</button>
      {sign_in_error && <button type="button" className="account-primary" disabled={starting}
        onClick={on_start_sign_in}>Request a new code</button>}
    </div>
  </AccountDialog>;
}
