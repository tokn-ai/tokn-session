import { useEffect, useRef, type RefObject } from "react";
import { createPortal } from "react-dom";
import { isDesktop } from "../lib/transport";
import { useMachineSettings } from "../lib/useMachineSettings";
import { useViewerPreferences } from "../lib/viewerPreferences";
import { SettingsPage } from "../pages/SettingsPage";
import { MachineHostingSettings } from "./MachineHostingSettings";
import { NativePreferenceSaveNotice } from "./NativePreferenceSaveNotice";
import { ActionNotificationHost } from "./ActionNotifications";
import "./SettingsDialog.css";

interface SettingsDialogProps {
  remote: boolean;
  machine_label: string;
  machine_key: string;
  connected?: boolean;
  on_close: () => void;
  return_focus: RefObject<HTMLButtonElement | null>;
}

export function SettingsDialog({ remote, machine_label, machine_key, connected = true, on_close, return_focus }: SettingsDialogProps) {
  const dialog = useRef<HTMLDialogElement>(null);
  const local = !remote && isDesktop();
  const machine = useMachineSettings(local, undefined, connected);
  const viewer = useViewerPreferences();
  useEffect(() => {
    const opener = return_focus.current;
    dialog.current?.showModal();
    return () => { if (opener?.isConnected) opener.focus({ preventScroll: true }); };
  }, [return_focus]);
  return createPortal(<dialog ref={dialog} className="settings-dialog" aria-label="Settings"
    onCancel={(event) => { event.preventDefault(); on_close(); }}>
    <SettingsPage
      on_close={on_close}
      machine_label={machine_label}
      machine_key={machine_key}
      machine_connected={connected}
      preferences={viewer.preferences}
      on_preferences_change={viewer.updatePreferences}
      on_preferences_reset={viewer.resetPreferences}
      settings={machine.settings}
      settings_loading={machine.loading}
      settings_error={machine.error}
      on_settings_retry={machine.retry}
      on_reader_apply={machine.applyReader}
      hosting={<div className="settings-page__card">{!connected ? <p>Choose a machine to view its hosting settings.</p> : local ? <MachineHostingSettings /> : <p className="settings-page__notice">
        {machine.settings?.hosting.reason ?? "Hosting management is unavailable through this connection. Open this machine’s desktop app to manage hosting."}
      </p>}</div>}
    />
    <NativePreferenceSaveNotice placement="settings" />
    <ActionNotificationHost />
  </dialog>, document.body);
}
