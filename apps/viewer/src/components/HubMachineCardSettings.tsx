import { useRef, useState } from "react";
import { machineReference, type SavedHubHost } from "../lib/hubDeviceStore";
import { AccountDialog } from "./AccountDialog";
import { SettingsIcon } from "./Icons";
import "./HubMachineCardSettings.css";

interface HubMachineCardSettingsProps {
  host: SavedHubHost;
  label: string;
  busy: boolean;
  passkeys: boolean;
  on_passkey: (host: SavedHubHost) => void;
  on_forget: (host: SavedHubHost) => void;
}

export function HubMachineCardSettings({ host, label, busy, passkeys, on_passkey, on_forget }: HubMachineCardSettingsProps) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const settings_label = `Settings for ${label}`;

  function run(action: (host: SavedHubHost) => void) {
    // HubAccess shows progress, cancellation and errors in the gallery.
    setOpen(false);
    action(host);
  }

  return <>
    <button type="button" ref={trigger} className="hub-machine-card__gear" aria-haspopup="dialog"
      aria-label={settings_label} title={settings_label} disabled={busy} onClick={() => setOpen(true)}>
      <SettingsIcon />
    </button>
    {open && <AccountDialog title="Machine settings" description={label} return_focus={trigger} on_close={() => setOpen(false)}>
      <div className="machine-settings-reference">
        <h3>Machine reference</h3>
        <code>{machineReference(host)}</code>
      </div>
      {passkeys && <button className="machine-settings-action" type="button" disabled={busy}
        onClick={() => run(on_passkey)}>Add a passkey</button>}
      <div className="machine-settings-forget">
        <p>Forgetting removes the saved reference from this device. The machine keeps its existing access permissions.</p>
        <button type="button" disabled={busy} onClick={() => run(on_forget)}>Forget machine</button>
      </div>
    </AccountDialog>}
  </>;
}
