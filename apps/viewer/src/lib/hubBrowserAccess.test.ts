import { beforeEach, expect, it, vi } from "vitest";
import { BrowserHubAccess } from "./hubAccess";
import { HubDeviceStore, type DeviceStorage, type HubDeviceRecord } from "./hubDeviceStore";
import { authenticateHubHost, EncryptedHubClient, pairHubHost } from "./hubEncryptedClient";
import type { CryptoApi } from "./hubCrypto";
vi.mock("./hubEncryptedClient", () => ({
  authenticateHubHost: vi.fn(), pairHubHost: vi.fn(), EncryptedHubClient: { connect: vi.fn() },
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
});
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
