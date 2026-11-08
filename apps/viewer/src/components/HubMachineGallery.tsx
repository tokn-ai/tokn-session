import { useId } from "react";
import type { HubAccessService } from "../lib/hubAccess";
import { hubHostLabel, machineReference } from "../lib/hubDeviceStore";
import { discoveryGroupKey } from "../lib/hubDiscovery";
import { viewerCapabilities } from "../lib/platform";
import type { DiscoveryGroup, DiscoveredMachine, SavedHubHost } from "../lib/types";
import { useHubDiscovery, type HubDiscoveryGroupState } from "../lib/useHubDiscovery";
import { useHubPresence } from "../lib/useHubPresence";
import { ConnectionIcon, RefreshIcon, ServerIcon } from "./Icons";
import "./HubMachineGallery.css";

interface GalleryMachine {
  host_id: string;
  saved?: SavedHubHost;
  candidate?: DiscoveredMachine;
  online?: boolean;
  groups: Map<string, DiscoveryGroup>;
}

function galleryMachines(hosts: SavedHubHost[], groups: HubDiscoveryGroupState[]): GalleryMachine[] {
  const machines = new Map<string, GalleryMachine>();
  for (const host of hosts) {
    const memberships = new Map<string, DiscoveryGroup>();
    if (host.discovery_group) memberships.set(discoveryGroupKey(host.discovery_group), host.discovery_group);
    machines.set(host.host_id, { host_id: host.host_id, saved: host, groups: memberships });
  }
  for (const state of groups) {
    for (const candidate of state.machines) {
      let machine = machines.get(candidate.host_id);
      if (!machine) {
        machine = { host_id: candidate.host_id, candidate, groups: new Map() };
        machines.set(candidate.host_id, machine);
      }
      // Directory metadata may add presence and group labels, never a saved key
      // or authority. One online report permits a pairing attempt; an older
      // offline group result must not block the host's own verification.
      machine.online = machine.online === true || candidate.online;
      machine.groups.set(state.group_key, state.group);
    }
  }
  return [...machines.values()];
}

function groupLabel(group: DiscoveryGroup, namespaces: Map<string, number>): string {
  return (namespaces.get(group.namespace) ?? 0) > 1 ? `${group.namespace} · ${group.secret_digest.slice(0, 8)}` : group.namespace;
}

interface HubMachineGalleryProps {
  service: HubAccessService;
  hosts: SavedHubHost[];
  busy: boolean;
  opening_host_id?: string;
  on_open: (host: SavedHubHost) => void;
  on_pair: (machine: DiscoveredMachine) => void;
  on_add: () => void;
  on_passkey: (host: SavedHubHost) => void;
  on_forget: (host: SavedHubHost) => void;
}

export function HubMachineGallery({ service, hosts, busy, opening_host_id, on_open, on_pair, on_add, on_passkey, on_forget }: HubMachineGalleryProps) {
  const { groups, refresh_group } = useHubDiscovery(service, hosts);
  const capabilities = viewerCapabilities();
  const heading_id = useId();
  const machines = galleryMachines(hosts, groups);
  const presence = useHubPresence(service, machines.map((machine) => machine.host_id));
  const namespaces = new Map<string, number>();
  for (const { group } of groups) namespaces.set(group.namespace, (namespaces.get(group.namespace) ?? 0) + 1);

  return <section className="hub-gallery" aria-labelledby={heading_id}>
    <header className="hub-gallery__heading"><h2 id={heading_id}>Your machines</h2><span aria-label={`${machines.length} machines`}>{machines.length}</span></header>
    <ul className="hub-machine-grid" aria-label="Machines">
      {machines.map((machine) => {
        const host = machine.saved;
        const candidate = machine.candidate;
        const label = host ? hubHostLabel(host) : candidate!.name;
        const status = presence.get(machine.host_id)!;
        const online = status.state === "online" || (status.state !== "offline" && machine.online === true);
        return <li className="hub-machine-card" key={machine.host_id}>
          <div className="hub-machine-card__top"><span className="hub-machine-card__icon"><ServerIcon /></span>
            <span className="hub-machine-card__trust">{host ? "Paired" : "Pairing required"}</span></div>
          <div className="hub-machine-card__identity"><h3>{label}</h3>
            {host?.machine_address && host.name && host.name !== host.machine_address && <p>{host.name}</p>}
            <span className={`hub-machine-card__presence${status.state === "online" ? " is-online" : ""}`} title={status.error ?? "Connector status reported by the Hub"}>
              {status.state === "checking" ? "Checking…" : status.state === "unavailable" ? "Status unavailable" : status.state === "online" ? "Online" : "Offline"}
            </span>
          </div>
          {machine.groups.size > 0 && <div className="hub-machine-card__groups" aria-label="Discovery groups">{[...machine.groups].map(([key, group]) => <span key={key} title={`${group.namespace} · ${group.secret_digest}`}>{groupLabel(group, namespaces)}</span>)}</div>}
          <div className="hub-machine-card__actions">
            {host ? <button className="hub-machine-card__primary" type="button" disabled={busy}
              aria-label={`${capabilities.native_hub ? "Open" : "Sign in to"} ${label}`} onClick={() => on_open(host)}>
              {opening_host_id === host.host_id ? "Connecting…" : capabilities.native_hub ? "Open" : "Sign in"}
            </button> : <button className="hub-machine-card__primary" type="button" disabled={busy || !online}
              aria-label={`Pair ${label}`} onClick={() => on_pair({ ...candidate!, online })}>Pair</button>}
            {host && <details className="hub-machine-card__settings"><summary>Machine settings</summary><div>
              {capabilities.passkeys && <button type="button" disabled={busy} onClick={() => on_passkey(host)}>Add a passkey</button>}
              <span className="hub-field-label">Machine reference</span><code className="hub-machine-reference">{machineReference(host)}</code>
              <button type="button" className="hub-secondary" disabled={busy} onClick={() => on_forget(host)}>Forget {label}</button>
              <p>Forgetting removes this device’s reference. Authorization on the machine is unchanged.</p>
            </div></details>}
          </div>
        </li>;
      })}
      <li className="hub-machine-add"><button type="button" aria-label="Add machine" title="Add machine" disabled={busy} onClick={on_add}><span aria-hidden="true">+</span></button></li>
    </ul>
    <div className="hub-gallery-discovery" aria-label="Machine discovery"><div className="hub-gallery-discovery__label"><ConnectionIcon /><span>Discovery</span></div>
      {groups.length === 0 && <span className="hub-gallery-discovery__hint">{hosts.length === 0 ? "Pair a machine to find its group." : "Open a saved machine to find its group."}</span>}
      {groups.map((state) => <section className="hub-gallery-discovery__group" key={state.group_key}
        aria-label={`Discovery group ${state.group.namespace} ${state.group.secret_digest.slice(0, 8)}`}>
        <div className="hub-gallery-discovery__chip"><span title={state.group.secret_digest}>{groupLabel(state.group, namespaces)}</span>
          <button type="button" disabled={busy || state.loading} onClick={() => refresh_group(state.group_key)}
            aria-label={`${state.error ? "Retry" : "Refresh"} discovery group ${state.group.namespace} ${state.group.secret_digest.slice(0, 8)}`} title={state.error ? "Retry discovery" : "Refresh discovery"}><RefreshIcon /></button>
        </div>
        {state.loading && <span className="hub-gallery-discovery__hint" role="status">Discovering {groupLabel(state.group, namespaces)}…</span>}
        {state.error && <p className="hub-error" role="alert">{state.error}</p>}
        {!state.loading && !state.error && state.machines.length === 0 && <span className="hub-gallery-discovery__hint">Empty group</span>}
      </section>)}
    </div>
    <details className="hub-gallery-help"><summary>Set up discovery</summary><ol>
      <li>On the hosting computer, update Tokn and set a discovery namespace in <strong>Host this computer</strong>, or use the connector’s <code>--namespace</code> option.</li>
      <li>Use the same namespace and authenticator secret for computers in a group.</li>
      <li>Pair one known machine to learn its group. For an existing pairing, open or sign in to it, then return to Machines. Pair each additional machine separately.</li>
    </ol></details>
  </section>;
}
