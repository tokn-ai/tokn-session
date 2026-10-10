import { useEffect, useId, useRef, useState, type RefObject } from "react";
import { getLocalHostPairing } from "../lib/tauri";
import type { LocalHostPairing, StartLocalHostRequest } from "../lib/types";
import { createPortal } from "react-dom";
import { hostLabels, isHosting, useLocalHost } from "../lib/localHost";
import { CloseIcon } from "./Icons";
import "./HostSetup.css";

const message = (error: unknown) => error instanceof Error ? error.message : String(error);

function PairingSetup() {
  const [pairing, setPairing] = useState<LocalHostPairing>();
  const [remaining, setRemaining] = useState(0);
  const [error, setError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let alive = true; let pending = false; let expires = 0;
    const load = async () => {
      if (pending) return; pending = true;
      try {
        const result = await getLocalHostPairing();
        if (alive) {
          expires = performance.now() + (result.expires_at - result.host_time) * 1000;
          setRemaining(Math.max(0, result.expires_at - result.host_time)); setPairing(result); setError(undefined);
        }
      } catch (error) { if (alive) { setPairing(undefined); setError(message(error)); } }
      finally { pending = false; }
    };
    void load();
    const timer = setInterval(() => {
      if (!expires) return;
      const seconds = Math.max(0, Math.ceil((expires - performance.now()) / 1000)); setRemaining(seconds);
      if (seconds === 0) { expires = 0; setPairing(undefined); void load(); }
    }, 1000);
    return () => { alive = false; clearInterval(timer); };
  }, [attempt]);
  return <div className="host-pairing">
    {error ? <><p role="alert" className="hub-error">{error}</p><button type="button" onClick={() => setAttempt((value) => value + 1)}>Retry pairing display</button></>
      : !pairing ? <p role="status">Loading pairing setup…</p> : <>
        <label>Machine reference<code className="hub-machine-reference">{pairing.machine_reference}</code></label>
        <p>Use this reference to connect from another device. Scan the QR with an authenticator that supports SHA-256.</p>
        <img src={pairing.qr_data_url} width="220" height="220" alt="SHA-256 authenticator setup QR" />
        <div className="host-current-code"><span>Current verification code</span><strong>{pairing.current_code}</strong><span>Valid for {remaining}s</span></div>
        <p>Compare this code with your authenticator. Settings: TOTP, SHA-256, 6 digits, 30 seconds.</p>
        <details><summary>Full authenticator setup URI</summary><code className="hub-machine-reference">{pairing.setup_uri}</code>
          <p>A manually entered secret does not specify SHA-256. Set the algorithm explicitly in your authenticator.</p></details>
        <p>Pairing setup contains your authenticator secret. Keep it private.</p>
      </>}
  </div>;
}

export function HostSetup() {
  const host = useLocalHost();
  if (!host) return null;
  return <button type="button" className="host-setup-trigger" aria-haspopup="dialog" onClick={() => host.open()}>Host this computer</button>;
}

export function HostQuickControl({ on_open_settings, return_focus }: { on_open_settings: () => void; return_focus?: RefObject<HTMLButtonElement | null> }) {
  const host = useLocalHost();
  if (!host) return null;
  const hosting = isHosting(host.status);
  const open = () => { on_open_settings(); host.open(return_focus?.current); };
  return <section className="host-quick" aria-label="This computer hosting">
    <div className="host-quick__heading"><div><strong>This computer</strong>
      <p role="status">{host.status ? hostLabels[host.status.phase] : "Loading hosting status…"}</p></div>
      <button type="button" className="host-switch" role="switch" aria-label="Host this computer" aria-checked={hosting}
        disabled={host.busy || !host.status} onClick={() => {
          if (!hosting && (!host.status?.hub_url || !host.status.name)) open();
          else void host.change();
        }}><span><span /></span></button>
    </div>
    <p>Available remotely while this app is open.</p>
    {(host.error || host.status?.error) && <p role="alert">{host.error ?? host.status?.error}</p>}
    <button type="button" onClick={open}>Hosting settings</button>
  </section>;
}

export function HostDialog() {
  const host = useLocalHost()!;
  const dialog = useRef<HTMLDialogElement>(null);
  const id = useId();
  const [draft, setDraft] = useState<StartLocalHostRequest>();
  const [show_pairing, setShowPairing] = useState(false);
  const settings = draft ?? host.status ?? { hub_url: "", name: "This computer", allow_control: false };
  const { hub_url, name, allow_control } = settings;
  useEffect(() => {
    const opener = host.return_focus ?? document.activeElement;
    dialog.current?.showModal();
    return () => { if (opener instanceof HTMLElement && opener.isConnected) opener.focus(); };
  }, []);
  const hosting = isHosting(host.status);
  return createPortal(<dialog ref={dialog} className="host-dialog" aria-labelledby={`${id}-title`}
    onCancel={(event) => { event.preventDefault(); host.close(); }}>
    <header><div><h2 id={`${id}-title`}>Host this computer</h2><p>Share this computer’s sessions while the app is open.</p></div>
      <button type="button" className="icon-button" aria-label="Close hosting settings" onClick={host.close}><CloseIcon /></button></header>
    <div className="host-dialog__body">
      {host.status && <div className="host-status" data-phase={host.status.phase}><span className="connection-dot" aria-hidden="true" /><span role="status">{hostLabels[host.status.phase]}</span></div>}
      {hosting ? <>
        <dl className="host-summary"><div><dt>Computer</dt><dd>{host.status?.name}</dd></div>
          <div><dt>Hub</dt><dd>{host.status?.hub_url}</dd></div>
          <div><dt>Access</dt><dd>{host.status?.allow_control ? "Viewing and agent input" : "Viewing only"}</dd></div></dl>
        <button type="button" disabled={host.busy} onClick={() => { setShowPairing(false); void host.change(); }}>{host.busy ? "Stopping…" : "Stop hosting"}</button>
        <p className="host-hint">Stop hosting to change these settings.</p>
      </> : <form onSubmit={(event) => { event.preventDefault(); setShowPairing(false); void host.change({ hub_url, name, allow_control }); }}>
        <label htmlFor={`${id}-hub`}>Hosting Hub address</label><input id={`${id}-hub`} type="url" required placeholder="https://hub.example.com" value={hub_url} disabled={host.busy} onChange={(event) => setDraft({ ...settings, hub_url: event.target.value })} />
        <label htmlFor={`${id}-name`}>Host name</label><input id={`${id}-name`} required maxLength={128} value={name} disabled={host.busy} onChange={(event) => setDraft({ ...settings, name: event.target.value })} />
        <label className="host-control"><input type="checkbox" checked={allow_control} disabled={host.busy} onChange={(event) => setDraft({ ...settings, allow_control: event.target.checked })} />Allow remote agent input</label>
        <button disabled={host.busy || !host.status || !hub_url.trim() || !name.trim()}>{host.busy ? "Starting…" : "Start hosting"}</button>
      </form>}
      {(host.error || host.status?.error) && <p className="hub-error" role="alert">{host.error ?? host.status?.error}</p>}
      {!host.status && (host.error ? <button type="button" onClick={host.retry}>Retry host status</button> : <p role="status">Loading host settings…</p>)}
      {host.status?.machine_reference && <section className="host-pairing-section">
        <button type="button" aria-expanded={show_pairing} disabled={host.busy} onClick={() => setShowPairing((value) => !value)}>{show_pairing ? "Hide pairing setup" : "Show pairing setup"}</button>
        {show_pairing && <PairingSetup />}
      </section>}
    </div>
  </dialog>, document.body);
}
