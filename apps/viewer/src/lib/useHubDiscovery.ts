import { useCallback, useEffect, useRef, useState } from "react";
import { hubErrorMessage } from "./hub";
import type { HubAccessService } from "./hubAccess";
import { discoveryGroupKey, validateDiscoveryGroup, validateDiscoveryResult } from "./hubDiscovery";
import type { DiscoveryGroup, DiscoveredMachine, SavedHubHost } from "./types";

export interface HubDiscoveryGroupState {
  group_key: string;
  group: DiscoveryGroup;
  machines: DiscoveredMachine[];
  loading: boolean;
  error?: string;
}

interface GroupEntry {
  state: HubDiscoveryGroupState;
  request?: AbortController;
}

interface DiscoveryOwner {
  service: HubAccessService;
  entries: Map<string, GroupEntry>;
  disposed: boolean;
}

/** Directory metadata never promotes a candidate to a paired machine. */
export function useHubDiscovery(service: HubAccessService, hosts: SavedHubHost[]) {
  const known = new Map<string, DiscoveryGroup>();
  for (const host of hosts) {
    if (!host.discovery_group) continue;
    const group = validateDiscoveryGroup(host.discovery_group);
    known.set(discoveryGroupKey(group), group);
  }
  const group_keys = [...known.keys()].sort();
  const membership = JSON.stringify(group_keys);
  const context = useRef({ service, known });
  context.current = { service, known };
  const owner = useRef<DiscoveryOwner | null>(null);
  const [snapshot, setSnapshot] = useState<{ service: HubAccessService; groups: Map<string, HubDiscoveryGroupState> }>(() => ({ service, groups: new Map() }));

  const publish = useCallback((current: DiscoveryOwner) => {
    if (owner.current !== current || current.disposed) return;
    setSnapshot({ service: current.service, groups: new Map([...current.entries].map(([key, entry]) => [key, entry.state])) });
  }, []);

  const load = useCallback((current: DiscoveryOwner, group_key: string, group: DiscoveryGroup) => {
    const previous = current.entries.get(group_key);
    if (previous?.request) return;
    const request = new AbortController();
    const state = { group_key, group, machines: previous?.state.machines ?? [], loading: true };
    current.entries.set(group_key, { state, request });
    publish(current);
    const active = () => !request.signal.aborted && !current.disposed && owner.current === current
      && context.current.service === current.service && context.current.known.has(group_key)
      && current.entries.get(group_key)?.request === request;
    void Promise.resolve().then(async () => {
      if (!active()) return;
      const result = validateDiscoveryResult(await current.service.discover(group, request.signal), group);
      if (!active()) return;
      current.entries.set(group_key, { state: { group_key, group, machines: result.machines, loading: false } });
      publish(current);
    }).catch((error: unknown) => {
      if (!active()) return;
      current.entries.set(group_key, { state: { ...state, loading: false, error: hubErrorMessage(error) } });
      publish(current);
    });
  }, [publish]);

  useEffect(() => {
    const current: DiscoveryOwner = { service, entries: new Map(), disposed: false };
    owner.current = current;
    return () => {
      current.disposed = true;
      for (const entry of current.entries.values()) entry.request?.abort();
      if (owner.current === current) owner.current = null;
    };
  }, [service]);

  useEffect(() => {
    const current = owner.current;
    if (!current || current.service !== service) return;
    const groups = context.current.known;
    for (const [key, entry] of current.entries) {
      if (groups.has(key)) continue;
      entry.request?.abort();
      current.entries.delete(key);
    }
    for (const [key, group] of groups) {
      if (!current.entries.has(key)) load(current, key, group);
    }
    publish(current);
  }, [service, membership, load, publish]);

  const refresh_group = useCallback((group_key: string) => {
    const current = owner.current;
    const group = context.current.known.get(group_key);
    if (current && current.service === context.current.service && group) load(current, group_key, group);
  }, [load]);

  const groups: HubDiscoveryGroupState[] = group_keys.map((group_key) => {
    const state = snapshot.service === service ? snapshot.groups.get(group_key) : undefined;
    return state ?? { group_key, group: known.get(group_key)!, machines: [], loading: true };
  });
  return { groups, refresh_group };
}
