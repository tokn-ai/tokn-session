import { useRef, useState } from "react";
import { SettingsIcon } from "./Icons";
import { SettingsDialog } from "./SettingsDialog";

/** Device preferences are available before any machine connection is opened. */
export function SettingsEntry() {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  return <>
    <button aria-label="Open settings" aria-haspopup="dialog" className="settings-entry"
      onClick={() => setOpen(true)} ref={trigger} type="button"><SettingsIcon />Settings</button>
    {open && <SettingsDialog remote connected={false} machine_label="No machine selected" machine_key="none"
      return_focus={trigger} on_close={() => setOpen(false)} />}
  </>;
}
