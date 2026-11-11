import { useId } from "react";
import type { HubAccessService } from "../lib/hubAccess";
import { hubHostLabel, machineReference } from "../lib/hubDeviceStore";
import { discoveryGroupKey } from "../lib/hubDiscovery";
import { viewerCapabilities } from "../lib/platform";
import type { DiscoveredMachine, ResolvedMachine, SavedHubHost } from "../lib/types";
import { useHubDiscovery, type HubDiscoveryGroupState } from "../lib/useHubDiscovery";
import { useHubPresence } from "../lib/useHubPresence";
import { ConnectionIcon, RefreshIcon, ServerIcon } from "./Icons";
import "./HubMachineGallery.css";

interface GalleryMachine {
  host_id: string;
  saved?: SavedHubHost;
  account?: ResolvedMachine;
  discovery?: DiscoveredMachine;
  online?: boolean;
  groups: Set<string>;
}

function galleryMachines(hosts: SavedHubHost[], account_machines: ResolvedMachine[], groups: HubDiscoveryGroupState[]): GalleryMachine[] {
  const machines = new Map<string, GalleryMachine>();
  function entry(host_id: string): GalleryMachine {
    const previous = machines.get(host_id);
    if (previous) return previous;
    const machine = { host_id, groups: new Set<string>() };
    machines.set(host_id, machine);
    return machine;
  }
  for (const saved of hosts) {
    const machine = entry(saved.host_id);
    machine.saved = saved;
    if (saved.discovery_group) machine.groups.add(discoveryGroupKey(saved.discovery_group));
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
      machine.groups.add(state.group_key);
    }
  }
  return [...machines.values()];
}

function groupLabels(groups: HubDiscoveryGroupState[]): Map<string, string> {
  const totals = new Map<string, number>();
  const ordinals = new Map<string, number>();
  for (const { group: { namespace } } of groups) totals.set(namespace, (totals.get(namespace) ?? 0) + 1);
  return new Map(groups.map(({ group_key, group: { namespace } }) => {
    const ordinal = (ordinals.get(namespace) ?? 0) + 1;
    ordinals.set(namespace, ordinal);
    return [group_key, totals.get(namespace) === 1 ? namespace : `${namespace} · ${ordinal}`];
  }));
}

export interface HubGalleryAccountRoster {
  hub_url: string;
  machines: ResolvedMachine[];
  unavailable?: boolean;
  on_retry: () => void;
}

interface HubMachineGalleryProps {
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

export function HubMachineGallery({ service, hosts, account_roster, busy, opening_host_id, on_open, on_pair, on_add, on_passkey, on_forget }: HubMachineGalleryProps) {
  const { groups, refresh_group } = useHubDiscovery(service, hosts);
  const capabilities = viewerCapabilities();
  const heading_id = useId();
  const machines = galleryMachines(hosts, account_roster?.machines ?? [], groups);
  const presence = useHubPresence(service, machines.map((machine) => machine.host_id));
  const group_labels = groupLabels(groups);

  return <section className="hub-gallery" aria-labelledby={heading_id}>
    <header className="hub-gallery__heading"><h2 id={heading_id}>Your machines</h2><span aria-label={`${machines.length} machines`}>{machines.length}</span></header>
    {account_roster?.unavailable && <p className="hub-feedback">
      <span role="status">Account machines unavailable.</span>{" "}
      <button type="button" onClick={account_roster.on_retry}>Retry account machines</button>
    </p>}
    <ul className="hub-machine-grid" aria-label="Machines">
      {machines.map((machine) => {
        const host = machine.saved;
        const candidate = machine.account ?? machine.discovery;
        const address = machine.account?.machine_address ?? machine.discovery?.machine_address ?? host?.machine_address;
        const name = machine.account?.name ?? host?.name ?? machine.discovery?.name;
        const label = address ?? name ?? (host ? hubHostLabel(host) : `Machine ${machine.host_id.slice(0, 8)}`);
        const status = presence.get(machine.host_id)!;
        const online = status.state === "online" || (status.state !== "offline" && machine.online === true);
        return <li className="hub-machine-card" key={machine.host_id}>
          <div className="hub-machine-card__top"><span className="hub-machine-card__icon"><ServerIcon /></span>
            <span className="hub-machine-card__trust">{host ? "Paired" : "Pairing required"}</span></div>
          <div className="hub-machine-card__identity"><h3>{label}</h3>
            {name && name !== label && <p>{name}</p>}
            <span className={`hub-machine-card__presence${status.state === "online" ? " is-online" : ""}`}>
              <span title={status.error ?? "Connector status reported by the Hub"}>
                {status.state === "checking" ? "Checking…" : status.state === "unavailable" ? "Status unavailable" : status.state === "online" ? "Online" : "Offline"}
              </span>
              {status.connector_version && <span className="hub-machine-card__version" title="Connector version">· v{status.connector_version}</span>}
            </span>
          </div>
          {(machine.account || machine.groups.size > 0) && <div className="hub-machine-card__groups" aria-label="Machine groups">
            {machine.account && <span>Account machine</span>}
            {[...machine.groups].map((key) => <span key={key}>{group_labels.get(key)}</span>)}
          </div>}
          <div className="hub-machine-card__actions">
            {host ? <button className="hub-machine-card__primary" type="button" disabled={busy}
              aria-label={`${capabilities.native_hub ? "Open" : "Sign in to"} ${label}`} onClick={() => on_open(host)}>
              {opening_host_id === host.host_id ? "Connecting…" : capabilities.native_hub ? "Open" : "Sign in"}
            </button> : candidate && <button className="hub-machine-card__primary" type="button" disabled={busy || !online}
              aria-label={`Pair ${label}`} onClick={() => on_pair({ ...candidate, online })}>Pair</button>}
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
      {groups.map((state) => {
        const label = group_labels.get(state.group_key)!;
        return <section className="hub-gallery-discovery__group" key={state.group_key} aria-label={`Discovery group ${label}`}>
          <div className="hub-gallery-discovery__chip"><span>{label}</span>
            <button type="button" disabled={busy || state.loading} onClick={() => refresh_group(state.group_key)}
              aria-label={`${state.error ? "Retry" : "Refresh"} discovery group ${label}`} title={state.error ? "Retry discovery" : "Refresh discovery"}><RefreshIcon /></button>
          </div>
          {state.loading && <span className="hub-gallery-discovery__hint" role="status">Discovering {label}…</span>}
          {state.error && <p className="hub-error" role="alert">{state.error}</p>}
          {!state.loading && !state.error && state.machines.length === 0 && <span className="hub-gallery-discovery__hint">Empty group</span>}
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
