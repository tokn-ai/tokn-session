import { useEffect, useRef, useState, type ReactNode } from "react";
import { hubErrorMessage } from "../lib/hub";
import { openHubAccess, type HubAccessService } from "../lib/hubAccess";
import { canonicalHubUrl, hubHostLabel as hostLabel, machineReference, type SavedHubHost } from "../lib/hubDeviceStore";
import { deselectMachine, selectMachine, type ConnectionState, type ViewerClient } from "../lib/transport";
import type { DiscoveredMachine, HubMachineSelection, TransportState } from "../lib/types";
import { ViewerPage } from "../pages/ViewerPage";
import { RemoteConnection } from "./RemoteConnection";
import { HostSetup } from "./HostSetup";
import { HubMachineGallery, type HubGalleryAccountRoster } from "./HubMachineGallery";
import { HubPairingDialog } from "./HubPairingDialog";
import { ConnectionIcon, ServerIcon } from "./Icons";
import { SettingsEntry } from "./SettingsEntry";
import "./HubAccess.css";
import { viewerCapabilities } from "../lib/platform";

type Operation = "loading_hub" | "opening_machine" | "pairing" | "signing_in" | "adding_passkey" | "forgetting";
const OPERATION_LABELS: Record<Operation, string> = {
  loading_hub: "Loading saved machines…", opening_machine: "Connecting to machine…", pairing: "Pairing with machine…",
  signing_in: "Waiting for machine passkey…", adding_passkey: "Adding a passkey…", forgetting: "Forgetting machine…",
};
function validHubUrl(value: string): string | undefined {
  try { return canonicalHubUrl(value); } catch { return undefined; }
}

export interface HubAccessProps {
  initial_hub_url?: string;
  startup_host_id?: string | null;
  on_local?: () => void;
  local_error?: string;
  on_machine_open?: (selection: HubMachineSelection) => void;
  on_hub_ready?: (hub_url: string) => void;
  known_hub_urls?: string[];
  account_roster?: HubGalleryAccountRoster;
  account_control?: ReactNode;
}

export function HubAccess({ initial_hub_url = window.location.origin, startup_host_id, on_local, local_error, on_machine_open, on_hub_ready, known_hub_urls = [], account_roster, account_control }: HubAccessProps) {
  const [draft_hub_url, setDraftHubUrl] = useState(initial_hub_url);
  const [service, setService] = useState<HubAccessService>();
  const [hosts, setHosts] = useState<SavedHubHost[]>([]);
  const [machine, setMachine] = useState("");
  const [code, setCode] = useState("");
  const [method, setMethod] = useState<"code" | "passkey">("code");
  const [pair_target, setPairTarget] = useState<DiscoveredMachine>();
  const [pairing_open, setPairingOpen] = useState(false);
  const machine_input = useRef<HTMLInputElement>(null);
  const code_input = useRef<HTMLInputElement>(null);
  const [editing_hub, setEditingHub] = useState(false);
  const [active, setActive] = useState<{ host: SavedHubHost; client: ViewerClient }>();
  const [connection_state, setConnectionState] = useState<ConnectionState>("connecting");
  const [transport, setTransport] = useState<TransportState>({ kind: "relay" });
  const [pending, setPending] = useState<{ kind: Operation; host_id?: string }>();
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const operation = useRef<AbortController>(undefined);
  const lifetime = useRef(0);
  const service_ref = useRef<HubAccessService>(undefined);
  const active_client = useRef<ViewerClient>(undefined);
  const startup = useRef({ hub_url: initial_hub_url, host_id: startup_host_id });
  const callbacks = useRef({ on_machine_open, on_hub_ready });
  callbacks.current = { on_machine_open, on_hub_ready };
  const busy = !!pending;
  const capabilities = viewerCapabilities();
  const native = capabilities.native_hub;
  const desktop = capabilities.local_machine;
  const known_hubs = [...new Set([initial_hub_url, ...known_hub_urls, ...(service ? [service.hub_url] : [])].flatMap((url) => {
    const canonical = validHubUrl(url); return canonical ? [canonical] : [];
  }))];

  async function openHost(current: HubAccessService, host: SavedHubHost, signal: AbortSignal) {
    if (signal.aborted) return;
    setConnectionState("connecting"); setTransport({ kind: "relay" });
    const client = await current.connect(host, signal);
    if (signal.aborted) { client.close(); return; }
    try { await refreshHosts(current, signal); }
    catch (error) { client.close(); throw error; }
    if (signal.aborted) { client.close(); return; }
    active_client.current = client;
    client.setStateListener((state) => { if (active_client.current === client) setConnectionState(state); });
    client.setTransportListener?.((next) => { if (active_client.current === client) setTransport(next); });
    setPairingOpen(false); selectMachine(client); setActive({ host, client });
    callbacks.current.on_machine_open?.({ kind: "hub", hub_url: current.hub_url, host_id: host.host_id });
  }
  async function loadService(url: string, signal: AbortSignal, requested_host_id: string | null | undefined) {
    service_ref.current?.dispose?.(); service_ref.current = undefined;
    setService(undefined); setHosts([]); setMachine(""); setCode(""); setPairTarget(undefined); setPairingOpen(false);
    const next = await openHubAccess(url);
    if (signal.aborted) { next.dispose?.(); return; }
    let status: Awaited<ReturnType<HubAccessService["status"]>>;
    try { status = await next.status(); }
    catch (error) { next.dispose?.(); throw error; }
    if (signal.aborted) { next.dispose?.(); return; }
    service_ref.current = next; setService(next); setDraftHubUrl(next.hub_url); setHosts(status.hosts); setEditingHub(false);
    callbacks.current.on_hub_ready?.(next.hub_url);
    const selected_id = requested_host_id === undefined ? status.selected_host_id : requested_host_id;
    const selected = status.hosts.find((host) => host.host_id === selected_id);
    if (selected && native) { setPending({ kind: "opening_machine", host_id: selected.host_id }); await openHost(next, selected, signal); }
    else if (requested_host_id && !selected) setNotice("The selected machine is no longer saved here. Choose a machine or connect it again.");
  }
  async function action(kind: Operation, run: (signal: AbortSignal) => Promise<void>, host_id?: string) {
    operation.current?.abort();
    const controller = new AbortController(); operation.current = controller;
    setPending({ kind, host_id }); setError(undefined); setNotice(undefined);
    try { await run(controller.signal); }
    catch (failure) { if (!controller.signal.aborted) setError(hubErrorMessage(failure)); }
    finally { if (operation.current === controller) { operation.current = undefined; setPending(undefined); } }
  }
  function cancel() {
    operation.current?.abort(); operation.current = undefined; setPending(undefined); setCode("");
    setNotice("Connection canceled. Choose a machine or try again when you are ready.");
  }
  useEffect(() => {
    const generation = ++lifetime.current;
    const { hub_url, host_id } = startup.current;
    if (!native || validHubUrl(hub_url)) void action("loading_hub", (signal) => loadService(hub_url, signal, host_id));
    return () => {
      const owned_client = active_client.current;
      operation.current?.abort(); active_client.current = undefined;
      queueMicrotask(() => {
        if (lifetime.current === generation) {
          if (owned_client) deselectMachine(owned_client);
          service_ref.current?.dispose?.(); service_ref.current = undefined;
        }
      });
    };
  // Startup props describe this mounted picker. Preference writes must not reopen it.
  }, []);

  useEffect(() => {
    if (pairing_open) (pair_target ? code_input : machine_input).current?.focus({ preventScroll: true });
  }, [pair_target, pairing_open]);

  useEffect(() => {
    if (native) return;
    const lock = () => {
      operation.current?.abort(); operation.current = undefined;
      selectMachine(); active_client.current = undefined;
      service_ref.current?.dispose?.(); service_ref.current = undefined;
      setActive(undefined); setService(undefined); setPairingOpen(false); setPending(undefined); setCode("");
    };
    const restore = (event: PageTransitionEvent) => { if (event.persisted) window.location.reload(); };
    window.addEventListener("pagehide", lock);
    window.addEventListener("pageshow", restore);
    return () => {
      window.removeEventListener("pagehide", lock);
      window.removeEventListener("pageshow", restore);
    };
  }, []);

  async function refreshHosts(current: HubAccessService, signal: AbortSignal) {
    const status = await current.status(); if (!signal.aborted) setHosts(status.hosts);
  }
  async function addMachine(signal: AbortSignal) {
    if (!service) return;
    const target = machine.trim();
    const pairing_code = code; setCode("");
    const host = method === "code" ? await service.pairMachine(target, pairing_code, signal) : await service.loginMachine(target, signal);
    if (signal.aborted) return;
    setMachine(""); setPairTarget(undefined); await refreshHosts(service, signal);
    if (signal.aborted) return;
    setPending({ kind: "opening_machine", host_id: host.host_id });
    await openHost(service, host, signal);
  }
  async function addPasskey(host: SavedHubHost, signal: AbortSignal) {
    if (!service) return;
    await service.authenticate(host, true, signal);
    if (!signal.aborted) setNotice("Passkey added for this machine. Copy its machine reference to sign in on another device.");
  }
  function changeHost() {
    operation.current?.abort(); operation.current = undefined; active_client.current = undefined;
    selectMachine(); setActive(undefined); setPairingOpen(false); setCode(""); setConnectionState("connecting"); setPending(undefined); setError(undefined); setNotice(undefined);
  }
  function openPairing() { setPairingOpen(true); setError(undefined); setNotice(undefined); }
  function closePairing() {
    if (pending) cancel();
    setPairingOpen(false); setCode(""); setPairTarget(undefined); setError(undefined);
  }
  const progress = pending && <div className="hub-progress" role="status"><span>{OPERATION_LABELS[pending.kind]}</span>
    {pending.kind !== "forgetting" && <button type="button" onClick={cancel}>Cancel</button>}</div>;
  if (active) return <ViewerPage key={`${service?.hub_url}/${active.host.host_id}`} remote
    machine_label={hostLabel(active.host)} machine_key={`${service?.hub_url}/${active.host.host_id}`} connection={
    <RemoteConnection name={hostLabel(active.host)} hub_url={service?.hub_url} encrypted state={connection_state} transport={transport}>
      <div className="connection-primary-actions"><button className="connection-panel__primary" onClick={changeHost}>Machines</button></div>
      <div className="connection-secondary-settings">
        {progress}{notice && <p role="status">{notice}</p>}{error && <p className="hub-error" role="alert">{error}</p>}
        {capabilities.passkeys && <button disabled={busy} onClick={() => { void action("adding_passkey", (signal) => addPasskey(active.host, signal), active.host.host_id); }}>Add a machine passkey</button>}
        <details><summary>Machine identity</summary><p>Use this reference when signing in from a new device.</p><code className="hub-machine-reference">{machineReference(active.host)}</code></details>
      </div>
    </RemoteConnection>
  } />;

  return <main className="hub-home hub-access"><div className="hub-content">
    <header className="hub-header"><div className="hub-app-title"><span className="hub-app-mark"><ServerIcon /></span><div><span className="hub-app-brand">Tokn Sessions</span><h1>Machines</h1></div></div>
      <div className="hub-actions"><SettingsEntry />{!native && account_control}</div>
    </header>
    {desktop && <section className="hub-this-machine" aria-labelledby="this-machine-title">
      <div><h2 id="this-machine-title">This machine</h2><p>Open sessions saved on this computer.</p>{local_error && <p className="hub-error" role="alert">{local_error}</p>}</div>
      <div className="hub-this-machine-actions"><button aria-label="Open This machine" disabled={!on_local} onClick={() => { changeHost(); on_local?.(); }}>{local_error ? "Retry" : "Open"}</button>
      <HostSetup /></div>
    </section>}
    {native && known_hubs.length > 0 && <nav className="hub-known-hubs" aria-label="Saved Hubs"><span className="hub-field-label">Remote machines through</span>
      {known_hubs.map((url) => <button key={url} aria-pressed={service?.hub_url === url} disabled={busy} onClick={() => { changeHost(); setDraftHubUrl(url); void action("loading_hub", (signal) => loadService(url, signal, null)); }}>{url}</button>)}
    </nav>}
    {service && <section className="hub-origin" aria-label="Current Hub"><div><ConnectionIcon /><strong>{service.hub_url}</strong></div>
      {native && <button disabled={busy} onClick={() => { setDraftHubUrl(service.hub_url); setEditingHub((value) => !value); }}>Change Hub</button>}</section>}
    {(!service || editing_hub) && <form className="hub-origin-form" onSubmit={(event) => { event.preventDefault(); changeHost(); void action("loading_hub", (signal) => loadService(draft_hub_url, signal, null)); }}>
      {!service && <><h2>Connect a machine</h2><p className="hub-muted">Enter its Hub address to open saved machines or pair another machine.</p></>}
      <label htmlFor="encrypted-hub-url">Hub address</label><input id="encrypted-hub-url" type="url" value={draft_hub_url} onChange={(event) => setDraftHubUrl(event.target.value)} required disabled={busy} placeholder="https://hub.example.com" />
      <div className="hub-actions"><button disabled={busy}>Connect to Hub</button>{service && <button type="button" disabled={busy} onClick={() => setEditingHub(false)}>Keep current Hub</button>}</div>
    </form>}
    {!pairing_open && <>{progress}{error && <p className="hub-error hub-feedback" role="alert">{error}</p>}{notice && <p className="hub-feedback" role="status">{notice}</p>}</>}
    {service && <HubMachineGallery key={service.hub_url} service={service} hosts={hosts} busy={busy}
      account_roster={account_roster?.hub_url === service.hub_url ? account_roster : undefined}
      opening_host_id={pending?.kind === "opening_machine" ? pending.host_id : undefined}
      on_open={(host) => { void action("opening_machine", (signal) => openHost(service, host, signal), host.host_id); }}
      on_pair={(candidate) => {
        setMachine(candidate.host_id); setPairTarget(candidate); setMethod("code"); setCode(""); openPairing();
      }} on_add={openPairing}
      on_passkey={(host) => { void action("adding_passkey", (signal) => addPasskey(host, signal), host.host_id); }}
      on_forget={(host) => { void action("forgetting", async (signal) => { await service.forget(host.host_id); await refreshHosts(service, signal); }, host.host_id); }} />}
    {!native && <footer className="hub-app-footer"><a href="/admin">Hub administration</a></footer>}
    {service && pairing_open && <HubPairingDialog on_close={closePairing}>
        {progress}{error && <p className="hub-error hub-feedback" role="alert">{error}</p>}{notice && <p className="hub-feedback" role="status">{notice}</p>}
        <form className="hub-machine-form" onSubmit={(event) => { event.preventDefault(); void action(method === "code" ? "pairing" : "signing_in", addMachine); }}>
          {pair_target && <p className="machine-hint">Pairing with <strong>{pair_target.name}</strong>. Enter its current authenticator code to verify this machine.</p>}
          <label htmlFor="encrypted-machine">Machine address or reference</label><input ref={machine_input} id="encrypted-machine" value={machine} onChange={(event) => { setMachine(event.target.value); setPairTarget(undefined); }} required autoComplete="off" spellCheck={false} disabled={busy} placeholder="username:host" />
          {capabilities.passkeys && <fieldset className="hub-auth-method" disabled={busy}><legend>Connect using</legend>
            <label><input type="radio" name="machine-auth-method" checked={method === "code"} onChange={() => { setMethod("code"); setError(undefined); }} />Use authenticator code</label>
            <label><input type="radio" name="machine-auth-method" checked={method === "passkey"} onChange={() => { setMethod("passkey"); setCode(""); setError(undefined); }} />Use machine passkey</label>
          </fieldset>}
          {method === "code" ? <><label htmlFor="encrypted-code">Authenticator code</label><input ref={code_input} id="encrypted-code" value={code} onChange={(event) => setCode(event.target.value)} type="password" inputMode="numeric" pattern="[0-9]{6}" minLength={6} maxLength={6} required autoComplete="one-time-code" disabled={busy} />
            <p className="machine-hint">Enter the current six-digit code from the machine’s authenticator. Your machine verifies it.</p></>
            : <p className="machine-hint">On a new device, paste <strong>username:host@public_key</strong> or the complete machine reference from its connector. Remembered machines can use their address.</p>}
          <button className="hub-connect-primary" disabled={busy || !machine.trim() || (method === "code" && !/^[0-9]{6}$/.test(code))}>{method === "code" ? (native ? "Pair and connect" : "Pair and set up passkey") : "Sign in with machine passkey"}</button>
          <details className="hub-input-help"><summary>Where do I find the address?</summary><p>Use your Hub machine address, or paste the machine ID or reference printed by the host connector.</p></details>
        </form>
    </HubPairingDialog>}
  </div></main>;
}
