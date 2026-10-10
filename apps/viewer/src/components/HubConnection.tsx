import { useEffect, useRef, useState } from "react";
import { HubClient, HubError, hubErrorMessage, type HubEnrollment, type HubHost, type HubNamespace, type HubStatus } from "../lib/hub";
import { parseMachineAddress } from "../lib/hubAddress";
import { RemoteClient, selectMachine, type ConnectionState } from "../lib/transport";
import { RemoteConnection } from "./RemoteConnection";
import { ViewerPage } from "../pages/ViewerPage";
import "./HubAccess.css";

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
  const [namespaces, setNamespaces] = useState<HubNamespace[]>([]);
  const [username, setUsername] = useState("");
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
      session.request<{ namespaces: HubNamespace[] }>("namespaces"),
    ]).then(([catalog, pending, namespace_catalog]) => {
      if (session.signal.aborted) return;
      setHosts(catalog.hosts);
      setEnrollments(pending.enrollments);
      setNamespaces(namespace_catalog.namespaces);
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
    <RemoteConnection name={`${active.host.machine_address ?? active.host.name}${active.host.access === "view" ? " · View only" : ""}`} hub_url={session.endpoint} state={state}>
      <button onClick={disconnectHost}>Change host</button>
      <button onClick={() => { void logout(); }}>Sign out</button>
    </RemoteConnection>
  } />;

  return <main className="hub-home">
    <div className="hub-content">
      <header className="hub-header">
        <div><p className="hub-eyebrow">Tokn Hub</p><h1>Hub administration</h1><p>Manage machine addresses and host routes.</p></div>
        <div className="hub-actions">
          <a href="/">Open your machines</a>
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
      {loaded && <section className="hub-namespace-section" aria-labelledby="hub-namespaces-title">
        <h2 id="hub-namespaces-title">Machine addresses</h2><p>Create a username namespace, then assign an encrypted host an address such as <code>username:host</code>. Names are permanent and do not create sign-in accounts.</p>
        {namespaces.length > 0 && <ul className="hub-namespace-list" aria-label="Username namespaces">{namespaces.map((entry) => <li key={entry.username}>{entry.username}</li>)}</ul>}
        <form className="hub-namespace-form" onSubmit={(event) => { event.preventDefault(); void action("namespace", async () => {
          const target = parseMachineAddress(`${username.trim()}:machine`).username;
          await session.request("namespaces", "POST", { username: target });
          setUsername(""); setNotice(`Username namespace ${target} created.`); await refreshAfterChange();
        }); }}>
          <label htmlFor="hub-namespace-username">New username</label><input id="hub-namespace-username" value={username} onChange={(event) => setUsername(event.target.value)} required disabled={!!busy} maxLength={63} autoComplete="off" spellCheck={false} placeholder="username" />
          <p>Use lowercase letters, numbers, and internal hyphens. Start and end with a letter or number.</p>
          <button disabled={!!busy || !username.trim()}>{busy === "namespace" ? "Creating…" : "Create username namespace"}</button>
        </form>
      </section>}
      {loaded && hosts.length === 0 && <div className="hub-empty"><h2>No hosts connected yet</h2><p>Start a host connector and approve its pairing request below.</p></div>}
      <ul className="hub-hosts" aria-label="Enrolled hosts">
        {hosts.map((host) => <li key={host.host_id} className="hub-host hub-admin-host">
          <div className="hub-host-details"><h2>{host.machine_address ?? host.name}</h2>{host.machine_address && <p>{host.name}</p>}<p><span className={`hub-presence ${host.online ? "is-online" : ""}`}>{host.online ? "Online" : "Offline"}</span> · {host.secure_only ? "End-to-end encrypted" : host.access === "view" ? "View only" : "View and control"}</p><code>{host.host_id}</code>
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
          {host.secure_only && !host.machine_address && <MachineAddressForm host={host} namespaces={namespaces} busy={!!busy} on_assign={(target) => action(`address:${host.host_id}`, async () => {
            const address = parseMachineAddress(target);
            await session.request(`namespaces/${encodeURIComponent(address.username)}/machines/${encodeURIComponent(address.machine_name)}`, "POST", { host_id: host.host_id });
            setNotice(`Machine address ${target} assigned. It is permanent.`); await refreshAfterChange();
          })} />}
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

function MachineAddressForm({ host, namespaces, busy, on_assign }: {
  host: HubHost;
  namespaces: HubNamespace[];
  busy: boolean;
  on_assign: (address: string) => Promise<void>;
}) {
  const [username, setUsername] = useState("");
  const [machine_name, setMachineName] = useState("");
  const selected_username = username || namespaces[0]?.username || "";
  return <details className="hub-admin-address"><summary>Assign a machine address</summary>
    {namespaces.length === 0 ? <p>Create a username namespace above before assigning this host an address.</p> : <form onSubmit={(event) => {
      event.preventDefault(); void on_assign(`${selected_username}:${machine_name.trim()}`);
    }}>
      <label htmlFor={`host-namespace-${host.host_id}`}>Username for {host.name}</label><select id={`host-namespace-${host.host_id}`} value={selected_username} onChange={(event) => setUsername(event.target.value)} disabled={busy} required>
        {namespaces.map((entry) => <option key={entry.username}>{entry.username}</option>)}
      </select>
      <label htmlFor={`host-machine-name-${host.host_id}`}>Machine name for {host.name}</label><input id={`host-machine-name-${host.host_id}`} value={machine_name} onChange={(event) => setMachineName(event.target.value)} required disabled={busy} maxLength={63} autoComplete="off" spellCheck={false} placeholder="host" />
      <p>This address is permanent. It will remain reserved if the host is revoked.</p>
      <button disabled={busy || !machine_name.trim()}>Assign {selected_username}:{machine_name.trim() || "host"}</button>
    </form>}
  </details>;
}
