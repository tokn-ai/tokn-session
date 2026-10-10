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
  await first.saveHost(first_host); await first.saveHost(second_host); await first.selectHost(second_host.host_id);
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
it("adds readable metadata to legacy pins without changing keys or erasing names on ordinary saves", async () => {
  const storage = new MemoryStorage();
  const store = await HubDeviceStore.open("https://hub.example", crypto, storage);
  await store.saveHost(first_host);
  const named = { ...first_host, machine_address: "clouds:macbook", name: "MacBook" };
  await store.saveHost(named);
  await store.saveHost(first_host);
  const reopened = await HubDeviceStore.open("https://hub.example", crypto, storage);
  expect(reopened.hosts).toEqual([named]);
  expect(machineReference(reopened.hosts[0])).toBe(`clouds:macbook@${first_host.host_public_key}`);
  await expect(store.saveHost({ ...second_host, machine_address: named.machine_address })).rejects.toThrow("another remembered machine");
  await expect(store.saveHost({ ...named, machine_address: "clouds:other" })).rejects.toThrow("saved address changed");
  expect(store.hosts).toEqual([named]);
});
it("metadata commits preserve selection and cannot resurrect a pin forgotten in another tab", async () => {
  const storage = new MemoryStorage();
  const first_tab = await HubDeviceStore.open("https://hub.example", crypto, storage);
  await first_tab.saveHost(first_host); await first_tab.saveHost(second_host); await first_tab.selectHost(second_host.host_id);
  const second_tab = await HubDeviceStore.open("https://hub.example", crypto, storage);
  const named = { ...first_host, machine_address: "clouds:macbook", name: "MacBook" };
  await first_tab.rememberMetadata(named);
  expect(first_tab.selected_host_id).toBe(second_host.host_id);
  await second_tab.forgetHost(first_host.host_id);
  await expect(first_tab.rememberMetadata(named)).rejects.toThrow("forgotten");
  const reopened = await HubDeviceStore.open("https://hub.example", crypto, storage);
  expect(reopened.hosts).toEqual([second_host]);
  expect(reopened.selected_host_id).toBe(second_host.host_id);
});
it("refreshes cross-tab alias pins and failed named saves preserve the selected machine", async () => {
  const storage = new MemoryStorage();
  const first_tab = await HubDeviceStore.open("https://hub.example", crypto, storage);
  const second_tab = await HubDeviceStore.open("https://hub.example", crypto, storage);
  await second_tab.saveHost({ ...first_host, machine_address: "clouds:macbook", name: "MacBook" });
  await second_tab.selectHost(first_host.host_id);
  expect(first_tab.hosts).toEqual([]);
  await first_tab.refresh();
  expect(first_tab.hosts[0].machine_address).toBe("clouds:macbook");
  await expect(first_tab.saveHost({ ...second_host, machine_address: "clouds:macbook", name: "Wrong machine" })).rejects.toThrow("another remembered");
  await first_tab.refresh();
  expect(first_tab.hosts).toEqual(second_tab.hosts);
  expect(first_tab.selected_host_id).toBe(first_host.host_id);
});
