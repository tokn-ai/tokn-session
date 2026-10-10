import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { NativeHubClient, nativeHubAuthenticate, nativeHubPair } from "./hubNativeClient";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
const host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43) };
const descriptor = { ...host, connection_id: "connection-one", endpoint: "https://hub.example/encrypted/machine" };
const handlers = new Map<string, (event: { payload: unknown }) => void>();
beforeEach(() => {
  handlers.clear();
  vi.mocked(listen).mockImplementation(async (name, handler) => { handlers.set(name, handler as (event: { payload: unknown }) => void); return () => { handlers.delete(name); }; });
  vi.mocked(invoke).mockImplementation(async (name) => {
    if (name === "hub_client_open") return descriptor;
    if (name === "hub_client_auth_start") return { auth_id: "auth-one", options: { publicKey: { challenge: "challenge" } } };
    return undefined;
  });
});
afterEach(() => { vi.clearAllMocks(); });
it("registers native event listeners before starting the pump and ignores retired connections", async () => {
  const client = await NativeHubClient.connect("https://hub.example", host);
  const changed = vi.fn();
  await client.listen("session-updated", changed);
  expect(handlers.has("hub-client-event")).toBe(true);
  expect(handlers.has("hub-client-state")).toBe(true);
  expect(invoke).toHaveBeenCalledWith("hub_client_listen", { connection_id: descriptor.connection_id });
  handlers.get("hub-client-event")!({ payload: { connection_id: "old", event: "session-updated", payload: { revision: "old" } } });
  expect(changed).not.toHaveBeenCalled();
  handlers.get("hub-client-event")!({ payload: { connection_id: descriptor.connection_id, event: "session-updated", payload: { revision: "1" } } });
  expect(changed).toHaveBeenCalledWith({ payload: { revision: "1" } });
  client.close(); expect(handlers.size).toBe(0);
});
it("drops late native responses after machine switch", async () => {
  const client = await NativeHubClient.connect("https://hub.example", host);
  let resolve!: (value: unknown) => void;
  vi.mocked(invoke).mockImplementation(async (name) => name === "hub_client_request" ? new Promise((done) => { resolve = done; }) : undefined);
  const result = client.invoke("list_sessions");
  await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("hub_client_request", expect.any(Object)));
  client.close(); resolve({ sessions: ["old"] });
  await expect(result).rejects.toThrow("disconnected");
});
it("closes native connections and removes listeners when pump startup fails", async () => {
  const client = await NativeHubClient.connect("https://hub.example", host);
  vi.mocked(invoke).mockRejectedValueOnce(new Error("host offline"));
  await expect(client.listen("session-updated", vi.fn())).rejects.toThrow("host offline");
  expect(handlers.size).toBe(0);
  await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("hub_client_close", { connection_id: descriptor.connection_id }));
  await expect(client.invoke("list_sessions")).rejects.toThrow("disconnected");
});
it("cancels the same pending native passkey channel when browser credential work is aborted", async () => {
  const controller = new AbortController();
  const get = vi.fn().mockImplementation((_options, signal: AbortSignal) => new Promise((_resolve, reject) => signal.addEventListener("abort", () => reject(new Error("aborted")))));
  const authentication = nativeHubAuthenticate("https://hub.example", host, false, controller.signal, { create: vi.fn(), get });
  const rejected = expect(authentication).rejects.toThrow("aborted");
  await vi.waitFor(() => expect(get).toHaveBeenCalled());
  controller.abort(); await rejected;
  await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("hub_client_auth_cancel", { auth_id: "auth-one" }));
  expect(vi.mocked(invoke).mock.calls.some(([name]) => name === "hub_client_auth_finish")).toBe(false);
});
it("sends a verified machine pin to native pairing before persistence", async () => {
  await nativeHubPair("https://hub.example", host.host_id, "123456", host.host_public_key);
  expect(invoke).toHaveBeenCalledWith("hub_client_pair", { hub_url: "https://hub.example", host_id: host.host_id, code: "123456", expected_host_public_key: host.host_public_key });
});
it("keeps readable display metadata out of cryptographic IPC arguments", async () => {
  const named = { ...host, machine_address: "clouds:macbook", name: "MacBook" };
  const client = await NativeHubClient.connect("https://hub.example", named);
  expect(invoke).toHaveBeenCalledWith("hub_client_open", { hub_url: "https://hub.example", host_id: host.host_id, host_public_key: host.host_public_key });
  await nativeHubAuthenticate("https://hub.example", named, false, new AbortController().signal, { create: vi.fn(), get: vi.fn().mockResolvedValue({ credential: true }) });
  expect(invoke).toHaveBeenCalledWith("hub_client_auth_start", { hub_url: "https://hub.example", host_id: host.host_id, host_public_key: host.host_public_key, register: false });
  client.close();
});
