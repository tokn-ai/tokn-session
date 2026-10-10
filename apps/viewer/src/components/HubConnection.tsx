import { useEffect, useRef, useState } from "react";
import { HubClient, HubError, hubErrorMessage, type HubEnrollment, type HubHost, type HubStatus } from "../lib/hub";
import { RemoteClient, selectMachine, type ConnectionState } from "../lib/transport";
import { RemoteConnection } from "./RemoteConnection";
import { ViewerPage } from "../pages/ViewerPage";

interface HubConnectionProps {
  initial_status: HubStatus;
  bootstrap_token?: string;
}

export function HubConnection({ initial_status, bootstrap_token }: HubConnectionProps) {
  const [configured, setConfigured] = useState(initial_status.configured);
  const [session, setSession] = useState<HubClient>();
  const [notice, setNotice] = useState<string>();

  function authenticated(next: HubClient, message?: string) {
    session?.close();
    setSession(next);
    setConfigured(true);
    setNotice(message);
  }
  function disconnected(message?: string) {
    selectMachine();
    session?.close();
    setSession(undefined);
    setNotice(message);
  }
  return session
    ? <HubHosts key={session.access_token} session={session} initial_notice={notice} on_authenticated={authenticated} on_disconnected={disconnected} />
    : <HubLogin configured={configured} bootstrap_token={bootstrap_token} notice={notice} on_authenticated={authenticated} />;
}

function HubLogin({ configured, bootstrap_token, notice, on_authenticated }: {
  configured: boolean;
  bootstrap_token?: string;
  notice?: string;
  on_authenticated: (session: HubClient) => void;
}) {
  const [bootstrap, setBootstrap] = useState(bootstrap_token ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const attempt = useRef<HubClient>(undefined);
  useEffect(() => () => attempt.current?.close(), []);

  async function authenticate() {
    const client = new HubClient();
    attempt.current = client;
    setBusy(true);
    setError(undefined);
    try {
      const next = await client.authenticate(!configured, configured ? undefined : bootstrap.trim());
      if (client.signal.aborted) { next.close(); return; }
      setBootstrap("");
      on_authenticated(next);
    } catch (error) {
      if (!client.signal.aborted) setError(hubErrorMessage(error));
    } finally {
      if (!client.signal.aborted) setBusy(false);
      client.close();
    }
  }

  return <main className="machine-connect hub-login">
    <form onSubmit={(event) => { event.preventDefault(); void authenticate(); }}>
      <p className="hub-eyebrow">Tokn Hub</p>
      <h1>{configured ? "Hub administration" : "Set up your Hub"}</h1>
      <p>{configured ? "Sign in with an administrator passkey to manage host routes." : "Create your first administrator passkey to manage this Hub."}</p>
      <a href="/">Open your machines</a>
      {!configured && <>
        <label htmlFor="hub-bootstrap">Setup token</label>
        <input id="hub-bootstrap" type="password" required value={bootstrap} disabled={busy} autoComplete="off"
          onChange={(event) => setBootstrap(event.target.value)} />
        <p className="machine-hint">Use the setup link or token printed when the Hub starts.</p>
      </>}
      {notice && <p role="status">{notice}</p>}
      {error && <p role="alert">{error}</p>}
      <button disabled={busy}>{busy ? "Waiting for passkey…" : configured ? "Sign in with passkey" : "Create passkey"}</button>
      <p className="machine-hint">Your access token stays in this tab’s memory. Refreshing signs you out.</p>
    </form>
  </main>;
}

function HubHosts({ session, initial_notice, on_authenticated, on_disconnected }: {
  session: HubClient;
  initial_notice?: string;
  on_authenticated: (session: HubClient, message?: string) => void;
  on_disconnected: (message?: string) => void;
}) {
  const [hosts, setHosts] = useState<HubHost[]>([]);
  const [enrollments, setEnrollments] = useState<HubEnrollment[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [active, setActive] = useState<{ host: HubHost; client: RemoteClient }>();
  const [state, setState] = useState<ConnectionState>("connecting");
  const [busy, setBusy] = useState<string>();
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState(initial_notice);
  const [revoke_id, setRevokeId] = useState<string>();
  const selected = useRef<string>(undefined);
  const refresh = useRef<Promise<void>>(undefined);
  const lifetime_generation = useRef(0);

  function failure(error: unknown) {
    if (session.signal.aborted) return;
    if (error instanceof HubError && error.status === 401) {
      on_disconnected("Your Hub session expired. Sign in again.");
    } else setError(hubErrorMessage(error));
  }

  function refreshHosts(): Promise<void> {
    if (refresh.current) return refresh.current;
    refresh.current = Promise.all([
      session.request<{ hosts: HubHost[] }>("hosts"),
      session.request<{ enrollments: HubEnrollment[] }>("enrollments"),
    ]).then(([catalog, pending]) => {
      if (session.signal.aborted) return;
      setHosts(catalog.hosts);
      setEnrollments(pending.enrollments);
      setLoaded(true);
      if (selected.current && !catalog.hosts.some((host) => host.host_id === selected.current)) {
        disconnectHost();
        setNotice("This host’s access was revoked.");
      }
    }).catch(failure).finally(() => { refresh.current = undefined; });
    return refresh.current;
  }

  async function refreshAfterChange() {
    // An earlier poll may have observed the catalog before the mutation.
    await refresh.current;
    if (!session.signal.aborted) await refreshHosts();
  }

  useEffect(() => {
    const generation = ++lifetime_generation.current;
    void refreshHosts();
    const timer = window.setInterval(() => { void refreshHosts(); }, 5_000);
    return () => {
      clearInterval(timer);
      selectMachine();
      // StrictMode replays effects immediately; only a real unmount should
      // retire the user-created session and pending WebAuthn/request work.
      queueMicrotask(() => { if (lifetime_generation.current === generation) session.close(); });
    };
  // Session identity owns the lifetime; the keyed parent remounts on login.
  }, [session]);

  function disconnectHost() {
    selected.current = undefined;
    selectMachine();
    setActive(undefined);
    setState("connecting");
  }

  async function connectHost(host: HubHost) {
    if (host.secure_only) return;
    setBusy(host.host_id);
    setError(undefined);
    selected.current = host.host_id;
    try {
      const client = await RemoteClient.connect(`${session.endpoint}/hosts/${encodeURIComponent(host.host_id)}`, session.access_token, session.signal);
      if (session.signal.aborted || selected.current !== host.host_id) { client.close(); return; }
      client.setStateListener(setState);
      selectMachine(client);
      setActive({ host, client });
    } catch (error) { failure(error); }
    finally { if (!session.signal.aborted) setBusy(undefined); }
  }

  async function action(id: string, operation: () => Promise<void>) {
    setBusy(id);
    setError(undefined);
    setNotice(undefined);
    try { await operation(); }
    catch (error) { failure(error); }
    finally { if (!session.signal.aborted) setBusy(undefined); }
  }

  async function logout() {
    // Capture the credential for server revocation, then clear the viewer and
    // original client immediately, even if the network is unavailable.
    const logout_client = new HubClient(session.access_token, session.endpoint);
    on_disconnected();
    const timer = window.setTimeout(() => logout_client.close(), 5_000);
    try { await logout_client.request("auth/logout", "POST", {}); }
    catch { /* The local session has already ended. */ }
    finally { clearTimeout(timer); logout_client.close(); }
  }

  if (active) return <ViewerPage key={active.host.host_id} remote connection={
    <RemoteConnection name={`Tokn Hub · ${active.host.name}${active.host.access === "view" ? " · View only" : ""}`} state={state}>
      <button onClick={disconnectHost}>Change host</button>
      <button onClick={() => { void logout(); }}>Sign out</button>
    </RemoteConnection>
  } />;

  return <main className="hub-home">
    <div className="hub-content">
      <header className="hub-header">
        <div><p className="hub-eyebrow">Tokn Hub</p><h1>Your hosts</h1><p>Manage host connections and access.</p></div>
        <div className="hub-actions">
          <button disabled={!!busy} onClick={() => { void action("passkey", async () => {
            const next = await session.authenticate(true);
            if (session.signal.aborted) { next.close(); return; }
            on_authenticated(next, "Passkey added. You can use it the next time you sign in.");
          }); }}>{busy === "passkey" ? "Waiting for passkey…" : "Add passkey"}</button>
          <button onClick={() => { void logout(); }}>Sign out</button>
        </div>
      </header>
      {error && <p className="hub-error" role="alert">{error}</p>}
      {notice && <p role="status">{notice}</p>}
      {!loaded && <p role="status">Loading hosts…</p>}
      {loaded && hosts.length === 0 && <div className="hub-empty"><h2>No hosts connected yet</h2><p>Start a host connector and approve its pairing request below.</p></div>}
      <ul className="hub-hosts" aria-label="Enrolled hosts">
        {hosts.map((host) => <li key={host.host_id} className="hub-host">
          <div className="hub-host-details"><h2>{host.name}</h2><p><span className={`hub-presence ${host.online ? "is-online" : ""}`}>{host.online ? "Online" : "Offline"}</span> · {host.secure_only ? "End-to-end encrypted" : host.access === "view" ? "View only" : "View and control"}</p><code>{host.host_id}</code>
            {host.secure_only && <p>Open your machines to pair using this host’s authenticator code or sign in with a machine passkey.</p>}
          </div>
          <div className="hub-actions">
            {!host.secure_only && <button disabled={!host.online || !!busy} onClick={() => { void connectHost(host); }}>{busy === host.host_id ? "Connecting…" : `Open ${host.name}`}</button>}
            {revoke_id === host.host_id
              ? <><span>Remove this host’s access?</span><button disabled={!!busy} onClick={() => { void action(`revoke:${host.host_id}`, async () => {
                await session.request(`hosts/${encodeURIComponent(host.host_id)}`, "DELETE");
                setRevokeId(undefined);
                await refreshAfterChange();
              }); }}>Confirm revoke</button><button disabled={!!busy} onClick={() => setRevokeId(undefined)}>Cancel</button></>
              : <button className="hub-secondary" disabled={!!busy} onClick={() => setRevokeId(host.host_id)}>Revoke {host.name}</button>}
          </div>
        </li>)}
      </ul>
      <section className="hub-pairing" aria-labelledby="hub-pairing-title">
        <h2 id="hub-pairing-title">Pair a host</h2>
        <p>Approve only a request you started. Match the host name and code with the connector’s output.</p>
        {enrollments.length === 0 ? <p className="hub-muted">Waiting for pairing requests…</p> : <ul className="hub-hosts">
          {enrollments.map((enrollment) => <li key={enrollment.host_id} className="hub-host">
            <div className="hub-host-details"><h3>{enrollment.name}</h3><p className="hub-pairing-code">{enrollment.pairing_code}</p><p>Requests {enrollment.access === "view" ? "view-only access" : "view and agent control access"} · expires in {Math.max(1, Math.ceil(enrollment.expires_in / 60))} min</p><code>{enrollment.host_id}</code></div>
            <button disabled={!!busy} onClick={() => { void action(`approve:${enrollment.host_id}`, async () => {
              await session.request("enrollments/approve", "POST", { pairing_code: enrollment.pairing_code });
              setNotice(`${enrollment.name} approved. Waiting for it to connect.`);
              await refreshAfterChange();
            }); }}>Approve {enrollment.name}</button>
          </li>)}
        </ul>}
      </section>
    </div>
  </main>;
}
