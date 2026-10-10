import { expect, it } from "vitest";
import { HubDeviceStore, canonicalHubUrl, machineReference, parseMachineReference, type DeviceStorage, type HubDeviceRecord } from "./hubDeviceStore";
import type { CryptoApi, DeviceIdentity } from "./hubCrypto";
class MemoryStorage implements DeviceStorage {
  records = new Map<string, HubDeviceRecord>();
  async update(key: string, change: (record: HubDeviceRecord | undefined) => HubDeviceRecord) {
    const record = change(this.records.get(key)); this.records.set(key, record); return record;
  }
}
let identities = 0;
const makeIdentity = (secret: string): DeviceIdentity => ({ public_key: () => secret, export_secret: () => secret, free: () => {} });
const crypto = { DeviceIdentity: { generate: () => makeIdentity(String.fromCharCode(65 + identities++).repeat(43)), from_secret: makeIdentity } } as unknown as CryptoApi;
const first_host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43) };
const second_host = { host_id: "550e8400-e29b-41d4-a716-446655440001", host_public_key: "I".repeat(43) };
it("persists one device per Hub and partitions host pins and selected machines", async () => {
  const storage = new MemoryStorage();
  const first = await HubDeviceStore.open("https://hub.example", crypto, storage);
  await first.saveHost(first_host); await first.saveHost(second_host);
  const reopened = await HubDeviceStore.open("https://hub.example/", crypto, storage);
  expect(reopened.identity.public_key()).toBe(first.identity.public_key());
  expect(reopened.hosts).toEqual([first_host, second_host]);
  expect(reopened.selected_host_id).toBe(second_host.host_id);
  const separate = await HubDeviceStore.open("https://other.example", crypto, storage);
  expect(separate.identity.public_key()).not.toBe(first.identity.public_key());
  expect(separate.hosts).toEqual([]);
  await reopened.forgetHost(second_host.host_id);
  expect(reopened.hosts).toEqual([first_host]); expect(reopened.selected_host_id).toBeNull();
});
it("refuses changed pins and corrupt existing trust without resetting device identity", async () => {
  const storage = new MemoryStorage();
  const store = await HubDeviceStore.open("https://hub.example", crypto, storage);
  await store.saveHost(first_host);
  await expect(store.saveHost({ ...first_host, host_public_key: "J".repeat(43) })).rejects.toThrow("encryption key changed");
  expect(store.hosts).toEqual([first_host]);
  storage.records.get("https://hub.example")!.device_secret = "invalid";
  await expect(HubDeviceStore.open("https://hub.example", crypto, storage)).rejects.toThrow("invalid");
  expect(storage.records.get("https://hub.example")!.device_secret).toBe("invalid");
});
it("requires a full independently obtained machine reference for passkey first use", () => {
  expect(parseMachineReference(machineReference(first_host))).toEqual(first_host);
  expect(() => parseMachineReference(first_host.host_id)).toThrow("complete machine reference");
  expect(() => canonicalHubUrl("http://remote.example")).toThrow("HTTPS");
  expect(() => canonicalHubUrl("https://hub.example/hosts")).toThrow("origin");
  expect(canonicalHubUrl("http://localhost:5559/")).toBe("http://localhost:5559");
});
