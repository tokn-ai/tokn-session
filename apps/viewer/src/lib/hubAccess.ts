import { loadHubCrypto, type CryptoApi } from "./hubCrypto";
import { HubDeviceStore, validateHost, canonicalHubUrl, type SavedHubHost } from "./hubDeviceStore";
import { EncryptedHubClient, pairHubHost, authenticateHubHost } from "./hubEncryptedClient";
import { NativeHubClient, nativeHubAuthenticate, nativeHubForget, nativeHubPair, nativeHubStatus } from "./hubNativeClient";
import { isDesktop, type ViewerClient } from "./transport";

export interface HubAccessService {
  readonly hub_url: string;
  status(): Promise<{ hosts: SavedHubHost[]; selected_host_id: string | null; device_public_key: string }>;
  pair(host_id: string, code: string, signal: AbortSignal, expected_host_public_key?: string): Promise<SavedHubHost>;
  authenticate(host: SavedHubHost, register: boolean, signal: AbortSignal): Promise<void>;
  connect(host: SavedHubHost, signal: AbortSignal): Promise<ViewerClient>;
  forget(host_id: string): Promise<void>;
}
class BrowserHubAccess implements HubAccessService {
  constructor(readonly hub_url: string, private crypto: CryptoApi, private store: HubDeviceStore) {}
  async status() { return { hosts: this.store.hosts, selected_host_id: this.store.selected_host_id, device_public_key: this.store.identity.public_key() }; }
  async pair(host_id: string, code: string, signal: AbortSignal, expected_host_public_key?: string) {
    const host = await pairHubHost(this.hub_url, host_id, code, this.store.identity, this.crypto, signal);
    if (signal.aborted) throw new Error("Machine disconnected");
    if (expected_host_public_key && host.host_public_key !== expected_host_public_key) throw new Error("Paired machine key differs from the reference. Verify the reference before opening it.");
    await this.store.saveHost(host);
    return host;
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
}
export async function openHubAccess(hub_url: string): Promise<HubAccessService> {
  const canonical = canonicalHubUrl(hub_url);
  if (isDesktop()) return {
    hub_url: canonical,
    status: () => nativeHubStatus(canonical),
    pair: async (host_id, code, signal, expected_host_public_key) => { const host = await nativeHubPair(canonical, host_id, code, expected_host_public_key); if (signal.aborted) throw new Error("Machine disconnected"); return host; },
    authenticate: async (host, register, signal) => { await nativeHubAuthenticate(canonical, host, register, signal); },
    connect: (host, signal) => NativeHubClient.connect(canonical, host, signal),
    forget: (host_id) => nativeHubForget(canonical, host_id),
  };
  const crypto = await loadHubCrypto();
  return new BrowserHubAccess(canonical, crypto, await HubDeviceStore.open(canonical, crypto));
}
