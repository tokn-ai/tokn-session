import { useEffect, useState } from "react";
import { hubErrorMessage } from "../lib/hub";
import type { HubAccessService } from "../lib/hubAccess";
import { discoveryGroupKey, validateDiscoveryGroup } from "../lib/hubDiscovery";
import type { DiscoveryGroup, DiscoveredMachine, SavedHubHost } from "../lib/types";
import { ConnectionIcon } from "./Icons";

interface GroupState {
  loading: boolean;
  machines: DiscoveredMachine[];
  error?: string;
}

function DiscoveryGroupCard({ group, service, paired, busy, on_pair }: {
  group: DiscoveryGroup;
  service: HubAccessService;
  paired: Set<string>;
  busy: boolean;
  on_pair: (machine: DiscoveredMachine) => void;
}) {
  const [state, setState] = useState<GroupState>({ loading: true, machines: [] });
  const [refresh, setRefresh] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setState((previous) => ({ ...previous, loading: true, error: undefined }));
    void service.discover(group, controller.signal).then((result) => {
      if (!controller.signal.aborted) setState({ loading: false, machines: result.machines });
    }).catch((error: unknown) => {
      if (!controller.signal.aborted) setState((previous) => ({ ...previous, loading: false, error: hubErrorMessage(error) }));
    });
    return () => controller.abort();
  }, [service, group.namespace, group.secret_digest, refresh]);

  return <section className="hub-discovery-group" aria-label={`Discovery group ${group.namespace} ${group.secret_digest.slice(0, 8)}`}>
    <header><div><h3>{group.namespace}</h3><span className="hub-field-label" title={group.secret_digest}>Group {group.secret_digest.slice(0, 8)}</span></div>
      <button type="button" disabled={state.loading || busy} onClick={() => setRefresh((revision) => revision + 1)} aria-label={`${state.error ? "Retry" : "Refresh"} discovery group ${group.namespace} ${group.secret_digest.slice(0, 8)}`}>{state.error ? "Retry" : "Refresh"}</button>
    </header>
    {state.loading && <p className="hub-muted" role="status">Discovering machines…</p>}
    {state.error && <p className="hub-error" role="alert">{state.error}</p>}
    {!state.loading && !state.error && state.machines.length === 0 && <p className="hub-muted">No machines are registered in this group.</p>}
    {state.machines.length > 0 && <ul aria-label={`Machines in ${group.namespace} ${group.secret_digest.slice(0, 8)}`}>{state.machines.map((machine) => <li key={machine.host_id}>
      <div className="hub-discovery-machine"><strong>{machine.name}</strong><span className={machine.online ? "hub-discovery-online" : "hub-muted"}>{machine.online ? "Online" : "Offline"}{paired.has(machine.host_id) ? " · Paired" : " · Pairing required"}</span></div>
      {!paired.has(machine.host_id) && <button type="button" disabled={busy || !machine.online} aria-label={`Pair ${machine.name}`} onClick={() => on_pair(machine)}>Pair</button>}
    </li>)}</ul>}
  </section>;
}

export function HubDiscoveryGroups({ service, hosts, busy, on_pair }: {
  service: HubAccessService;
  hosts: SavedHubHost[];
  busy: boolean;
  on_pair: (machine: DiscoveredMachine) => void;
}) {
  const groups = new Map<string, DiscoveryGroup>();
  for (const host of hosts) {
    if (!host.discovery_group) continue;
    const group = validateDiscoveryGroup(host.discovery_group);
    groups.set(discoveryGroupKey(group), group);
  }
  const paired = new Set(hosts.map((host) => host.host_id));
  return <section className="hub-discovery" aria-labelledby="discovery-groups-title">
    <header className="hub-discovery-heading"><div><ConnectionIcon /><h2 id="discovery-groups-title">Discovered groups</h2></div><span className="hub-discovery-count" aria-label={`${groups.size} discovery groups`}>{groups.size}</span></header>
    {groups.size === 0 && <div className="hub-empty-state hub-discovery-setup">
      <h3>No groups yet</h3>
      <p>{hosts.length === 0
        ? "Pair your first machine to discover its group."
        : "Open a saved machine to check for its group."}</p>
    </div>}
    {[...groups].map(([key, group]) => <DiscoveryGroupCard key={`${service.hub_url}/${key}`} group={group} service={service} paired={paired} busy={busy} on_pair={on_pair} />)}
    <details className="hub-discovery-help">
      <summary>Set up discovery</summary>
      <ol>
        <li>On the hosting computer, update Tokn and set a discovery namespace in the desktop app’s <strong>Host this computer</strong> dialog, or use the connector’s <code>--namespace</code> option.</li>
        <li>Use the same namespace and authenticator secret on the computers you want to discover together through this Hub.</li>
        <li>Pair one known machine using its address and authenticator code. If it is already paired, open or sign in to it, then return to Machines to refresh its group. Each additional machine requires its own pairing.</li>
      </ol>
    </details>
  </section>;
}
