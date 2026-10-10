import { loadHubCrypto, type CryptoApi } from "./hubCrypto";
import { HubDeviceStore, validateHost, canonicalHubUrl, type SavedHubHost } from "./hubDeviceStore";
import { EncryptedHubClient, pairHubHost, authenticateHubHost } from "./hubEncryptedClient";
import { NativeHubClient, nativeHubAuthenticate, nativeHubForget, nativeHubPair, nativeHubStatus, nativeHubResolve, nativeHubRememberMetadata } from "./hubNativeClient";
import { isDesktop, type ViewerClient } from "./transport";
import { parseMachineTarget, resolveBrowserMachine, validateResolvedMachine } from "./hubAddress";
import type { ResolvedMachine } from "./types";

export interface HubAccessBackend {
  readonly hub_url: string;
  status(): Promise<{ hosts: SavedHubHost[]; selected_host_id: string | null; device_public_key: string }>;
  pair(host_id: string, code: string, signal: AbortSignal, expected_host_public_key?: string, machine?: ResolvedMachine): Promise<SavedHubHost>;
  authenticate(host: SavedHubHost, register: boolean, signal: AbortSignal): Promise<void>;
  connect(host: SavedHubHost, signal: AbortSignal): Promise<ViewerClient>;
  forget(host_id: string): Promise<void>;
  resolve(machine_address: string, signal: AbortSignal): Promise<ResolvedMachine>;
  rememberMetadata(machine: ResolvedMachine, signal: AbortSignal): Promise<SavedHubHost>;
}
export interface HubAccessService extends HubAccessBackend {
  pairMachine(input: string, code: string, signal: AbortSignal): Promise<SavedHubHost>;
  loginMachine(input: string, signal: AbortSignal): Promise<SavedHubHost>;
}

function checkCancelled(signal: AbortSignal) {
  if (signal.aborted) throw new Error("Machine disconnected");
}

/** Directory names select a UUID; only pairing or a known pin establishes trust. */
export function withMachineAddressing(backend: HubAccessBackend): HubAccessService {
  async function target(input: string, signal: AbortSignal) {
    checkCancelled(signal);
    const parsed = parseMachineTarget(input);
    const resolved = parsed.machine_address ? await backend.resolve(parsed.machine_address, signal) : undefined;
    const host_id = resolved?.host_id ?? parsed.host_id!;
    const status = await backend.status();
    checkCancelled(signal);
    const alias_pin = status.hosts.find((host) => host.machine_address === parsed.machine_address && parsed.machine_address !== undefined);
    if (alias_pin && alias_pin.host_id !== host_id) throw new Error("This address now points to another machine. Its saved identity was preserved.");
    const remembered = status.hosts.find((host) => host.host_id === host_id);
    if (remembered && parsed.host_public_key && remembered.host_public_key !== parsed.host_public_key) throw new Error("Machine key differs from its remembered identity. Its saved key was preserved.");
    return { host_id, host_public_key: parsed.host_public_key ?? remembered?.host_public_key, resolved, remembered };
  }
  return Object.assign(backend, {
    async pairMachine(input: string, code: string, signal: AbortSignal) {
      const selected = await target(input, signal);
      const host = selected.remembered ?? await backend.pair(selected.host_id, code, signal, selected.host_public_key, selected.resolved);
      checkCancelled(signal);
      return selected.resolved ? backend.rememberMetadata(selected.resolved, signal) : host;
    },
    async loginMachine(input: string, signal: AbortSignal) {
      const selected = await target(input, signal);
      if (!selected.host_public_key) throw new Error("On a new device, passkey sign-in requires the complete machine reference with @ and its verified encryption key. Pair with an authenticator code first, or obtain the reference from a trusted device.");
      const host = validateHost({ host_id: selected.host_id, host_public_key: selected.host_public_key, ...selected.remembered });
      await backend.authenticate(host, false, signal);
      checkCancelled(signal);
      return selected.resolved ? backend.rememberMetadata(selected.resolved, signal) : host;
    },
  });
}

class BrowserHubAccess implements HubAccessBackend {
  constructor(readonly hub_url: string, private crypto: CryptoApi, private store: HubDeviceStore) {}
  async status() { await this.store.refresh(); return { hosts: this.store.hosts, selected_host_id: this.store.selected_host_id, device_public_key: this.store.identity.public_key() }; }
  async pair(host_id: string, code: string, signal: AbortSignal, expected_host_public_key?: string, machine?: ResolvedMachine) {
    const host = await pairHubHost(this.hub_url, host_id, code, this.store.identity, this.crypto, signal);
    if (signal.aborted) throw new Error("Machine disconnected");
    if (expected_host_public_key && host.host_public_key !== expected_host_public_key) throw new Error("Paired machine key differs from the reference. Verify the reference before opening it.");
    const resolved = machine ? await this.resolve(machine.machine_address, signal) : undefined;
    checkCancelled(signal);
    if (resolved && resolved.host_id !== host.host_id) throw new Error("Machine address changed during pairing. Its saved identity was preserved.");
    const named = resolved ? { ...host, machine_address: resolved.machine_address, name: resolved.name } : host;
    await this.store.saveHost(named);
    return named;
  }
  async authenticate(host: SavedHubHost, register: boolean, signal: AbortSignal) {
    validateHost(host);
    await authenticateHubHost(this.hub_url, host, this.store.identity, this.crypto, register, signal);
    if (signal.aborted) throw new Error("Machine disconnected");
    await this.store.saveHost(host);
  }
  async connect(host: SavedHubHost, signal: AbortSignal) {
    const client = await EncryptedHubClient.connect(this.hub_url, host, this.store.identity, this.crypto, signal);
    try { await this.store.selectHost(host.host_id); return client; }
    catch (error) { client.close(); throw error; }
  }
  forget(host_id: string) { return this.store.forgetHost(host_id); }
  resolve(machine_address: string, signal: AbortSignal) { return resolveBrowserMachine(this.hub_url, machine_address, signal); }
  async rememberMetadata(machine: ResolvedMachine, signal: AbortSignal) {
    const resolved = await this.resolve(machine.machine_address, signal);
    checkCancelled(signal);
    if (resolved.host_id !== machine.host_id) throw new Error("Machine address changed during pairing. Its saved identity was preserved.");
    const host = this.store.hosts.find((host) => host.host_id === resolved.host_id);
    if (!host) throw new Error("Pair this machine before saving its address.");
    const updated = { ...host, machine_address: resolved.machine_address, name: resolved.name };
    await this.store.rememberMetadata(updated);
    return updated;
  }
}
export async function openHubAccess(hub_url: string): Promise<HubAccessService> {
  const canonical = canonicalHubUrl(hub_url);
  if (isDesktop()) return withMachineAddressing({
    hub_url: canonical,
    status: () => nativeHubStatus(canonical),
    pair: async (host_id, code, signal, expected_host_public_key, machine) => { const host = await nativeHubPair(canonical, host_id, code, expected_host_public_key, machine?.machine_address); if (signal.aborted) throw new Error("Machine disconnected"); return host; },
    authenticate: async (host, register, signal) => { await nativeHubAuthenticate(canonical, host, register, signal); },
    connect: (host, signal) => NativeHubClient.connect(canonical, host, signal),
    forget: (host_id) => nativeHubForget(canonical, host_id),
    resolve: async (machine_address, signal) => { checkCancelled(signal); const resolved = await nativeHubResolve(canonical, machine_address); checkCancelled(signal); return validateResolvedMachine(resolved, machine_address); },
    rememberMetadata: async (machine, signal) => { checkCancelled(signal); const host = await nativeHubRememberMetadata(canonical, machine); checkCancelled(signal); return host; },
  });
  const crypto = await loadHubCrypto();
  return withMachineAddressing(new BrowserHubAccess(canonical, crypto, await HubDeviceStore.open(canonical, crypto)));
}
