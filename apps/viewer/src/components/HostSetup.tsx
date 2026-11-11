import { BackgroundHostingControls } from "./BackgroundHostingControls";
import { useEffect, useId, useRef, useState, type RefObject } from "react";
import { getLocalHostPairing } from "../lib/tauri";
import { canonicalHubUrl } from "../lib/hubDeviceStore";
import type { LocalHostPairing, StartLocalHostRequest } from "../lib/types";
import { createPortal } from "react-dom";
import { hostLabels, isHosting, useLocalHost } from "../lib/localHost";
import { CloseIcon } from "./Icons";
import { ActionNotificationHost } from "./ActionNotifications";
import "./HostSetup.css";

const message = (error: unknown) => error instanceof Error ? error.message : String(error);

function validHubOrigin(value: string): string | undefined {
  try {
    return canonicalHubUrl(value.trim());
  } catch {
    return undefined;
  }
}

function AccountRegistration({ hub_origin, stop_first, initial_setup, busy, on_refresh }: {
  hub_origin?: string;
  stop_first: boolean;
  initial_setup: boolean;
  busy: boolean;
  on_refresh: () => void;
}) {
  const hub = hub_origin ?? "https://hub.example.com";
  const hub_argument = /^[a-z0-9_.:/-]+$/i.test(hub) ? hub : `'${hub.replace(/'/g, "'\\''")}'`;
  const loopback_argument = hub.startsWith("http:") ? " --insecure-loopback" : "";
  return <details className="host-registration" open={initial_setup || undefined}>
    <summary>Register this computer with a Hub account</summary>
    <div>
      <p>Register with your Hub account to give this computer an address under your username.</p>
      {stop_first && <p>Stop hosting on this computer before running the registration command.</p>}
      <ol>
        <li><p>Sign in to your Hub account in the terminal.</p><code>tokn-session-hub auth login --hub {hub_argument}</code></li>
        <li><p>Choose a machine name in place of <code>my-computer</code> and register it.</p><code>tokn-session-hub connect --hub {hub_argument}{loopback_argument} --machine my-computer --register-only</code></li>
      </ol>
      <p>Refresh hosting status when registration is complete.</p>
      <button type="button" disabled={busy} onClick={on_refresh}>Refresh hosting status</button>
    </div>
  </details>;
}

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
        {pairing.discovery_group && <p>Discovery group <strong>{pairing.discovery_group.namespace}</strong></p>}
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
  const external = host.status?.phase === "external";
  const background = host.status?.phase === "background";
  const unavailable = host.status?.phase === "unavailable";
  const open = () => { on_open_settings(); host.open(return_focus?.current); };
  return <section className="host-quick" aria-label="This computer hosting">
    <div className="host-quick__heading"><div><strong>This computer</strong>
      <p role="status">{host.status ? hostLabels[host.status.phase] : "Loading hosting status…"}</p></div>
      <button type="button" className="host-switch" role="switch" aria-label="Host this computer" aria-checked={hosting || external}
        disabled={host.busy || !host.status || external || background || unavailable} onClick={() => {
          if (!hosting && (!host.status?.hub_url || !host.status.name)) open();
          else void host.change();
        }}><span><span /></span></button>
    </div>
    <p>{background ? "A background API is still running. Stop it in the hosting dialog." : external ? "Managed by background hosting." : "Available remotely while this app is open."}</p>
    {(host.error || host.status?.error) && <p role="alert">{host.error ?? host.status?.error}</p>}
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
  const hub_origin = validHubOrigin(hub_url);
  const saved_origin = host.status && validHubOrigin(host.status.hub_url);
  const machine_address = hub_origin && hub_origin === saved_origin ? host.status?.machine_address : undefined;
  useEffect(() => {
    const opener = host.return_focus ?? document.activeElement;
    dialog.current?.showModal();
    return () => { if (opener instanceof HTMLElement && opener.isConnected) opener.focus(); };
  }, []);
  const hosting = isHosting(host.status);
  const external = host.status?.phase === "external";
  const background = host.status?.phase === "background";
  const unavailable = host.status?.phase === "unavailable";
  return createPortal(<dialog ref={dialog} className="host-dialog" aria-labelledby={`${id}-title`}
    onCancel={(event) => { event.preventDefault(); host.close(); }}>
    <header><div><h2 id={`${id}-title`}>Host this computer</h2><p>{external ? "This computer is hosted by an external connector." : "Share this computer’s sessions while the app is open."}</p></div>
      <button type="button" className="icon-button" aria-label="Close hosting settings" onClick={host.close}><CloseIcon /></button></header>
    <div className="host-dialog__body">
      {host.status && <div className="host-status" data-phase={host.status.phase}><span className="connection-dot" aria-hidden="true" /><span role="status">{hostLabels[host.status.phase]}</span></div>}
      {hosting || external || background ? <>
        <dl className="host-summary"><div><dt>Computer</dt><dd>{host.status?.name}</dd></div>
          <div><dt>Hub</dt><dd>{host.status?.hub_url}</dd></div>
          {machine_address ? <div><dt>Machine address</dt><dd>{machine_address}</dd></div>
            : host.status?.discovery_namespace && <div><dt>Legacy discovery group</dt><dd>{host.status.discovery_namespace}</dd></div>}
          <div><dt>Access</dt><dd>{host.status?.allow_control ? "Viewing and agent input" : "Viewing only"}</dd></div></dl>
        {!external && !background && <button type="button" disabled={host.busy || unavailable} onClick={() => { setShowPairing(false); void host.change(); }}>{host.busy ? "Stopping…" : "Stop hosting"}</button>}
        {hosting && <p className="host-hint">Stop hosting to change these settings.</p>}
      </> : <form onSubmit={(event) => {
        event.preventDefault(); setShowPairing(false);
        void host.change({ hub_url, name, allow_control });
      }}>
        <label htmlFor={`${id}-hub`}>Hosting Hub address</label><input id={`${id}-hub`} type="url" required placeholder="https://hub.example.com" value={hub_url} disabled={host.busy || unavailable} onChange={(event) => setDraft({ ...settings, hub_url: event.target.value })} />
        <label htmlFor={`${id}-name`}>Host name</label><input id={`${id}-name`} required maxLength={128} value={name} disabled={host.busy || unavailable} onChange={(event) => setDraft({ ...settings, name: event.target.value })} />
        {machine_address && <p className="host-hint">Machine address <strong>{machine_address}</strong></p>}
        <label className="host-control"><input type="checkbox" checked={allow_control} disabled={host.busy || unavailable} onChange={(event) => setDraft({ ...settings, allow_control: event.target.checked })} />Allow remote agent input</label>
        <button disabled={host.busy || unavailable || !host.status || !hub_url.trim() || !name.trim()}>{host.busy ? "Starting…" : "Start hosting"}</button>
      </form>}
      {host.status && !machine_address && <AccountRegistration hub_origin={hub_origin}
        stop_first={hosting || external || background} initial_setup={!host.status.machine_reference}
        busy={host.busy} on_refresh={host.retry} />}
      <BackgroundHostingControls on_action={() => setShowPairing(false)} />
      {(host.error || host.status?.error) && <p className="hub-error" role="alert">{host.error ?? host.status?.error}</p>}
      {unavailable && <button type="button" disabled={host.busy} onClick={host.retry}>Retry host status</button>}
      {!host.status && (host.error ? <button type="button" onClick={host.retry}>Retry host status</button> : <p role="status">Loading host settings…</p>)}
      {host.status?.machine_reference && <section className="host-pairing-section">
        <button type="button" aria-expanded={show_pairing} disabled={host.busy || unavailable} onClick={() => setShowPairing((value) => !value)}>{show_pairing ? "Hide pairing setup" : "Show pairing setup"}</button>
        {show_pairing && <PairingSetup />}
      </section>}
    </div>
    <ActionNotificationHost />
  </dialog>, document.body);
}
