import { useCallback, useId, useRef, useState, type ReactNode } from "react";
import type { ConnectionState } from "../lib/transport";
import type { TransportState } from "../lib/types";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import { CloseIcon } from "./Icons";
import "./ConnectionPanel.css";

/** The same compact connection entry point for direct, Hub, and paired viewers. */
export function RemoteConnection({ name, state, hub_url, encrypted = false, transport, children }: {
  name: string;
  state: ConnectionState;
  hub_url?: string;
  encrypted?: boolean;
  transport?: TransportState;
  children: ReactNode;
}) {
  const [is_open, setOpen] = useState(false);
  const id = useId();
  const trigger_ref = useRef<HTMLButtonElement>(null);
  const close = useCallback(() => setOpen(false), []);
  const panel_ref = useFloatingPanel(is_open, close, trigger_ref);
  const label = state === "connected" ? "Connected" : state === "reconnecting" ? "Reconnecting" : "Connecting";
  const path_label = transport?.kind === "direct" ? "Direct" : transport?.kind === "relay" ? "Relayed" : undefined;
  return (
    <div className="relay-connection">
      <button
        aria-controls={id}
        aria-expanded={is_open}
        aria-haspopup="dialog"
        aria-label={`${name}. ${label}. Connection settings`}
        className="status-bar__connection"
        data-phase={state === "connected" ? "live" : state}
        onClick={() => setOpen((open) => !open)}
        ref={trigger_ref}
        title={name}
        type="button"
      >
        <span aria-hidden="true" className="connection-dot" />
        <span>{path_label && state === "connected" ? path_label : label} · {name}</span>
      </button>
      {is_open && <div aria-labelledby={`${id}-title`} className="connection-panel" id={id} ref={panel_ref} role="dialog" tabIndex={-1}>
        <header className="connection-panel__header">
          <h2 id={`${id}-title`}>Connection</h2>
          <button aria-label="Close connection settings" className="icon-button" onClick={close} type="button"><CloseIcon /></button>
        </header>
        <div className="remote-connection__body">
          <dl className="connection-summary">
            <div><dt>Machine</dt><dd>{name}</dd></div>
            {hub_url && <div><dt>Hub</dt><dd>{hub_url}</dd></div>}
            {path_label && <div><dt>Traffic</dt><dd>{path_label}{encrypted ? " · encrypted" : ""}</dd></div>}
            {encrypted && <div><dt>Security</dt><dd>End-to-end encrypted</dd></div>}
          </dl>
          {transport?.kind === "relay" && transport.reason && <p className="connection-state">{transport.reason}</p>}
          <p className="connection-state" role="status">{state === "reconnecting" ? "Reconnecting · showing last received data" : label}</p>
          <div className="remote-connection__actions">{children}</div>
        </div>
      </div>}
    </div>
  );
}
