import { useEffect, useRef, useState } from "react";
import { hubErrorMessage } from "../lib/hub";
import { openHubAccess, type HubAccessService } from "../lib/hubAccess";
import { canonicalHubUrl, machineReference, parseMachineReference, type SavedHubHost } from "../lib/hubDeviceStore";
import { isDesktop, selectMachine, type ConnectionState, type ViewerClient } from "../lib/transport";
import { ViewerPage } from "../pages/ViewerPage";
import { RemoteConnection } from "./RemoteConnection";
import "./PairedConnection.css";

const LAST_HUB = "tokn.last-hub.v1";
function initialHubUrl(fallback: string): string {
  if (!isDesktop()) return fallback;
  try { const saved = localStorage.getItem(LAST_HUB); return saved ? canonicalHubUrl(saved) : fallback; }
  catch { return fallback; }
}
export function HubAccess({ initial_hub_url = window.location.origin, on_local }: { initial_hub_url?: string; on_local?: () => void }) {
  const [hub_url, setHubUrl] = useState(() => initialHubUrl(initial_hub_url));
  const [service, setService] = useState<HubAccessService>();
  const [hosts, setHosts] = useState<SavedHubHost[]>([]);
  const [machine, setMachine] = useState("");
  const [code, setCode] = useState("");
  const [active, setActive] = useState<{ host: SavedHubHost; client: ViewerClient }>();
  const [connection_state, setConnectionState] = useState<ConnectionState>("connecting");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const operation = useRef<AbortController>(undefined);
  const lifetime = useRef(0);

  async function openHost(current: HubAccessService, host: SavedHubHost, signal: AbortSignal) {
    const client = await current.connect(host, signal);
    if (signal.aborted) { client.close(); return; }
    client.setStateListener(setConnectionState);
    selectMachine(client); setActive({ host, client });
  }
  async function loadService(url: string, signal: AbortSignal) {
    const next = await openHubAccess(url);
    const status = await next.status();
    if (signal.aborted) return;
    setService(next); setHubUrl(next.hub_url); setHosts(status.hosts);
    if (isDesktop()) { try { localStorage.setItem(LAST_HUB, next.hub_url); } catch { /* Keys remain in native device storage. */ } }
    const selected = status.hosts.find((host) => host.host_id === status.selected_host_id);
    if (selected) await openHost(next, selected, signal);
  }
  async function action(run: (signal: AbortSignal) => Promise<void>) {
    operation.current?.abort();
    const controller = new AbortController(); operation.current = controller;
    setBusy(true); setError(undefined); setNotice(undefined);
    try { await run(controller.signal); }
    catch (failure) { if (!controller.signal.aborted) setError(hubErrorMessage(failure)); }
    finally { if (!controller.signal.aborted) setBusy(false); }
  }
  useEffect(() => {
    const generation = ++lifetime.current;
    const url = initialHubUrl(initial_hub_url);
    if (!isDesktop() || url !== "https://") void action((signal) => loadService(url, signal));
    return () => {
      operation.current?.abort();
      queueMicrotask(() => { if (lifetime.current === generation) selectMachine(); });
    };
  }, [initial_hub_url]);

  async function refreshHosts(current: HubAccessService, signal: AbortSignal) { const status = await current.status(); if (!signal.aborted) setHosts(status.hosts); }
  async function pair(signal: AbortSignal) {
    if (!service) return;
    const reference = machine.includes("@") ? parseMachineReference(machine) : undefined;
    const host_id = reference?.host_id ?? machine.trim();
    const pairing_code = code; setCode("");
    const host = await service.pair(host_id, pairing_code, signal, reference?.host_public_key);
    if (signal.aborted) return;
    setMachine(""); await refreshHosts(service, signal);
    setNotice("Machine paired. You can add a passkey from connection settings.");
    await openHost(service, host, signal);
  }
  async function login(signal: AbortSignal) {
    if (!service) return;
    const host = parseMachineReference(machine);
    await service.authenticate(host, false, signal);
    if (signal.aborted) return;
    setMachine(""); await refreshHosts(service, signal);
    await openHost(service, host, signal);
  }
  async function addPasskey(host: SavedHubHost, signal: AbortSignal) {
    if (!service) return;
    await service.authenticate(host, true, signal);
    if (!signal.aborted) setNotice("Passkey added for this machine. Use its complete machine reference when signing in on another device.");
  }
  function changeHost() {
    operation.current?.abort(); selectMachine(); setActive(undefined); setConnectionState("connecting"); setBusy(false);
  }
  if (active) return <ViewerPage key={`${service?.hub_url}/${active.host.host_id}`} remote connection={
    <RemoteConnection name={`Hub · ${active.host.host_id.slice(0, 8)}`} state={connection_state}>
      <p className="machine-hint">{notice}</p>
      <button disabled={busy} onClick={() => { void action((signal) => addPasskey(active.host, signal)); }}>Add a passkey</button>
      <button onClick={changeHost}>Change machine</button>
      {on_local && <button onClick={() => { changeHost(); on_local(); }}>Local sessions</button>}
      {error && <p role="alert">{error}</p>}
      <details><summary>Machine reference</summary><code className="hub-machine-reference">{machineReference(active.host)}</code></details>
    </RemoteConnection>
  } />;

  return <main className="hub-home"><div className="hub-content">
    <header className="hub-header"><div><p className="hub-eyebrow">Tokn Hub</p><h1>Your machines</h1><p>Pair with an authenticator code, or sign in with a machine passkey.</p></div>
      <div className="hub-actions">{on_local && <button onClick={() => { changeHost(); on_local(); }}>Local sessions</button>}{!isDesktop() && <a href="/admin">Hub administration</a>}</div>
    </header>
    {error && <p className="hub-error" role="alert">{error}</p>}
    {notice && <p role="status">{notice}</p>}
    {(!service || isDesktop()) && <form className="paired-host-form" onSubmit={(event) => { event.preventDefault(); changeHost(); void action((signal) => loadService(hub_url, signal)); }}>
      <label htmlFor="encrypted-hub-url">Hub address</label><input id="encrypted-hub-url" type="url" value={hub_url} onChange={(event) => setHubUrl(event.target.value)} required disabled={busy} />
      <button disabled={busy}>{busy ? "Connecting…" : "Connect to Hub"}</button>
    </form>}
    {service && <>
      <p className="hub-muted">{service.hub_url} · This device is remembered for future connections.</p>
      {hosts.length > 0 && <ul className="hub-hosts" aria-label="Saved machines">{hosts.map((host) => <li key={host.host_id} className="hub-host">
        <div className="hub-host-details"><h2>Machine {host.host_id.slice(0, 8)}</h2><code>{host.host_id}</code><details><summary>Machine reference</summary><code className="hub-machine-reference">{machineReference(host)}</code></details></div>
        <div className="hub-actions"><button disabled={busy} onClick={() => { void action((signal) => openHost(service, host, signal)); }}>Open {host.host_id.slice(0, 8)}</button>
          <button disabled={busy} onClick={() => { void action((signal) => addPasskey(host, signal)); }}>Add a passkey</button>
          <button disabled={busy} onClick={() => { void action(async (signal) => { await service.forget(host.host_id); await refreshHosts(service, signal); }); }}>Forget {host.host_id.slice(0, 8)}</button></div>
      </li>)}</ul>}
      <section className="hub-pairing" aria-labelledby="encrypted-pair-title"><h2 id="encrypted-pair-title">Connect a machine</h2>
        <form className="paired-host-form" onSubmit={(event) => { event.preventDefault(); void action(pair); }}>
          <label htmlFor="encrypted-machine">Machine ID or reference</label><input id="encrypted-machine" value={machine} onChange={(event) => setMachine(event.target.value)} required autoComplete="off" spellCheck={false} disabled={busy} />
          <label htmlFor="encrypted-code">Authenticator code</label><input id="encrypted-code" value={code} onChange={(event) => setCode(event.target.value)} type="password" inputMode="numeric" pattern="[0-9]{6}" minLength={6} maxLength={6} autoComplete="off" disabled={busy} />
          <button disabled={busy || code.length !== 6}>{busy ? "Connecting…" : "Pair and connect"}</button>
          <button type="button" disabled={busy || !machine.trim()} onClick={() => { void action(login); }}>Sign in with machine passkey</button>
          <p className="machine-hint">Passkey sign-in requires the complete machine reference printed by its connector. The code is verified by your machine.</p>
        </form>
      </section>
    </>}
  </div></main>;
}
