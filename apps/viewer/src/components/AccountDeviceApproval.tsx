import { useEffect, useRef, useState, type RefObject } from "react";
import type { HubClient } from "../lib/hub";
import { approveAccountDevice, type AccountDevicePurpose } from "../lib/hubAccount";
import { AccountDialog, type AccountFailure } from "./AccountDialog";
import { TerminalIcon } from "./Icons";
import { VerificationCodeInput } from "./VerificationCodeInput";

export function AccountDeviceApproval({ client, initial_code, approved, on_approved, on_reset, on_failure }: {
  client: HubClient;
  initial_code: string;
  approved: boolean;
  on_approved: () => void;
  on_reset: () => void;
  on_failure: AccountFailure;
}) {
  const [open, setOpen] = useState(initial_code !== "");
  const trigger = useRef<HTMLButtonElement>(null);
  return <section className="account-section account-cli" aria-labelledby="account-cli-title">
    <div className="account-section-heading">
      <div className="account-section-copy account-action-copy">
        <span className="account-section-icon"><TerminalIcon /></span>
        <div><h2 id="account-cli-title">CLI access</h2>
          <p>{approved ? "Your last CLI login was approved." : initial_code ? "A terminal login is waiting for your approval." : "Connect your terminal to this account."}</p>
        </div>
      </div>
      <button ref={trigger} type="button" aria-haspopup="dialog" onClick={() => { on_reset(); setOpen(true); }}>Approve a CLI login</button>
    </div>
    {open && <DeviceApprovalDialog client={client} initial_code={initial_code} approved={approved} on_approved={on_approved}
      on_failure={on_failure} on_close={() => setOpen(false)} return_focus={trigger} />}
  </section>;
}

export function DeviceApprovalDialog({
  client, initial_code, approved, on_approved, on_failure, on_close, return_focus, purpose = "cli",
}: {
  client: HubClient;
  initial_code: string;
  approved: boolean;
  on_approved: () => void;
  on_failure: AccountFailure;
  on_close: () => void;
  return_focus: RefObject<HTMLElement | null>;
  purpose?: AccountDevicePurpose;
}) {
  const [code, setCode] = useState(() => {
    const normalized = initial_code.replace(/-/g, "").toUpperCase();
    return /^[A-Z0-9]{8}$/.test(normalized) ? normalized.split("") : [];
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const input = useRef<HTMLInputElement>(null);
  const done = useRef<HTMLButtonElement>(null);
  const submitting = useRef(false);
  const viewer = purpose === "viewer";
  const complete = viewer ? /^[A-Z0-9]{4}-[A-Z0-9]{4}$/.test(initial_code) : /^[A-Z0-9]{8}$/.test(code.join(""));
  useEffect(() => { if (approved) done.current?.focus(); }, [approved]);

  async function approve() {
    if (submitting.current || !complete) return;
    submitting.current = true;
    setBusy(true);
    setError(undefined);
    try {
      const user_code = viewer ? initial_code : `${code.slice(0, 4).join("")}-${code.slice(4).join("")}`;
      await approveAccountDevice(client, user_code, purpose);
      if (client.signal.aborted) return;
      setCode([]);
      on_approved();
    } catch (error) { if (!client.signal.aborted) on_failure(error, setError); }
    finally {
      submitting.current = false;
      if (!client.signal.aborted) setBusy(false);
    }
  }

  const title = viewer ? "Approve an Apell sign-in" : "Approve a CLI login";
  const description = viewer
    ? "Apell can list the machines registered to this account. Only approve a request with the code shown in Tokn Sessions."
    : "Enter the device code shown in your terminal. Only approve a login you started.";
  return <AccountDialog title={title} busy={busy} on_close={on_close} return_focus={return_focus}
    initial_focus={viewer ? undefined : input} description={approved ? undefined : description}>
    {approved ? <>
      <p className="account-feedback" role="status">{viewer
        ? "Sign-in approved. Return to Tokn Sessions in Apell."
        : "CLI login approved. Return to your terminal."}</p>
      <div className="account-dialog-actions"><button ref={done} type="button" className="account-primary" onClick={on_close}>Done</button></div>
    </> : <form className="account-form" onSubmit={(event) => { event.preventDefault(); void approve(); }}>
      {viewer ? <div className="account-key-value"><code>{initial_code}</code></div>
        : <VerificationCodeInput kind="device" value={code} disabled={busy} input_ref={input} on_change={setCode} />}
      {error && <p className="account-error" role="alert">{error}</p>}
      <div className="account-dialog-actions">
        <button type="button" disabled={busy} onClick={on_close}>Cancel</button>
        <button type="submit" className="account-primary" disabled={busy || !complete}>
          {busy ? "Approving…" : viewer ? "Approve Apell sign-in" : "Approve CLI login"}
        </button>
      </div>
    </form>}
  </AccountDialog>;
}
