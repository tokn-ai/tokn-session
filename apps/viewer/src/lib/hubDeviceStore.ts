import type { DeviceIdentity, CryptoApi } from "./hubCrypto";
import type { SavedHubHost } from "./types";
import { parseMachineAddress } from "./hubAddress";
export type { SavedHubHost } from "./types";
export interface HubDeviceRecord {
  version: 2;
  hub_url: string;
  hosts: SavedHubHost[];
  selected_host_id: string | null;
}
export interface LegacyHubDeviceRecord extends Omit<HubDeviceRecord, "version"> {
  version: 1;
  device_secret: string;
}
export type StoredHubDeviceRecord = HubDeviceRecord | LegacyHubDeviceRecord;
export interface DeviceStorage {
  update(hub_url: string, change: (record: StoredHubDeviceRecord | undefined) => HubDeviceRecord): Promise<HubDeviceRecord>;
}
const KEY = /^[A-Za-z0-9_-]{43}$/;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

export function canonicalHubUrl(value: string): string {
  const url = new URL(value);
  if (url.username || url.password || url.search || url.hash || url.pathname !== "/") {
    throw new Error("Enter the Hub origin without a path, credentials, query, or fragment.");
  }
  const loopback = url.hostname === "localhost" || url.hostname === "[::1]"
    || /^127\.(?:\d{1,3}\.){2}\d{1,3}$/.test(url.hostname);
  if (url.protocol !== "https:" && !(url.protocol === "http:" && loopback)) {
    throw new Error("A remote Hub requires HTTPS. HTTP is available on localhost for development.");
  }
  return url.origin;
}
export function validateHost(host: SavedHubHost): SavedHubHost {
  if (!UUID.test(host.host_id) || !KEY.test(host.host_public_key)) throw new Error("Invalid machine reference.");
  if (host.machine_address !== undefined) parseMachineAddress(host.machine_address);
  if (host.name !== undefined && (typeof host.name !== "string" || !host.name.trim() || new TextEncoder().encode(host.name).length > 128 || /[\u0000-\u001f\u007f-\u009f]/.test(host.name))) {
    throw new Error("Invalid saved machine name.");
  }
  return {
    host_id: host.host_id, host_public_key: host.host_public_key,
    ...(host.machine_address === undefined ? {} : { machine_address: host.machine_address }),
    ...(host.name === undefined ? {} : { name: host.name }),
  };
}
export function machineReference(host: SavedHubHost): string {
  return `${host.machine_address ?? host.host_id}@${host.host_public_key}`;
}
export function parseMachineReference(value: string): SavedHubHost {
  const match = /^([^@]+)@([^@]+)$/.exec(value.trim());
  if (!match) throw new Error("Paste the complete machine reference printed by the host connector.");
  return validateHost({ host_id: match[1], host_public_key: match[2] });
}
function validateRecord(record: StoredHubDeviceRecord, hub_url: string): HubDeviceRecord {
  if (!record || (record.version !== 1 && record.version !== 2) || record.hub_url !== hub_url
    || (record.version === 1 && !KEY.test(record.device_secret))
    || !Array.isArray(record.hosts) || record.hosts.length > 64) throw new Error("Saved device state is invalid. Restore it before reconnecting.");
  const hosts = record.hosts.map(validateHost);
  if (new Set(hosts.map((host) => host.host_id)).size !== hosts.length
    || new Set(hosts.flatMap((host) => host.machine_address ? [host.machine_address] : [])).size !== hosts.filter((host) => host.machine_address).length
    || (record.selected_host_id !== null && !hosts.some((host) => host.host_id === record.selected_host_id))) {
    throw new Error("Saved device state is invalid. Restore it before reconnecting.");
  }
  // Drop legacy private keys atomically while retaining independently pinned hosts.
  return { version: 2, hub_url, hosts, selected_host_id: record.selected_host_id };
}

function mergeHost(hosts: SavedHubHost[], host: SavedHubHost, require_existing = false): SavedHubHost[] {
  const previous = hosts.find((entry) => entry.host_id === host.host_id);
  if (!previous && require_existing) throw new Error("This machine was forgotten. Pair it again before saving its address.");
  if (previous && previous.host_public_key !== host.host_public_key) throw new Error("This machine's saved encryption key changed. Verify its identity before pairing again.");
  if (host.machine_address && hosts.some((entry) => entry.host_id !== host.host_id && entry.machine_address === host.machine_address)) {
    throw new Error("This address belongs to another remembered machine. Its saved identity was preserved.");
  }
  if (previous?.machine_address && host.machine_address && previous.machine_address !== host.machine_address) {
    throw new Error("This machine's saved address changed. Its saved identity was preserved.");
  }
  if (!previous && hosts.length >= 64) throw new Error("Remove a saved machine before adding another.");
  return previous ? hosts.map((entry) => entry.host_id === host.host_id ? { ...entry, ...host } : entry) : [...hosts, host];
}

export class IndexedDeviceStorage implements DeviceStorage {
  private database?: Promise<IDBDatabase>;
  private open(): Promise<IDBDatabase> {
    this.database ??= new Promise((resolve, reject) => {
      if (!globalThis.indexedDB) { reject(new Error("Browser storage is unavailable. Enable IndexedDB to remember this device.")); return; }
      const request = indexedDB.open("tokn-hub-devices", 1);
      request.onupgradeneeded = () => request.result.createObjectStore("devices", { keyPath: "hub_url" });
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(new Error("Could not open saved device storage."));
      request.onblocked = () => reject(new Error("Close older viewer tabs before updating device storage."));
    });
    return this.database;
  }
  async update(hub_url: string, change: (record: StoredHubDeviceRecord | undefined) => HubDeviceRecord): Promise<HubDeviceRecord> {
    const database = await this.open();
    return new Promise((resolve, reject) => {
      const transaction = database.transaction("devices", "readwrite");
      const store = transaction.objectStore("devices");
      const read = store.get(hub_url);
      let result: HubDeviceRecord;
      let failure: unknown;
      read.onsuccess = () => {
        try { result = change(read.result as StoredHubDeviceRecord | undefined); store.put(result); }
        catch (error) { failure = error; transaction.abort(); }
      };
      transaction.oncomplete = () => resolve(result);
      transaction.onerror = transaction.onabort = () => reject(failure ?? new Error("Could not save this device. Its trust state was preserved."));
    });
  }
}
const browser_storage = new IndexedDeviceStorage();

export class HubDeviceStore {
  private constructor(readonly hub_url: string, readonly identity: DeviceIdentity, private record: HubDeviceRecord, private storage: DeviceStorage) {}
  static async open(hub_url: string, crypto: CryptoApi, storage: DeviceStorage = browser_storage): Promise<HubDeviceStore> {
    const canonical = canonicalHubUrl(hub_url);
    // Each document owns a fresh identity. IndexedDB contains only host pins.
    const identity = crypto.DeviceIdentity.generate();
    try {
      const record = await storage.update(canonical, (existing) => existing ? validateRecord(existing, canonical) : {
        version: 2, hub_url: canonical, hosts: [], selected_host_id: null,
      });
      return new HubDeviceStore(canonical, identity, record, storage);
    } catch (error) { identity.free(); throw error; }
  }
  private disposed = false;
  dispose(): void { if (!this.disposed) { this.disposed = true; this.identity.free(); } }
  get hosts(): SavedHubHost[] { return this.record.hosts.map((host) => ({ ...host })); }
  get selected_host_id(): string | null { return this.record.selected_host_id; }
  async refresh(): Promise<void> {
    this.record = await this.storage.update(this.hub_url, (stored) => {
      const current = validateRecord(stored!, this.hub_url);
      return current;
    });
  }
  async saveHost(host: SavedHubHost): Promise<void> {
    host = validateHost(host);
    this.record = await this.storage.update(this.hub_url, (stored) => {
      const current = validateRecord(stored!, this.hub_url);
      return { ...current, hosts: mergeHost(current.hosts, host) };
    });
  }
  async rememberMetadata(host: SavedHubHost): Promise<void> {
    host = validateHost(host);
    if (!host.machine_address || !host.name) throw new Error("Machine address and name are required.");
    this.record = await this.storage.update(this.hub_url, (stored) => {
      const current = validateRecord(stored!, this.hub_url);
      // Metadata cannot recreate a pin forgotten in another tab, or change
      // which machine the user selected while directory lookup was pending.
      return { ...current, hosts: mergeHost(current.hosts, host, true) };
    });
  }
  async selectHost(host_id: string | null): Promise<void> {
    this.record = await this.storage.update(this.hub_url, (stored) => {
      const current = validateRecord(stored!, this.hub_url);
      if (host_id !== null && !current.hosts.some((host) => host.host_id === host_id)) throw new Error("This machine is not saved on this device.");
      return { ...current, selected_host_id: host_id };
    });
  }
  async forgetHost(host_id: string): Promise<void> {
    this.record = await this.storage.update(this.hub_url, (stored) => {
      const current = validateRecord(stored!, this.hub_url);
      return { ...current, hosts: current.hosts.filter((host) => host.host_id !== host_id), selected_host_id: current.selected_host_id === host_id ? null : current.selected_host_id };
    });
  }
}
