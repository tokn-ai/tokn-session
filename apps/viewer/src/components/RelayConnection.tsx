import { useCallback, useEffect, useId, useRef, useState } from "react";
import { configureRelay, getRelayStatus, listenForRelayStatus, listenForTransportReconnect } from "../lib/tauri";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import { HostQuickControl } from "./HostSetup";
import { ChevronIcon, CloseIcon } from "./Icons";
import type { RelayMode, RelaySettings, RelayStatus } from "../lib/types";
import "./ConnectionPanel.css";

const PHASE_LABELS: Record<RelayStatus["phase"], string> = {
  local: "Local history",
  starting: "Connecting",
  connecting: "Connecting",
  live: "Live updates",
  reconnecting: "Reconnecting",
  retrying: "Reconnecting",
  failed: "Connection needs attention",
};
const STATUS_LOAD_ERROR = "Relay connection status is unavailable.";

export function RelayConnection({ on_open_machines }: { on_open_machines?: () => void }) {
  const [status, setStatus] = useState<RelayStatus | null>(null);
  const [settings, setSettings] = useState<RelaySettings>({ mode: "automatic", endpoint: "tcp://127.0.0.1:5557", include_native: false });
  const [busy, setBusy] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const statusRevision = useRef(0);
  const [is_open, setOpen] = useState(false);
  const panel_id = useId();
  const trigger_ref = useRef<HTMLButtonElement>(null);
  const close = useCallback(() => setOpen(false), []);
  const panel_ref = useFloatingPanel(is_open, close, trigger_ref);
  useEffect(() => {
    let disposed = false;
    let stop: (() => void) | undefined;
    let stopReconnect: (() => void) | undefined;
    let settingsInitialized = false;
    let connectionEpoch = 0;
    void listenForTransportReconnect(() => {
      if (disposed) return;
      const epoch = ++connectionEpoch;
      const before = statusRevision.current;
      void getRelayStatus().then((next) => {
        if (!disposed && epoch === connectionEpoch) {
          setLoadError(null);
          if (statusRevision.current === before) {
            statusRevision.current += 1;
            setStatus(next);
            if (!settingsInitialized) {
              settingsInitialized = true;
              setSettings(next.settings);
            }
          }
        }
      }).catch(() => {
        if (!disposed && epoch === connectionEpoch && statusRevision.current === before) {
          setLoadError(STATUS_LOAD_ERROR);
        }
      });
    }).then((unlisten) => {
      if (disposed) unlisten();
      else stopReconnect = unlisten;
    }).catch(() => {});
    const subscribeEpoch = connectionEpoch;
    const subscribeRevision = statusRevision.current;
    void listenForRelayStatus((next) => {
      statusRevision.current += 1;
      if (!disposed) {
        setStatus(next);
        setLoadError(null);
        if (!settingsInitialized) {
          settingsInitialized = true;
          setSettings(next.settings);
        }
      }
    }).then(async (unlisten) => {
      if (disposed) { unlisten(); return; }
      stop = unlisten;
      const epoch = connectionEpoch;
      const before = statusRevision.current;
      try {
        const next = await getRelayStatus();
        if (!disposed && epoch === connectionEpoch) {
          setLoadError(null);
          if (statusRevision.current === before) {
            statusRevision.current += 1;
            setStatus(next);
            if (!settingsInitialized) {
              settingsInitialized = true;
              setSettings(next.settings);
            }
          }
        }
      } catch {
        if (!disposed && epoch === connectionEpoch && statusRevision.current === before) {
          setLoadError(STATUS_LOAD_ERROR);
        }
      }
    }).catch(() => {
      if (!disposed && connectionEpoch === subscribeEpoch && statusRevision.current === subscribeRevision) {
        setLoadError(STATUS_LOAD_ERROR);
      }
    });
    return () => { disposed = true; stop?.(); stopReconnect?.(); };
  }, []);

  async function save() {
    if (busy || !status) return;
    setBusy(true);
    setSaveError(null);
    const before = statusRevision.current;
    try {
      const next = await configureRelay({ ...settings, endpoint: settings.endpoint.trim() });
      if (statusRevision.current === before) {
        statusRevision.current += 1;
        setStatus(next);
      }
      setLoadError(null);
    } catch (error) {
      setSaveError(String(error));
    } finally {
      setBusy(false);
    }
  }

  const mode = status?.settings.mode ?? "automatic";
  const phase = status?.phase ?? "starting";
  const error = saveError ?? loadError;
  const label = saveError ? "Settings need attention"
    : loadError ? "Connection unavailable"
    : mode === "local" ? "Local history" : PHASE_LABELS[phase];

  return (
    <div className="relay-connection">
      <button
        aria-controls={panel_id}
        aria-expanded={is_open}
        aria-haspopup="dialog"
        aria-label={`${on_open_machines ? "This machine. " : ""}${label}. Connection settings`}
        className="status-bar__connection"
        data-phase={error ? "failed" : phase}
        onClick={() => {
          if (!is_open && status && !busy) setSettings(status.settings);
          setOpen((open) => !open);
        }}
        ref={trigger_ref}
        title="Connection settings"
        type="button"
      >
        <span aria-hidden="true" className="connection-dot" />
        <span>{on_open_machines ? `This machine · ${label}` : label}</span>
      </button>
      {is_open && <div
        aria-labelledby={`${panel_id}-title`}
        aria-modal="false"
        className="connection-panel"
        id={panel_id}
        ref={panel_ref}
        role="dialog"
        tabIndex={-1}
      >
        <header className="connection-panel__header">
          <div>
            <h2 id={`${panel_id}-title`}>Connection</h2>
            <p className="connection-panel__summary">{label} · {mode === "automatic" ? "Automatic" : mode === "external" ? "External" : "Local"}</p>
          </div>
          <button aria-label="Close connection settings" className="icon-button" onClick={close} type="button"><CloseIcon /></button>
        </header>
        {on_open_machines && <div className="connection-panel__machine">
          <dl className="connection-summary"><div><dt>Machine</dt><dd>This machine</dd></div></dl>
          <button className="connection-panel__primary" onClick={on_open_machines} type="button">Machines<ChevronIcon className="connection-panel__navigate" /></button>
        </div>}
        {on_open_machines && <HostQuickControl on_open_settings={close} return_focus={trigger_ref} />}
        <form className="relay-settings" onSubmit={(event) => { event.preventDefault(); void save(); }}>
          <div className="relay-field">
            <label htmlFor={`${panel_id}-mode`}>Data source</label>
            <div className="relay-select">
              <select aria-describedby={`${panel_id}-description`} id={`${panel_id}-mode`} value={settings.mode} disabled={busy || !status} onChange={(event) => setSettings({ ...settings, mode: event.target.value as RelayMode })}>
                <option value="automatic">Automatic (recommended)</option>
                <option value="external">External Relay</option>
                <option value="local">Local history only</option>
              </select>
              <ChevronIcon />
            </div>
          </div>
          {settings.mode === "external" && <div className="relay-field">
            <label htmlFor={`${panel_id}-endpoint`}>Relay endpoint</label>
            <input id={`${panel_id}-endpoint`} value={settings.endpoint} onChange={(event) => setSettings({ ...settings, endpoint: event.target.value })} disabled={busy} spellCheck={false} />
          </div>}
          {settings.mode === "automatic" && <label className="relay-native">
            <input type="checkbox" checked={settings.include_native} disabled={busy || !status} onChange={(event) => setSettings({ ...settings, include_native: event.target.checked })} />
            Include native records
          </label>}
          <p id={`${panel_id}-description`}>{settings.mode === "automatic"
            ? "Read saved sessions and receive live updates automatically. Native records add provider-specific details to the inspector."
            : settings.mode === "external"
              ? "Connect to a Relay snapshot service running on this machine. You manage that service separately."
              : "Read saved sessions without starting a live Relay connection."}</p>
          {status?.active_endpoint && mode === "external" && <p>Connected to <code>{status.active_endpoint}</code></p>}
          {status && ["reconnecting", "retrying", "failed"].includes(phase) && <p>{mode === "automatic"
            ? phase === "failed"
              ? "Live updates are unavailable. Saved history remains available."
              : "Live updates are reconnecting. Saved history remains available."
            : "Showing the last received data while the connection is unavailable."}</p>}
          {(error || status?.error) && <p role="alert">{error ?? status?.error}</p>}
          <div className="connection-panel__footer"><button className="connection-panel__apply" type="submit" disabled={busy || !status}>{busy ? "Saving…" : phase === "failed" && settings.mode === "automatic" ? "Retry" : "Apply"}</button></div>
        </form>
      </div>}
    </div>
  );
}
