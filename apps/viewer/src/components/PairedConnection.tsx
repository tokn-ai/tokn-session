import "./PairedConnection.css";
import { useEffect, useRef, useState } from "react";
import { RemoteClient, selectMachine, type ConnectionState } from "../lib/transport";
import { RemoteConnection } from "./RemoteConnection";
import { ViewerPage } from "../pages/ViewerPage";

interface SavedHost {
  host_id: string;
  host_public_key: string;
}
interface Selection {
  host_id: string;
  connection_id: string;
}
interface LocalStatus {
  hosts: SavedHost[];
  selected: Selection | null;
  hub_url: string;
}

/** Onboarding is served only by the installed local client, never the Hub. */
export function PairedConnection({ initial_token }: { initial_token?: string }) {
  const [hosts, setHosts] = useState<SavedHost[]>([]);
  const [hub_url, setHubUrl] = useState("");
  const [host_id, setHostId] = useState("");
  const [code, setCode] = useState("");
  const [loaded, setLoaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const [active, setActive] = useState<Selection>();
  const [connection_state, setConnectionState] = useState<ConnectionState>("connecting");
  const operation = useRef<AbortController>(undefined);

  async function request<T>(command: string, signal: AbortSignal, body?: object): Promise<T> {
    const response = await fetch(`/api/local/${command}`, {
      method: body === undefined ? "GET" : "POST",
      headers: {
        Authorization: `Bearer ${initial_token ?? ""}`,
        ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
      credentials: "omit",
      cache: "no-store",
      redirect: "error",
    });
    if (!response.ok) {
      const result: { error?: string } = await response.json().catch(() => ({}));
      throw new Error(result.error ?? `Local client request failed (${response.status})`);
    }
    return response.status === 204 ? undefined as T : response.json() as Promise<T>;
  }

  async function openHost(selected: Selection, signal: AbortSignal) {
    const endpoint = `${window.location.origin}/paired/${encodeURIComponent(selected.connection_id)}`;
    const next = await RemoteClient.connect(endpoint, initial_token ?? "", signal);
    if (signal.aborted) { next.close(); return; }
    next.setStateListener(setConnectionState);
    selectMachine(next);
    setActive(selected);
  }

  useEffect(() => {
    if (!initial_token) return;
    const controller = new AbortController();
    operation.current = controller;
    setBusy(true);
    void request<LocalStatus>("status", controller.signal).then(async (status) => {
      if (controller.signal.aborted) return;
      setHosts(status.hosts);
      setHubUrl(status.hub_url);
      setLoaded(true);
      if (status.selected) await openHost(status.selected, controller.signal);
    }).catch((failure: unknown) => {
      if (!controller.signal.aborted) setError(failure instanceof Error ? failure.message : String(failure));
    }).finally(() => { if (!controller.signal.aborted) setBusy(false); });
    return () => {
      controller.abort();
      operation.current?.abort();
      selectMachine();
    };
  // The token identifies one local client lifetime. It never enters persistent browser storage.
  }, [initial_token]);

  async function action(run: (signal: AbortSignal) => Promise<void>) {
    operation.current?.abort();
    const controller = new AbortController();
    operation.current = controller;
    setBusy(true);
    setError(undefined);
    try { await run(controller.signal); }
    catch (failure) {
      if (!controller.signal.aborted) setError(failure instanceof Error ? failure.message : String(failure));
    } finally { if (!controller.signal.aborted) setBusy(false); }
  }

  async function selectHost(host_id: string, signal: AbortSignal) {
    const { selected } = await request<{ selected: Selection }>("select", signal, { host_id });
    await openHost(selected, signal);
  }

  async function pairHost(signal: AbortSignal) {
    // Clear the visible code immediately. Failed pairing always requires an
    // explicit new attempt, never an automatic retry of a consumed code.
    const pairing_code = code;
    setCode("");
    const { selected } = await request<{ selected: Selection }>("pair", signal, { host_id: host_id.trim(), code: pairing_code });
    if (signal.aborted) return;
    setHostId("");
    const status = await request<LocalStatus>("status", signal);
    if (signal.aborted) return;
    setHosts(status.hosts);
    await openHost(selected, signal);
  }

  async function changeHost(signal: AbortSignal) {
    selectMachine();
    setActive(undefined);
    setConnectionState("connecting");
    await request("disconnect", signal, {});
  }

  if (!initial_token) return <main className="machine-connect"><section>
    <h1>Your hosts</h1>
    <p>Open the connection link printed by your local Tokn client to continue.</p>
    <p className="machine-hint">The link keeps access to this local client in your tab’s memory.</p>
  </section></main>;

  if (active) return <ViewerPage key={active.connection_id} remote machine_label={`Encrypted host · ${active.host_id}`}
    machine_key={active.connection_id} connection={
    <RemoteConnection name={`Encrypted host · ${active.host_id}`} state={connection_state}>
      <button disabled={busy} onClick={() => { void action(changeHost); }}>Change host</button>
    </RemoteConnection>
  } />;

  return <main className="hub-home"><div className="hub-content">
    <header className="hub-header"><div>
      <p className="hub-eyebrow">Tokn · local client</p>
      <h1>Your hosts</h1>
      <p>{hub_url ? `Connected through ${hub_url}` : "Connect to your hosts through your Hub."}</p>
    </div></header>
    {error && <p className="hub-error" role="alert">{error}</p>}
    {!loaded && <p role="status">{busy ? "Loading saved hosts…" : "Could not load your hosts. Reopen the connection link to retry."}</p>}
    {hosts.length > 0 && <ul className="hub-hosts" aria-label="Saved hosts">
      {hosts.map((host) => <li className="hub-host" key={host.host_id}>
        <div className="hub-host-details"><h2>Paired host</h2><code>{host.host_id}</code><p>No authenticator code needed to reconnect.</p></div>
        <button disabled={busy} onClick={() => { void action((signal) => selectHost(host.host_id, signal)); }}>Open {host.host_id}</button>
      </li>)}
    </ul>}
    <section className="hub-pairing" aria-labelledby="pair-host-title">
      <h2 id="pair-host-title">Add a host</h2>
      <p>Copy the host ID printed by its connector and enter the current code from your authenticator.</p>
      <form className="paired-host-form" onSubmit={(event) => { event.preventDefault(); void action(pairHost); }}>
        <label htmlFor="paired-host-id">Host ID</label>
        <input id="paired-host-id" value={host_id} required disabled={busy || !loaded} autoComplete="off" spellCheck={false}
          onChange={(event) => setHostId(event.target.value)} />
        <label htmlFor="paired-host-code">Authenticator code</label>
        <input id="paired-host-code" type="password" inputMode="numeric" autoComplete="off" pattern="[0-9]{6}" minLength={6} maxLength={6}
          value={code} required disabled={busy || !loaded} onChange={(event) => setCode(event.target.value)} />
        <button disabled={busy || !loaded}>{busy ? "Connecting…" : "Pair and connect"}</button>
      </form>
    </section>
  </div></main>;
}
