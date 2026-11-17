import { useEffect, useRef, useState, type RefObject } from "react";
import type { HubClient } from "../lib/hub";
import { approveAccountDevice } from "../lib/hubAccount";
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

function DeviceApprovalDialog({ client, initial_code, approved, on_approved, on_failure, on_close, return_focus }: {
  client: HubClient;
  initial_code: string;
  approved: boolean;
  on_approved: () => void;
  on_failure: AccountFailure;
  on_close: () => void;
  return_focus: RefObject<HTMLButtonElement | null>;
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
  const complete = /^[A-Z0-9]{8}$/.test(code.join(""));
  useEffect(() => { if (approved) done.current?.focus(); }, [approved]);

  async function approve() {
    if (submitting.current || !complete) return;
    submitting.current = true;
    setBusy(true);
    setError(undefined);
    try {
      await approveAccountDevice(client, `${code.slice(0, 4).join("")}-${code.slice(4).join("")}`);
      if (client.signal.aborted) return;
      setCode([]);
      on_approved();
    } catch (error) { if (!client.signal.aborted) on_failure(error, setError); }
    finally {
      submitting.current = false;
      if (!client.signal.aborted) setBusy(false);
    }
  }

  return <AccountDialog title="Approve a CLI login" busy={busy} on_close={on_close} return_focus={return_focus} initial_focus={input}
    description={approved ? undefined : "Enter the device code shown in your terminal. Only approve a login you started."}>
    {approved ? <>
      <p className="account-feedback" role="status">CLI login approved. Return to your terminal.</p>
      <div className="account-dialog-actions"><button ref={done} type="button" className="account-primary" onClick={on_close}>Done</button></div>
    </> : <form className="account-form" onSubmit={(event) => { event.preventDefault(); void approve(); }}>
      <VerificationCodeInput kind="device" value={code} disabled={busy} input_ref={input} on_change={setCode} />
      {error && <p className="account-error" role="alert">{error}</p>}
      <div className="account-dialog-actions">
        <button type="button" disabled={busy} onClick={on_close}>Cancel</button>
        <button type="submit" className="account-primary" disabled={busy || !complete}>{busy ? "Approving…" : "Approve CLI login"}</button>
      </div>
    </form>}
  </AccountDialog>;
}
