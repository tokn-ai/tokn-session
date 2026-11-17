import { useEffect, useState } from "react";
import "./App.css";
import { RemoteConnection } from "./components/RemoteConnection";
import { ViewerPage } from "./pages/ViewerPage";
import { isDesktop, RemoteClient, selectMachine, type ConnectionState } from "./lib/transport";
import { PairedConnection } from "./components/PairedConnection";
import { detectHub, hubErrorMessage, type HubStatus } from "./lib/hub";
import { BrowserHub } from "./pages/BrowserHub";
import { NativePasskey } from "./components/NativePasskey";
import { DesktopWorkspace } from "./pages/DesktopWorkspace";
import { RemoteWorkspace } from "./pages/RemoteWorkspace";
import { viewerHost } from "./lib/platform";
import { SettingsEntry } from "./components/SettingsEntry";
import { ActionNotificationsProvider } from "./components/ActionNotifications";
import { AuthLayout } from "./components/AuthLayout";

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
  if (client) return <ViewerPage key={client.endpoint} remote machine_label={client.endpoint} machine_key={client.endpoint} connection={
    <RemoteConnection name={client.endpoint} state={state}>
      <button onClick={() => { selectMachine(); setClient(undefined); setState("connecting"); }}>Change machine</button>
    </RemoteConnection>
  } />;
  return <AuthLayout navigation={<SettingsEntry />} footer="Your sessions, wherever you work.">
    <form className="auth-form" aria-busy={busy} onSubmit={(event) => { event.preventDefault(); void connect(); }}>
      <header className="auth-heading">
        <p className="auth-eyebrow">Session viewer</p>
        <h1>Connect to your viewer</h1>
        <p className="auth-description">Pick up your agent sessions on another machine.</p>
      </header>
      <div className="auth-field">
        <label htmlFor="machine-url">Viewer address</label>
        <input id="machine-url" type="url" autoComplete="url" spellCheck={false} value={endpoint} disabled={busy} required
          aria-describedby="machine-url-hint" onChange={(event) => setEndpoint(event.target.value)} />
        <p className="auth-hint" id="machine-url-hint">The address of the viewer server on your machine.</p>
      </div>
      <div className="auth-field">
        <label htmlFor="machine-token">Access token</label>
        <input id="machine-token" type="password" value={token} disabled={busy} autoComplete="off"
          aria-describedby="machine-token-hint" onChange={(event) => setToken(event.target.value)} />
        <p className="auth-hint" id="machine-token-hint">The token is kept only for this connection.</p>
      </div>
      {error && <p className="auth-feedback" role="alert">{error}</p>}
      <button className="auth-primary" disabled={busy}>{busy ? "Connecting…" : "Connect"}</button>
    </form>
  </AuthLayout>;
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
  if (status) return <BrowserHub status={status} bootstrap_token={bootstrap_token} />;
  return <AuthLayout footer="Your sessions, wherever you work.">
    <form className="auth-form" onSubmit={(event) => { event.preventDefault(); setAttempt((value) => value + 1); }}>
      <header className="auth-heading">
        <p className="auth-eyebrow">Tokn</p>
        <h1>Session viewer</h1>
        <p className="auth-description">Your agent sessions are one connection away.</p>
      </header>
      {error ? <><p className="auth-feedback" role="alert">{error}</p><button className="auth-primary">Retry connection</button></>
        : <p className="auth-feedback" role="status">Connecting…</p>}
    </form>
  </AuthLayout>;
}

interface AppProps { initial_token?: string; bootstrap_token?: string; }

function AppContent({ initial_token, bootstrap_token }: AppProps) {
  if (viewerHost().kind === "apell") return <RemoteWorkspace />;
  if (isDesktop()) return <DesktopWorkspace />;
  if (window.location.pathname === "/passkey") return <NativePasskey />;
  if (window.location.pathname === "/connect") return <PairedConnection initial_token={initial_token} />;
  return initial_token ? <BrowserViewer initial_token={initial_token} /> : <BrowserGateway bootstrap_token={bootstrap_token} />;
}

function App(props: AppProps) {
  return <ActionNotificationsProvider><AppContent {...props} /></ActionNotificationsProvider>;
}
export default App;
