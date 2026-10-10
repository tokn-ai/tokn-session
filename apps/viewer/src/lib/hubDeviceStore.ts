import type { DeviceIdentity, CryptoApi } from "./hubCrypto";

export interface SavedHubHost {
  host_id: string;
  host_public_key: string;
}
export interface HubDeviceRecord {
  version: 1;
  hub_url: string;
  device_secret: string;
  hosts: SavedHubHost[];
  selected_host_id: string | null;
}
export interface DeviceStorage {
  update(hub_url: string, change: (record: HubDeviceRecord | undefined) => HubDeviceRecord): Promise<HubDeviceRecord>;
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
  return { host_id: host.host_id, host_public_key: host.host_public_key };
}
export function machineReference(host: SavedHubHost): string {
  return `${host.host_id}@${host.host_public_key}`;
}
export function parseMachineReference(value: string): SavedHubHost {
  const match = /^([^@]+)@([^@]+)$/.exec(value.trim());
  if (!match) throw new Error("Paste the complete machine reference printed by the host connector.");
  return validateHost({ host_id: match[1], host_public_key: match[2] });
}
function validateRecord(record: HubDeviceRecord, hub_url: string): HubDeviceRecord {
  if (record.version !== 1 || record.hub_url !== hub_url || !KEY.test(record.device_secret)
    || !Array.isArray(record.hosts) || record.hosts.length > 64) throw new Error("Saved device state is invalid. Restore it before reconnecting.");
  const hosts = record.hosts.map(validateHost);
  if (new Set(hosts.map((host) => host.host_id)).size !== hosts.length
    || (record.selected_host_id !== null && !hosts.some((host) => host.host_id === record.selected_host_id))) {
    throw new Error("Saved device state is invalid. Restore it before reconnecting.");
  }
  return { ...record, hosts };
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
  async update(hub_url: string, change: (record: HubDeviceRecord | undefined) => HubDeviceRecord): Promise<HubDeviceRecord> {
    const database = await this.open();
    return new Promise((resolve, reject) => {
      const transaction = database.transaction("devices", "readwrite");
      const store = transaction.objectStore("devices");
      const read = store.get(hub_url);
      let result: HubDeviceRecord;
      let failure: unknown;
      read.onsuccess = () => {
        try { result = change(read.result as HubDeviceRecord | undefined); store.put(result); }
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
    // Generate outside the transaction; an atomic read/write chooses an existing
    // identity if another tab enrolled this origin in the meantime.
    const candidate = crypto.DeviceIdentity.generate();
    try {
      const record = await storage.update(canonical, (existing) => existing ? validateRecord(existing, canonical) : {
        version: 1, hub_url: canonical, device_secret: candidate.export_secret(), hosts: [], selected_host_id: null,
      });
      return new HubDeviceStore(canonical, crypto.DeviceIdentity.from_secret(record.device_secret), record, storage);
    } finally { candidate.free(); }
  }
  get hosts(): SavedHubHost[] { return this.record.hosts.map((host) => ({ ...host })); }
  get selected_host_id(): string | null { return this.record.selected_host_id; }
  async saveHost(host: SavedHubHost): Promise<void> {
    validateHost(host);
    this.record = await this.storage.update(this.hub_url, (stored) => {
      const current = validateRecord(stored!, this.hub_url);
      const previous = current.hosts.find((entry) => entry.host_id === host.host_id);
      if (previous && previous.host_public_key !== host.host_public_key) throw new Error("This machine's saved encryption key changed. Verify its identity before pairing again.");
      if (!previous && current.hosts.length >= 64) throw new Error("Remove a saved machine before adding another.");
      return { ...current, hosts: previous ? current.hosts : [...current.hosts, host], selected_host_id: host.host_id };
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
