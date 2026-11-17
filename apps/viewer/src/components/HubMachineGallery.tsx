import { useId } from "react";
import type { HubAccessService } from "../lib/hubAccess";
import { discoveryGroupKey } from "../lib/hubDiscovery";
import { viewerCapabilities } from "../lib/platform";
import type { DiscoveredMachine, ResolvedMachine, SavedHubHost } from "../lib/types";
import { useHubDiscovery, type HubDiscoveryGroupState } from "../lib/useHubDiscovery";
import { useHubPresence } from "../lib/useHubPresence";
import { ConnectionIcon, RefreshIcon } from "./Icons";
import { HubMachineCard, type GalleryMachine } from "./HubMachineCard";
import "./HubMachineGallery.css";

function galleryMachines(hosts: SavedHubHost[], account_machines: ResolvedMachine[], groups: HubDiscoveryGroupState[]): GalleryMachine[] {
  const machines = new Map<string, GalleryMachine>();
  function entry(host_id: string): GalleryMachine {
    const previous = machines.get(host_id);
    if (previous) return previous;
    const machine = { host_id, namespaces: new Set<string>() };
    machines.set(host_id, machine);
    return machine;
  }
  const active_groups = new Set(groups.map((state) => state.group_key));
  for (const saved of hosts) {
    const machine = entry(saved.host_id);
    machine.saved = saved;
    if (saved.discovery_group && active_groups.has(discoveryGroupKey(saved.discovery_group))) {
      machine.namespaces.add(saved.discovery_group.namespace);
    }
  }
  for (const account of account_machines) {
    const machine = entry(account.host_id);
    machine.account = account;
    machine.online = account.online;
  }
  for (const state of groups) {
    for (const candidate of state.machines) {
      const machine = entry(candidate.host_id);
      const saved_group = machine.saved?.discovery_group;
      if (!machine.discovery || (!machine.discovery.machine_address
        && (candidate.machine_address || (saved_group && discoveryGroupKey(saved_group) === state.group_key)))) {
        machine.discovery = candidate;
      }
      machine.online = machine.online === true || candidate.online;
      machine.namespaces.add(state.group.namespace);
    }
  }
  return [...machines.values()];
}

function discoveryByNamespace(groups: HubDiscoveryGroupState[]): Map<string, HubDiscoveryGroupState[]> {
  const namespaces = new Map<string, HubDiscoveryGroupState[]>();
  for (const state of groups) {
    const namespace = state.group.namespace;
    const previous = namespaces.get(namespace) ?? [];
    previous.push(state);
    namespaces.set(namespace, previous);
  }
  return namespaces;
}

export interface HubGalleryAccountRoster {
  hub_url: string;
  machines: ResolvedMachine[];
  unavailable?: boolean;
  on_retry: () => void;
}

interface HubMachineGalleryProps {
  visible?: boolean;
  resumable_host_id?: string;
  service: HubAccessService;
  hosts: SavedHubHost[];
  account_roster?: HubGalleryAccountRoster;
  busy: boolean;
  opening_host_id?: string;
  on_open: (host: SavedHubHost) => void;
  on_pair: (machine: DiscoveredMachine) => void;
  on_add: () => void;
  on_passkey: (host: SavedHubHost) => void;
  on_forget: (host: SavedHubHost) => void;
}

export function HubMachineGallery({ visible = true, resumable_host_id, service, hosts, account_roster, busy, opening_host_id, on_open, on_pair, on_add, on_passkey, on_forget }: HubMachineGalleryProps) {
  const { groups, refresh_group } = useHubDiscovery(service, hosts, visible);
  const capabilities = viewerCapabilities();
  const heading_id = useId();
  const machines = galleryMachines(hosts, account_roster?.machines ?? [], groups);
  const presence = useHubPresence(service, machines.map((machine) => machine.host_id), visible);
  if (!visible) return null;
  // Namespace grouping is presentation only. Each capability keeps its own
  // request, cached results and host trust checks.
  const discovery = discoveryByNamespace(groups);

  return <section className="hub-gallery" aria-labelledby={heading_id}>
    <header className="hub-gallery__heading"><h2 id={heading_id}>Your machines</h2><span aria-label={`${machines.length} machines`}>{machines.length}</span></header>
    {account_roster?.unavailable && <p className="hub-feedback">
      <span role="status">Account machines unavailable.</span>{" "}
      <button type="button" onClick={account_roster.on_retry}>Retry account machines</button>
    </p>}
    <ul className="hub-machine-grid" aria-label="Machines">
      {machines.map((machine) => <HubMachineCard key={machine.host_id} machine={machine}
        status={presence.get(machine.host_id)!} resumable={resumable_host_id === machine.host_id} busy={busy} opening={opening_host_id === machine.host_id}
        on_open={on_open} on_pair={on_pair} on_passkey={on_passkey} on_forget={on_forget} />)}
      <li className="hub-machine-add"><button type="button" disabled={busy} onClick={on_add}>
        <span aria-hidden="true">+</span>Add machine
      </button></li>
    </ul>
    <div className="hub-gallery-discovery" aria-label="Machine discovery"><div className="hub-gallery-discovery__label"><ConnectionIcon /><span>Discovery</span></div>
      {groups.length === 0 && <span className="hub-gallery-discovery__hint">{hosts.length === 0 ? "Pair a machine to discover more machines." : "Open a saved machine to discover more machines."}</span>}
      {[...discovery].map(([namespace, states]) => {
        const loading = states.some((state) => state.loading);
        const failed = states.filter((state) => state.error);
        const errors = [...new Set(failed.map((state) => state.error!))];
        const empty = states.every((state) => state.machines.length === 0);
        const action = `${errors.length ? "Retry" : "Refresh"} discovery for ${namespace}`;
        return <section className="hub-gallery-discovery__group" key={namespace} aria-label={`Discovery for ${namespace}`}>
          <div className="hub-gallery-discovery__chip"><span>{namespace}</span>
            <button type="button" disabled={busy || loading} onClick={() => { for (const state of states) refresh_group(state.group_key); }}
              aria-label={action} title={action}><RefreshIcon /></button>
          </div>
          {loading && <span className="hub-gallery-discovery__hint" role="status">Checking for machines…</span>}
          {errors.length > 0 && <p className="hub-error hub-gallery-discovery__error" role="alert">
            <span>{failed.length < states.length ? "Some discovery results are unavailable." : "Discovery is unavailable."}</span>
            {errors.map((error) => <span key={error}>{error}</span>)}
          </p>}
          {!loading && errors.length === 0 && empty && <span className="hub-gallery-discovery__hint">No other machines found.</span>}
        </section>;
      })}
    </div>
    <details className="hub-gallery-help"><summary>Set up discovery</summary><ol>
      {!capabilities.native_hub && <li>Sign in to your Hub account to see machines registered to it.</li>}
      <li>Register a host with your Hub account and a machine name to add it to your account.</li>
      <li>For discovery without signing in, pair a known machine on this Hub. If it’s already paired, open it and return to Machines.</li>
      <li>Pair each additional machine separately.</li>
    </ol></details>
  </section>;
}
