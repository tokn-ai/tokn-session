import { useEffect, useState } from "react";
import "./App.css";
import { RemoteConnection } from "./components/RemoteConnection";
import { ViewerPage } from "./pages/ViewerPage";
import { isDesktop, RemoteClient, selectMachine, type ConnectionState } from "./lib/transport";
import { PairedConnection } from "./components/PairedConnection";
import { HubConnection } from "./components/HubConnection";
import { detectHub, hubErrorMessage, type HubStatus } from "./lib/hub";
import { HubAccess } from "./components/HubAccess";
import { NativePasskey } from "./components/NativePasskey";

function BrowserViewer({ initial_token }: { initial_token?: string }) {
  const [endpoint, setEndpoint] = useState(() => window.location.origin);
  const [token, setToken] = useState("");
  const [client, setClient] = useState<RemoteClient>();
  const [state, setState] = useState<ConnectionState>("connecting");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  useEffect(() => () => selectMachine(), []);
  useEffect(() => {
    if (!initial_token) return;
    let cancelled = false;
    setBusy(true);
    void RemoteClient.connect(window.location.origin, initial_token).then((next) => {
      if (cancelled) { next.close(); return; }
      next.setStateListener(setState);
      selectMachine(next);
      setClient(next);
    }).catch((error: unknown) => {
      if (!cancelled) setError(error instanceof Error ? error.message : String(error));
    }).finally(() => { if (!cancelled) setBusy(false); });
    return () => { cancelled = true; };
  }, [initial_token]);

  async function connect() {
    setBusy(true); setError(undefined);
    try {
      const next = await RemoteClient.connect(endpoint.trim(), token);
      next.setStateListener(setState);
      selectMachine(next); setClient(next); setToken("");
    } catch (error) { setError(error instanceof Error ? error.message : String(error)); }
    finally { setBusy(false); }
  }
  if (client) return <ViewerPage remote connection={
    <RemoteConnection name={client.endpoint} state={state}>
      <button onClick={() => { selectMachine(); setClient(undefined); setState("connecting"); }}>Change machine</button>
    </RemoteConnection>
  } />;
  return <main className="machine-connect">
    <form onSubmit={(event) => { event.preventDefault(); void connect(); }}>
      <h1>Session viewer</h1>
      <p>Connect to the viewer server on the machine whose sessions you want to read.</p>
      <label htmlFor="machine-url">Viewer address</label>
      <input id="machine-url" type="url" value={endpoint} disabled={busy} required onChange={(event) => setEndpoint(event.target.value)} />
      <label htmlFor="machine-token">Access token</label>
      <input id="machine-token" type="password" value={token} disabled={busy} autoComplete="off" onChange={(event) => setToken(event.target.value)} />
      <p className="machine-hint">The token is kept only for this connection.</p>
      {error && <p role="alert">{error}</p>}
      <button disabled={busy}>{busy ? "Connecting…" : "Connect"}</button>
    </form>
  </main>;
}
function BrowserGateway({ bootstrap_token }: { bootstrap_token?: string }) {
  const [status, setStatus] = useState<HubStatus | null>();
  const [error, setError] = useState<string>();
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    let cancelled = false;
    const timer = window.setTimeout(() => controller.abort(), 10_000);
    setError(undefined);
    void detectHub(controller.signal).then((next) => {
      if (!cancelled) setStatus(next);
    }).catch((error: unknown) => {
      if (!cancelled) setError(controller.signal.aborted ? "The server did not respond. Try again." : hubErrorMessage(error));
    }).finally(() => clearTimeout(timer));
    return () => { cancelled = true; clearTimeout(timer); controller.abort(); };
  }, [attempt]);
  if (status === null) return <BrowserViewer />;
  if (status) return window.location.pathname === "/admin"
    ? <HubConnection initial_status={status} bootstrap_token={bootstrap_token} />
    : <HubAccess />;
  return <main className="machine-connect"><form onSubmit={(event) => { event.preventDefault(); setAttempt((value) => value + 1); }}>
    <h1>Session viewer</h1>
    {error ? <><p role="alert">{error}</p><button>Retry connection</button></> : <p role="status">Connecting…</p>}
  </form></main>;
}

function App({ initial_token, bootstrap_token }: { initial_token?: string; bootstrap_token?: string }) {
  const [desktop_hub, setDesktopHub] = useState(false);
  if (isDesktop()) return desktop_hub
    ? <HubAccess initial_hub_url="https://" on_local={() => setDesktopHub(false)} />
    : <ViewerPage on_open_hub={() => setDesktopHub(true)} />;
  if (window.location.pathname === "/passkey") return <NativePasskey />;
  if (window.location.pathname === "/connect") return <PairedConnection initial_token={initial_token} />;
  return initial_token ? <BrowserViewer initial_token={initial_token} /> : <BrowserGateway bootstrap_token={bootstrap_token} />;
}
export default App;
