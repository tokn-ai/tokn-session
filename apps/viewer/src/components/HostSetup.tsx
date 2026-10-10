import { useEffect, useRef, useState } from "react";
import { getLocalHostPairing, getLocalHostStatus, listenForLocalHostStatus, startLocalHost, stopLocalHost } from "../lib/tauri";
import type { LocalHostPairing, LocalHostStatus } from "../lib/types";
import "./HostSetup.css";

const labels: Record<LocalHostStatus["phase"], string> = {
  stopped: "Not hosting from this app", connecting: "Connecting to Hub…", online: "Hosting through Hub",
  reconnecting: "Reconnecting to Hub…", error: "Hosting stopped with an error",
};
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
  const [status, setStatus] = useState<LocalHostStatus>();
  const [hub_url, setHubUrl] = useState("");
  const [name, setName] = useState("This computer");
  const [allow_control, setAllowControl] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  const [show_pairing, setShowPairing] = useState(false);
  const alive = useRef(true);
  const event_version = useRef(0);
  useEffect(() => {
    alive.current = true; let active = true; let unlisten: (() => void) | undefined; let received = false;
    void (async () => {
      try {
        const stop = await listenForLocalHostStatus((next) => { if (active) { received = true; event_version.current++; setStatus(next); } });
        if (!active) { stop(); return; } unlisten = stop;
        const initial = await getLocalHostStatus();
        if (active) {
          if (!received) setStatus(initial);
          setHubUrl(initial.hub_url); setName(initial.name); setAllowControl(initial.allow_control); setError(undefined);
        }
      } catch (error) { if (active) setError(message(error)); }
    })();
    return () => { active = false; alive.current = false; unlisten?.(); };
  }, [attempt]);
  const hosting = status !== undefined && status.phase !== "stopped";
  async function changeHosting() {
    const version = event_version.current;
    setBusy(true); setError(undefined); setShowPairing(false);
    try {
      const next = hosting ? await stopLocalHost() : await startLocalHost({ hub_url: hub_url.trim(), name: name.trim(), allow_control });
      if (alive.current && version === event_version.current) setStatus(next);
    } catch (error) { if (alive.current) setError(message(error)); }
    finally { if (alive.current) setBusy(false); }
  }
  return <details className="host-setup" onToggle={(event) => { if (!event.currentTarget.open) setShowPairing(false); }}>
    <summary>Host this computer{hosting && <span className="host-setup-badge">{status.phase === "online" ? "Online" : "Active"}</span>}</summary>
    <p>Make this computer’s sessions available through a Hub while the app is open.</p>
    {status && <p role="status">{labels[status.phase]}</p>}
    <form onSubmit={(event) => { event.preventDefault(); void changeHosting(); }}>
      <label htmlFor="host-hub-url">Hosting Hub address</label><input id="host-hub-url" type="url" required placeholder="https://hub.example.com" value={hub_url} disabled={busy || hosting} onChange={(event) => setHubUrl(event.target.value)} />
      <label htmlFor="host-name">Host name</label><input id="host-name" required maxLength={128} value={name} disabled={busy || hosting} onChange={(event) => setName(event.target.value)} />
      <label className="host-control"><input type="checkbox" checked={allow_control} disabled={busy || hosting} onChange={(event) => setAllowControl(event.target.checked)} />Allow remote agent input</label>
      <button disabled={busy || !status || (!hosting && (!hub_url.trim() || !name.trim()))}>{busy ? (hosting ? "Stopping…" : "Starting…") : hosting ? "Stop hosting" : "Start hosting"}</button>
    </form>
    {status?.error && <p className="hub-error" role="alert">{status.error}</p>}
    {error && <p className="hub-error" role="alert">{error}</p>}
    {!status && (error ? <button type="button" onClick={() => setAttempt((value) => value + 1)}>Retry host status</button> : <p role="status">Loading host settings…</p>)}
    {status?.machine_reference && <>
      <button type="button" aria-expanded={show_pairing} disabled={busy} onClick={() => setShowPairing((value) => !value)}>{show_pairing ? "Hide pairing setup" : "Show pairing setup"}</button>
      {show_pairing && <PairingSetup />}
    </>}
  </details>;
}
