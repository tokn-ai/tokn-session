import { afterEach, expect, it, vi } from "vitest";
import { EncryptedHubClient, authenticateHubHost, pairHubHost, type SocketFactory } from "./hubEncryptedClient";
import type { CryptoApi, DeviceIdentity } from "./hubCrypto";
import { encodeBase64Url } from "./hub";

const host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43) };
const identity: DeviceIdentity = { public_key: () => "D".repeat(43), export_secret: () => "S".repeat(43), free: () => {} };
const encode = (value: unknown) => new TextEncoder().encode(typeof value === "string" ? value : JSON.stringify(value));
const decode = (value: Uint8Array) => new TextDecoder().decode(value);
const bytes = (value: string) => encodeBase64Url(new TextEncoder().encode(value).buffer);
function crypto(): CryptoApi {
  return {
    DeviceIdentity: { generate: () => identity, from_secret: () => identity },
    ClientPairing: { start: () => ({ record: () => encode("pair"), free: vi.fn(), confirm: () => ({ record: () => encode("confirm"), free: vi.fn(), finish: () => JSON.stringify(host) }) }) },
    NoiseInitiator: { start: (_identity, pin) => ({ record: () => encode("noise"), free: vi.fn(), finish: (reply) => {
      if (decode(reply) !== "noise-reply") throw new Error("bad pin");
      return { encrypt_json: (message) => encode(message), decrypt_json: (record) => decode(record), remote_public_key: () => pin, free: vi.fn() };
    } }) },
  };
}
class TestSocket {
  binaryType = "";
  bufferedAmount = 0;
  onopen: ((event: Event) => void) | null = null;
  onclose: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  closed = false;
  messages: Record<string, unknown>[] = [];
  request?: Record<string, unknown>;
  constructor(readonly url: string, private respond: (socket: TestSocket, message: Record<string, unknown>) => void) { queueMicrotask(() => this.onopen?.(new Event("open"))); }
  send(record: Uint8Array) {
    const wire = decode(record);
    if (wire === "noise") { this.push("noise-reply"); return; }
    if (wire === "pair") { this.push("host-pair"); return; }
    if (wire === "confirm") { this.push("host-ack"); return; }
    const message = JSON.parse(wire) as Record<string, unknown>;
    this.messages.push(message);
    if (message.type === "device_request") this.request = message;
    this.respond(this, message);
  }
  push(value: unknown) { const data = encode(value); const buffer = new ArrayBuffer(data.length); new Uint8Array(buffer).set(data); this.onmessage?.(new MessageEvent("message", { data: buffer })); }
  close() { this.closed = true; }
  fail() { this.onclose?.(new Event("close")); }
}
function harness(respond: (socket: TestSocket, message: Record<string, unknown>) => void = (socket, message) => {
  if (message.type === "request_end") {
    const events = socket.request?.path === "/api/v1/events";
    socket.push({ type: "response", status: 200, content_type: events ? "text/event-stream" : "application/json" });
    socket.push({ type: "chunk", data: bytes(events ? "event: ready\ndata: {}\n\n" : '{"version":1}') });
    if (!events) socket.push({ type: "end" });
  }
}) {
  const sockets: TestSocket[] = [];
  const factory: SocketFactory = (url) => { const socket = new TestSocket(url, respond); sockets.push(socket); return socket as unknown as WebSocket; };
  return { sockets, factory };
}
afterEach(() => { vi.useRealTimers(); });

it("pairs directly through Hub and returns only a host-confirmed machine pin", async () => {
  const { factory, sockets } = harness();
  expect(await pairHubHost("https://hub.example", host.host_id, "123456", identity, crypto(), new AbortController().signal, factory)).toEqual(host);
  expect(sockets[0].url).toBe(`wss://hub.example/hub/v1/secure/${host.host_id}`);
  expect(sockets[0].closed).toBe(true);
});

it("keeps passkey start and finish on one encrypted channel bound to its own device", async () => {
  const { factory, sockets } = harness((socket, message) => {
    if (message.operation === "login_start") socket.push({ type: "auth_response", payload: { options: { publicKey: { challenge: "challenge" } } } });
    if (message.operation === "login_finish") socket.push({ type: "auth_response", payload: { authorized: true, registered: false, device_public_key: identity.public_key() } });
  });
  const get = vi.fn().mockResolvedValue({ id: "credential" });
  await authenticateHubHost("https://hub.example", host, identity, crypto(), false, new AbortController().signal, { create: vi.fn(), get }, factory);
  expect(sockets).toHaveLength(1);
  expect(sockets[0].messages).toEqual([
    { type: "auth_request", operation: "login_start", payload: {} },
    { type: "auth_request", operation: "login_finish", payload: { credential: { id: "credential" } } },
  ]);
  expect(get).toHaveBeenCalledWith({ challenge: "challenge" }, expect.any(AbortSignal));
  expect(sockets[0].closed).toBe(true);
});

it("fragments bounded requests and acknowledges response chunks", async () => {
  const { factory, sockets } = harness();
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, factory);
  await client.invoke("submit_session_input", { text: "x".repeat(70_000) });
  const socket = sockets[1];
  expect(socket.messages.filter((message) => message.type === "request_body")).toHaveLength(3);
  expect(socket.messages).toContainEqual({ type: "window", credits: 1 });
  expect(socket.closed).toBe(true);
  client.close();
});

it("rejects oversized requests before opening a socket and never retries failed input", async () => {
  const { factory, sockets } = harness();
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, factory);
  await expect(client.invoke("submit_session_input", { text: "x".repeat(1024 * 1024) })).rejects.toThrow("1 MiB");
  expect(sockets).toHaveLength(1);
  const broken = harness((socket, message) => { if (message.type === "request_end") socket.fail(); });
  const failed = new EncryptedHubClient("https://hub.example", host, identity, crypto(), broken.factory);
  await expect(failed.invoke("submit_session_input", { text: "hello" })).rejects.toThrow("delivery may be uncertain");
  expect(broken.sockets).toHaveLength(1);
  failed.close(); client.close();
});

it("parses fragmented SSE and closes live streams and pending requests when switching", async () => {
  const { factory, sockets } = harness();
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, factory);
  const changed = vi.fn();
  const stop = await client.listen("session-updated", changed);
  sockets[1].push({ type: "chunk", data: bytes('event: session-updated\ndata: {"revision":') });
  sockets[1].push({ type: "chunk", data: bytes('"1"}\n\n') });
  await vi.waitFor(() => expect(changed).toHaveBeenCalledWith({ payload: { revision: "1" } }));
  stop(); client.close();
  expect(sockets.every((socket) => socket.closed)).toBe(true);
  await expect(client.invoke("list_sessions")).rejects.toThrow("disconnected");
});

it("rejects relay record floods instead of retaining an unbounded response", async () => {
  const { factory } = harness((socket, message) => {
    if (message.type !== "request_end") return;
    for (let index = 0; index < 20; index++) socket.push({ type: "chunk", data: bytes("x") });
  });
  const client = new EncryptedHubClient("https://hub.example", host, identity, crypto(), factory);
  await expect(client.invoke("list_sessions")).rejects.toThrow("queue exceeds");
  client.close();
});
