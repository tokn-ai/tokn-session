import { useEffect, useId, useRef } from "react";
import { ConnectionIcon, ServerIcon, WarningIcon } from "./Icons";
import "./HubConnectPanel.css";

interface HubConnectPanelProps {
  state: "connecting" | "error" | "form";
  hub_url: string;
  busy: boolean;
  error?: string;
  on_url_change: (url: string) => void;
  on_connect: () => void;
  on_retry: () => void;
  on_edit: () => void;
  on_cancel?: () => void;
}

export function HubConnectPanel({ state, hub_url, busy, error, on_url_change, on_connect, on_retry, on_edit, on_cancel }: HubConnectPanelProps) {
  const title_id = useId();
  const input_id = useId();
  const hint_id = useId();
  const input = useRef<HTMLInputElement>(null);
  const retry = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (state === "form") input.current?.focus({ preventScroll: true });
    if (state === "error") retry.current?.focus({ preventScroll: true });
  }, [state]);

  return <section className="hub-connect-panel" data-state={state} aria-labelledby={title_id}>
    <span className="hub-connect-panel__mark" aria-hidden="true">
      {state === "error" ? <WarningIcon /> : <ServerIcon />}
    </span>
    <h2 id={title_id}>{state === "connecting" ? "Connecting to your Hub" : state === "error" ? "Couldn’t connect to your Hub" : "Connect to your Hub"}</h2>
    <p className="hub-connect-panel__description">{state === "connecting" ? "Your machines will be ready in a moment."
      : state === "error" ? "Check the address and your connection, then try again."
        : "Enter your Hub address to find your machines."}</p>
    {state === "form" ? <form aria-busy={busy} onSubmit={(event) => { event.preventDefault(); if (!busy) on_connect(); }}>
      <label htmlFor={input_id}>Hub address</label>
      <input ref={input} id={input_id} type="url" value={hub_url} onChange={(event) => on_url_change(event.target.value)}
        required disabled={busy} autoComplete="url" spellCheck={false} aria-describedby={hint_id} placeholder="https://hub.example.com" />
      <p id={hint_id} className="hub-connect-panel__hint">Use the same Hub as the machine you want to open.</p>
      <button className="hub-connect-primary" type="submit" disabled={busy}>Connect to Hub</button>
      {on_cancel && <button className="hub-connect-panel__secondary" type="button" disabled={busy} onClick={on_cancel}>Keep current Hub</button>}
    </form> : <>
      <div className="hub-connect-panel__address"><ConnectionIcon /><span>{hub_url}</span></div>
      {state === "connecting" ? <>
        <p className="hub-connect-panel__status" role="status"><span className="hub-connect-panel__spinner" aria-hidden="true" />Loading saved machines…</p>
        <button className="hub-connect-panel__secondary" type="button" onClick={on_cancel}>Cancel</button>
      </> : <>
        <p className="hub-connect-panel__error" role="alert">{error}</p>
        <div className="hub-connect-panel__actions">
          <button ref={retry} className="hub-connect-primary" type="button" onClick={on_retry}>Try again</button>
          <button className="hub-connect-panel__secondary" type="button" onClick={on_edit}>Change Hub</button>
        </div>
      </>}
    </>}
  </section>;
}
