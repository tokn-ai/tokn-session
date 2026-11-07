import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { BrowserHubAccess } from "./hubAccess";
import { HubDeviceStore, type DeviceStorage, type HubDeviceRecord } from "./hubDeviceStore";
import { authenticateHubHost, EncryptedHubClient, pairHubHost, readHubDiscoveryGroup } from "./hubEncryptedClient";
import type { CryptoApi } from "./hubCrypto";
vi.mock("./hubEncryptedClient", () => ({
  authenticateHubHost: vi.fn(), pairHubHost: vi.fn(), readHubDiscoveryGroup: vi.fn(), EncryptedHubClient: { connect: vi.fn() },
}));
const host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43) };
let next_identity = 0;
const crypto = { DeviceIdentity: { generate: () => {
  const key = `${next_identity++}`;
  return { public_key: () => key, free: vi.fn() };
} } } as unknown as CryptoApi;
let record: HubDeviceRecord | undefined;
const storage: DeviceStorage = { update: async (_, change) => { record = change(record); return record; } };
const signal = () => new AbortController().signal;
async function open() { return new BrowserHubAccess("https://hub.example", crypto, await HubDeviceStore.open("https://hub.example", crypto, storage)); }
beforeEach(() => {
  record = undefined; vi.clearAllMocks();
  vi.mocked(pairHubHost).mockResolvedValue(host);
  vi.mocked(authenticateHubHost).mockResolvedValue(undefined);
  vi.mocked(EncryptedHubClient.connect).mockResolvedValue({ close: vi.fn() } as unknown as EncryptedHubClient);
  vi.mocked(readHubDiscoveryGroup).mockRejectedValue(new Error("Discovery unavailable"));
});
afterEach(() => vi.unstubAllGlobals());
it("TOTP enrolls a passkey then signs in before allowing session reads", async () => {
  const service = await open();
  await service.pair(host.host_id, "123456", signal());
  expect(EncryptedHubClient.connect).not.toHaveBeenCalled();
  await service.connect(host, signal());
  expect(vi.mocked(authenticateHubHost).mock.calls.map((call) => call[4])).toEqual([true, false]);
  expect(vi.mocked(authenticateHubHost).mock.invocationCallOrder[1]).toBeLessThan(vi.mocked(EncryptedHubClient.connect).mock.invocationCallOrder[0]);
  await service.connect(host, signal());
  expect(authenticateHubHost).toHaveBeenCalledTimes(2);
  const next_tab = await open();
  await next_tab.connect(host, signal());
  expect(vi.mocked(authenticateHubHost).mock.calls.map((call) => call[4])).toEqual([true, false, false]);
  expect(record).not.toHaveProperty("device_secret");
});
it("canceled assertion never opens sessions and the next attempt requires login again", async () => {
  const service = await open();
  await service.pair(host.host_id, "123456", signal());
  vi.mocked(authenticateHubHost).mockResolvedValueOnce(undefined).mockRejectedValueOnce(new Error("Passkey canceled"));
  await expect(service.connect(host, signal())).rejects.toThrow("Passkey canceled");
  expect(EncryptedHubClient.connect).not.toHaveBeenCalled();
  await service.connect(host, signal());
  expect(vi.mocked(authenticateHubHost).mock.calls.map((call) => call[4])).toEqual([true, false, false]);
});
it("expired tab authorization requires another passkey", async () => {
  vi.useFakeTimers();
  try {
    const service = await open();
    await service.authenticate(host, false, signal());
    await service.connect(host, signal());
    vi.advanceTimersByTime(8 * 60 * 60 * 1000);
    await service.connect(host, signal());
    expect(authenticateHubHost).toHaveBeenCalledTimes(2);
  } finally { vi.useRealTimers(); }
});
it("disposing a tab destroys its private identity and prevents reuse", async () => {
  const store = await HubDeviceStore.open("https://hub.example", crypto, storage);
  const service = new BrowserHubAccess("https://hub.example", crypto, store);
  await service.authenticate(host, false, signal());
  service.dispose(); service.dispose();
  expect(store.identity.free).toHaveBeenCalledOnce();
  await expect(service.connect(host, signal())).rejects.toThrow("disconnected");
  expect(EncryptedHubClient.connect).not.toHaveBeenCalled();
});

it("learns a discovery group only from successful machine pairing and keeps candidates unpaired", async () => {
  const service = await open();
  const group = { namespace: "personal", secret_digest: "A".repeat(43) };
  const candidate = { host_id: "550e8400-e29b-41d4-a716-446655440001", name: "Laptop", online: true };
  const fetcher = vi.fn().mockResolvedValue(new Response(JSON.stringify({ ...group, machines: [candidate] }), { headers: { "Content-Type": "application/json" } }));
  vi.stubGlobal("fetch", fetcher);
  await expect(service.discover(group, signal())).rejects.toThrow("Pair a machine");
  expect(fetcher).not.toHaveBeenCalled();
  vi.mocked(pairHubHost).mockResolvedValueOnce({ ...host, discovery_group: group });
  await service.pair(host.host_id, "123456", signal());
  expect(await service.discover(group, signal())).toEqual({ ...group, machines: [candidate] });
  expect((await service.status()).hosts).toEqual([{ ...host, discovery_group: group }]);
  expect(authenticateHubHost).not.toHaveBeenCalled();
  expect(EncryptedHubClient.connect).not.toHaveBeenCalled();
});

it("refreshes a previously paired machine's group through its authenticated connection and preserves it on lookup errors", async () => {
  const service = await open();
  const group = { namespace: "personal", secret_digest: "A".repeat(43) };
  await service.pair(host.host_id, "123456", signal());
  vi.mocked(readHubDiscoveryGroup).mockResolvedValueOnce(group);
  await service.connect(host, signal());
  expect(vi.mocked(authenticateHubHost).mock.invocationCallOrder[1]).toBeLessThan(vi.mocked(readHubDiscoveryGroup).mock.invocationCallOrder[0]);
  expect((await service.status()).hosts[0].discovery_group).toEqual(group);
  await service.connect(host, signal());
  expect((await service.status()).hosts[0].discovery_group).toEqual(group);
  vi.mocked(readHubDiscoveryGroup).mockResolvedValueOnce(null);
  await service.connect(host, signal());
  expect((await service.status()).hosts[0]).not.toHaveProperty("discovery_group");
});

it("does not restore an opted-out group when adding a passkey from an older host snapshot", async () => {
  const service = await open();
  const grouped_host = { ...host, discovery_group: { namespace: "personal", secret_digest: "A".repeat(43) } };
  vi.mocked(pairHubHost).mockResolvedValueOnce(grouped_host);
  await service.pair(host.host_id, "123456", signal());
  vi.mocked(readHubDiscoveryGroup).mockResolvedValueOnce(null);
  await service.connect(grouped_host, signal());
  expect((await service.status()).hosts[0]).not.toHaveProperty("discovery_group");
  await service.authenticate(grouped_host, true, signal());
  expect((await service.status()).hosts[0]).not.toHaveProperty("discovery_group");
});
