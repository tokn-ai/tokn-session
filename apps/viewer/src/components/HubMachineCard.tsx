import { hubHostLabel } from "../lib/hubDeviceStore";
import { viewerCapabilities } from "../lib/platform";
import type { DiscoveredMachine, ResolvedMachine, SavedHubHost } from "../lib/types";
import type { MachinePresenceStatus } from "../lib/useHubPresence";
import { ChevronIcon, ConnectionIcon, ServerIcon } from "./Icons";
import { HubMachineCardSettings } from "./HubMachineCardSettings";

export interface GalleryMachine {
  host_id: string;
  saved?: SavedHubHost;
  account?: ResolvedMachine;
  discovery?: DiscoveredMachine;
  online?: boolean;
  namespaces: Set<string>;
}

interface HubMachineCardProps {
  machine: GalleryMachine;
  status: MachinePresenceStatus;
  busy: boolean;
  opening: boolean;
  resumable?: boolean;
  on_open: (host: SavedHubHost) => void;
  on_pair: (machine: DiscoveredMachine) => void;
  on_passkey: (host: SavedHubHost) => void;
  on_forget: (host: SavedHubHost) => void;
}

const presence_labels = {
  checking: "Checking…",
  unavailable: "Status unavailable",
  online: "Online",
  offline: "Offline",
};

export function HubMachineCard({ machine, status, busy, opening, resumable, on_open, on_pair, on_passkey, on_forget }: HubMachineCardProps) {
  const host = machine.saved;
  const candidate = machine.account ?? machine.discovery;
  const address = machine.account?.machine_address ?? machine.discovery?.machine_address ?? host?.machine_address;
  const name = machine.account?.name || host?.name || machine.discovery?.name;
  const label = address ?? name ?? (host ? hubHostLabel(host) : `Machine ${machine.host_id.slice(0, 8)}`);
  const namespaces = [...machine.namespaces].filter((namespace) => !address?.startsWith(`${namespace}:`));
  const capabilities = viewerCapabilities();
  const open_label = resumable ? "Resume" : capabilities.native_hub ? "Open" : "Sign in";
  const online = status.state === "online" || (status.state !== "offline" && machine.online === true);

  return <li className="hub-machine-card">
    <div className="hub-machine-card__body">
      <div className="hub-machine-card__top">
        <span className="hub-machine-card__icon"><ServerIcon /></span>
        <div className="hub-machine-card__status">
          <span className={`hub-machine-card__presence is-${status.state}`} title={status.error ?? "Connector status reported by the Hub"}>
            {presence_labels[status.state]}
          </span>
          {status.connector_version && <span title="Connector version">v{status.connector_version}</span>}
        </div>
        {host && <HubMachineCardSettings host={host} label={label} busy={busy} passkeys={capabilities.passkeys}
          on_passkey={on_passkey} on_forget={on_forget} />}
      </div>
      <div className="hub-machine-card__identity">
        <h3>{name || label}</h3>
        {name && address && address !== name && <p>{address}</p>}
      </div>
      {namespaces.length > 0 && <div className="hub-machine-card__meta">
        {namespaces.map((namespace) => <span key={namespace} title="Discovery group"><ConnectionIcon />{namespace}</span>)}
      </div>}
    </div>
    <div className="hub-machine-card__footer">
      <span className="hub-machine-card__trust">{host ? "Paired" : "Pairing required"}</span>
      {host ? <button className="hub-machine-card__primary" type="button" disabled={busy}
        aria-label={`${open_label === "Sign in" ? "Sign in to" : open_label} ${label}`} onClick={() => on_open(host)}>
        {opening ? "Connecting…" : open_label}<ChevronIcon />
      </button> : candidate && <button className="hub-machine-card__primary" type="button" disabled={busy || !online}
        aria-label={`Pair ${label}`} onClick={() => on_pair({ ...candidate, online })}>Pair<ChevronIcon /></button>}
    </div>
  </li>;
}
