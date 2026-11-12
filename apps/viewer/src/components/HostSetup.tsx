import { BackgroundHostingControls } from "./BackgroundHostingControls";
import { useEffect, useId, useRef, useState, type RefObject } from "react";
import { getLocalHostPairing, openLocalHostLogin } from "../lib/tauri";
import { canonicalHubUrl } from "../lib/hubDeviceStore";
import { parseMachineAddress } from "../lib/hubAddress";
import type { LocalHostLogin, LocalHostPairing, StartLocalHostRequest } from "../lib/types";
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

function validMachineName(value: string): boolean {
  try { parseMachineAddress(`account:${value}`); return true; }
  catch { return false; }
}

function LoginApproval({ login }: { login: LocalHostLogin }) {
  const [error, setError] = useState<string>();
  return <section className="host-login" aria-label="Hub sign-in approval">
    <p>Approve this code in your browser to continue hosting.</p>
    <strong className="host-login__code">{login.user_code}</strong>
    <button type="button" onClick={() => {
      setError(undefined);
      void openLocalHostLogin().catch((error) => setError(message(error)));
    }}>Open sign-in page</button>
    {error && <p role="alert" className="hub-error">{error}</p>}
  </section>;
}

function MachineAddress({ address, disabled, external, on_rename }: {
  address: string;
  disabled: boolean;
  external: boolean;
  on_rename: (machine_name: string) => Promise<boolean>;
}) {
  const id = useId();
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState("");
  const current_name = address.split(":")[1];
  return <section className="host-machine" aria-label="Machine address">
    <div className="host-machine__heading"><div><span>Machine address</span><strong>{address}</strong></div>
      {!editing && <button type="button" disabled={disabled || external} onClick={() => { setName(current_name); setEditing(true); }}>Rename</button>}
    </div>
    {editing && <form onSubmit={(event) => {
      event.preventDefault();
      if (disabled || !validMachineName(name.trim()) || name.trim() === current_name) return;
      void on_rename(name.trim()).then((success) => { if (success) setEditing(false); });
    }}>
      <label htmlFor={`${id}-rename`}>New machine name</label>
      <input id={`${id}-rename`} value={name} maxLength={63} required disabled={disabled} autoComplete="off" autoCapitalize="none" spellCheck={false} onChange={(event) => setName(event.target.value)} />
      {name.trim() && !validMachineName(name.trim()) && <p>Use lowercase letters, numbers, and internal hyphens.</p>}
      <div className="host-form-actions"><button disabled={disabled || !validMachineName(name.trim()) || name.trim() === current_name}>Save name</button>
        <button type="button" disabled={disabled} onClick={() => setEditing(false)}>Cancel rename</button></div>
    </form>}
    {external && <p>Stop external hosting before renaming this computer.</p>}
  </section>;
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
          if (!hosting && (!host.status?.machine_address || !host.status.name)) open();
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
  const status = host.status;
  const hosting = isHosting(status);
  const external = status?.phase === "external";
  const background = status?.phase === "background";
  const unavailable = status?.phase === "unavailable";
  const pending_setup = status?.phase === "signing_in" || status?.phase === "registering";
  const renaming = status?.phase === "renaming";
  const showing_saved_host = hosting || external || background;
  const saved_settings: StartLocalHostRequest = {
    hub_url: status?.hub_url ?? "", name: status?.name ?? "This computer", allow_control: status?.allow_control ?? false,
    ...(status?.machine_name ? { machine_name: status.machine_name } : {}),
  };
  const settings = showing_saved_host ? saved_settings : draft ?? saved_settings;
  const { hub_url, name, allow_control, machine_name = "" } = settings;
  const hub_origin = validHubOrigin(hub_url);
  const saved_origin = status && validHubOrigin(status.hub_url);
  const machine_address = hub_origin && hub_origin === saved_origin ? status?.machine_address : undefined;
  const can_start = Boolean(hub_origin && name.trim()) && (Boolean(machine_address) || !machine_name.trim() || validMachineName(machine_name.trim())) && !host.busy && !unavailable;
  const can_cancel = pending_setup || renaming || (host.busy && !hosting && !external && !background);
  useEffect(() => {
    const opener = host.return_focus ?? document.activeElement;
    dialog.current?.showModal();
    return () => { if (opener instanceof HTMLElement && opener.isConnected) opener.focus(); };
  }, []);
  return createPortal(<dialog ref={dialog} className="host-dialog" aria-labelledby={`${id}-title`}
    onCancel={(event) => { event.preventDefault(); host.close(); }}>
    <header><div><h2 id={`${id}-title`}>Host this computer</h2><p>{external ? "This computer is hosted by an external connector." : "Share this computer’s sessions while the app is open."}</p></div>
      <button type="button" className="icon-button" aria-label="Close hosting settings" onClick={host.close}><CloseIcon /></button></header>
    <div className="host-dialog__body">
      {host.status && <div className="host-status" data-phase={host.status.phase}><span className="connection-dot" aria-hidden="true" /><span role="status">{hostLabels[host.status.phase]}</span></div>}
      {showing_saved_host ? <>
        <dl className="host-summary"><div><dt>Computer</dt><dd>{host.status?.name}</dd></div>
          <div><dt>Hub</dt><dd>{host.status?.hub_url}</dd></div>
          {status?.account_username && <div><dt>Account</dt><dd>{status.account_username}</dd></div>}
          <div><dt>Access</dt><dd>{host.status?.allow_control ? "Viewing and agent input" : "Viewing only"}</dd></div></dl>
        {!external && !background && !can_cancel && <button type="button" disabled={host.busy || unavailable} onClick={() => { setShowPairing(false); void host.change(); }}>{host.busy ? "Stopping…" : "Stop hosting"}</button>}
        {hosting && !pending_setup && !renaming && <p className="host-hint">Stop hosting to change these settings.</p>}
      </> : <form onSubmit={(event) => {
        event.preventDefault();
        if (!can_start) return;
        setShowPairing(false);
        void host.change({ hub_url, name, allow_control, ...(!machine_address && machine_name.trim() ? { machine_name: machine_name.trim() } : {}) });
      }}>
        <label htmlFor={`${id}-hub`}>Hosting Hub address</label><input id={`${id}-hub`} type="url" required placeholder="https://hub.example.com" value={hub_url} disabled={host.busy || unavailable} onChange={(event) => setDraft({ ...settings, hub_url: event.target.value })} />
        <label htmlFor={`${id}-name`}>Display name</label><input id={`${id}-name`} required maxLength={128} value={name} disabled={host.busy || unavailable} onChange={(event) => setDraft({ ...settings, name: event.target.value })} />
        {!machine_address && <><label htmlFor={`${id}-machine`}>Machine name</label><input id={`${id}-machine`} maxLength={63} value={machine_name} placeholder="Suggested from display name" autoComplete="off" autoCapitalize="none" spellCheck={false} disabled={host.busy || unavailable} onChange={(event) => setDraft({ ...settings, machine_name: event.target.value })} />
          {machine_name.trim() && !validMachineName(machine_name.trim()) && <p>Use lowercase letters, numbers, and internal hyphens.</p>}
          <p>Start hosting signs in to your Hub when needed and adds this computer to your account.</p></>}
        <label className="host-control"><input type="checkbox" checked={allow_control} disabled={host.busy || unavailable} onChange={(event) => setDraft({ ...settings, allow_control: event.target.checked })} />Allow remote agent input</label>
        <button disabled={!can_start}>{host.busy ? "Starting…" : "Start hosting"}</button>
      </form>}
      {status?.phase === "signing_in" && status.login && <LoginApproval login={status.login} />}
      {can_cancel && <button type="button" onClick={() => { setShowPairing(false); void host.cancel(); }}>Cancel</button>}
      {machine_address && <MachineAddress key={machine_address} address={machine_address} disabled={host.busy || unavailable || pending_setup || renaming}
        external={external || background} on_rename={host.rename} />}
      <BackgroundHostingControls on_action={() => setShowPairing(false)} />
      {(host.error || host.status?.error) && <p className="hub-error" role="alert">{host.error ?? host.status?.error}</p>}
      {unavailable && <button type="button" disabled={host.busy} onClick={host.retry}>Retry host status</button>}
      {!host.status && (host.error ? <button type="button" onClick={host.retry}>Retry host status</button> : <p role="status">Loading host settings…</p>)}
      {machine_address && host.status?.machine_reference && <section className="host-pairing-section">
        <button type="button" aria-expanded={show_pairing} disabled={host.busy || unavailable} onClick={() => setShowPairing((value) => !value)}>{show_pairing ? "Hide pairing setup" : "Show pairing setup"}</button>
        {show_pairing && <PairingSetup />}
      </section>}
    </div>
    <ActionNotificationHost />
  </dialog>, document.body);
}
