import { afterEach, expect, it, vi } from "vitest";
import { EncryptedHubClient, authenticateHubHost, pairHubHost, type SocketFactory } from "./hubEncryptedClient";
import type { CryptoApi, DeviceIdentity } from "./hubCrypto";
import { encodeBase64Url } from "./hub";
import { PeerRetiredError, RelayCarrier, type DirectPeer, type DirectPeerFactory } from "./hubTransport";

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
function viewerReply(socket: TestSocket): void {
  const events = socket.request?.path === "/api/v1/events";
  socket.push({ type: "response", status: 200, content_type: events ? "text/event-stream" : "application/json" });
  socket.push({ type: "chunk", data: bytes(events ? "event: ready\ndata: {}\n\n" : '{"version":1}') });
  if (!events) socket.push({ type: "end" });
}
function directHarness(options: { reject_input?: boolean; wrong_pin?: boolean; hold_health?: boolean } = {}) {
  const relay = harness((socket, message) => {
    if (message.type === "direct_config_request") socket.push({ type: "direct_config", ice_servers: ["stun:stun.example:3478"] });
    else if (message.type === "direct_offer") socket.push({ type: "direct_answer", sdp: "answer" });
    else if (message.type === "request_end") viewerReply(socket);
  });
  const direct = harness((socket, message) => {
    if (message.type !== "request_end") return;
    if (options.reject_input && socket.request?.path === "/api/v1/submit_session_input") socket.fail();
    else if (!options.hold_health || socket.request?.path !== "/api/v1/health") viewerReply(socket);
  });
  let fail!: (error: Error) => void;
  const carrier = new RelayCarrier("direct://machine", direct.factory);
  const peer: DirectPeer = {
    offer: vi.fn().mockResolvedValue("offer"), accept: vi.fn().mockResolvedValue(undefined), close: vi.fn(), retire: vi.fn(),
    open: async (signal) => {
      const records = await carrier.open(signal);
      if (!options.wrong_pin) return records;
      return { ...records, send: (record) => records.send(record), close: (error) => records.close(error), read: async () => encode("wrong-noise-reply") };
    },
  };
  const factory: DirectPeerFactory = vi.fn((_servers, _signal, failed) => { fail = failed; return peer; });
  return { relay, direct, factory, peer, interrupt: () => fail(new Error("Direct connection was interrupted.")) };
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

it("authenticates direct health before upgrading, then captures the direct carrier for new exchanges", async () => {
  const links = directHarness({ hold_health: true });
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const path = vi.fn(); client.setTransportListener(path);
  await vi.waitFor(() => expect(links.direct.sockets).toHaveLength(1));
  expect(path).toHaveBeenLastCalledWith({ kind: "relay" });
  await client.invoke("list_sessions");
  expect(links.relay.sockets.filter((socket) => socket.request?.path === "/api/v1/list_sessions")).toHaveLength(1);
  viewerReply(links.direct.sockets[0]);
  await vi.waitFor(() => expect(path).toHaveBeenLastCalledWith({ kind: "direct" }));
  await client.invoke("list_sessions");
  expect(links.direct.sockets[1].request?.path).toBe("/api/v1/list_sessions");
  expect(links.relay.sockets.filter((socket) => socket.request?.path === "/api/v1/list_sessions")).toHaveLength(1);
  expect(links.factory).toHaveBeenCalledWith(["stun:stun.example:3478"], expect.any(AbortSignal), expect.any(Function));
  expect(links.peer.accept).toHaveBeenCalledWith("answer");
  client.close(); expect(links.peer.close).toHaveBeenCalled();
});

it("keeps encrypted relay access when the direct host pin cannot authenticate", async () => {
  const links = directHarness({ wrong_pin: true });
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const path = vi.fn(); client.setTransportListener(path);
  await vi.waitFor(() => expect(path).toHaveBeenLastCalledWith({ kind: "relay", reason: "bad pin" }));
  await client.invoke("list_sessions");
  expect(links.relay.sockets[links.relay.sockets.length - 1]?.request?.path).toBe("/api/v1/list_sessions");
  expect(links.peer.close).toHaveBeenCalled(); client.close();
});

it("falls back for subsequent requests without replaying uncertain direct input", async () => {
  const links = directHarness({ reject_input: true });
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const path = vi.fn(); client.setTransportListener(path);
  await vi.waitFor(() => expect(path).toHaveBeenLastCalledWith({ kind: "direct" }));
  await expect(client.invoke("submit_session_input", { text: "send once" })).rejects.toThrow("delivery may be uncertain");
  expect(links.relay.sockets.some((socket) => socket.request?.path === "/api/v1/submit_session_input")).toBe(false);
  expect(path).toHaveBeenLastCalledWith({ kind: "relay", reason: expect.stringContaining("delivery may be uncertain") });
  await client.invoke("list_sessions");
  expect(links.relay.sockets[links.relay.sockets.length - 1]?.request?.path).toBe("/api/v1/list_sessions"); client.close();
});

it("restarts only the read-only live stream and emits recovery after a direct path fails", async () => {
  const links = directHarness();
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const path = vi.fn(); client.setTransportListener(path);
  await vi.waitFor(() => expect(path).toHaveBeenLastCalledWith({ kind: "direct" }));
  const recovered = vi.fn(); await client.listen("transport-reconnected", recovered);
  links.interrupt();
  await vi.waitFor(() => expect(recovered).toHaveBeenCalledOnce(), { timeout: 3000 });
  expect(path).toHaveBeenLastCalledWith({ kind: "relay", reason: "Direct connection was interrupted." });
  expect(links.relay.sockets[links.relay.sockets.length - 1]?.request?.path).toBe("/api/v1/events"); client.close();
});

it("retries direct negotiation in the background and cancels scheduled retries when the machine closes", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  const links = directHarness();
  vi.mocked(links.peer.offer).mockRejectedValueOnce(new Error("ICE attempt failed"));
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const path = vi.fn(); client.setTransportListener(path);
  await vi.waitFor(() => expect(path).toHaveBeenLastCalledWith({ kind: "relay", reason: "ICE attempt failed" }));
  await client.invoke("list_sessions");
  expect(links.relay.sockets[links.relay.sockets.length - 1]?.request?.path).toBe("/api/v1/list_sessions");
  await vi.advanceTimersByTimeAsync(30_000);
  expect(path).toHaveBeenLastCalledWith({ kind: "direct" }); expect(links.factory).toHaveBeenCalledTimes(2);
  links.interrupt(); client.close(); await vi.advanceTimersByTimeAsync(60_000);
  expect(links.factory).toHaveBeenCalledTimes(2);
});

it("routes an unsent request through relay when its direct peer retires", async () => {
  const links = directHarness();
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const path = vi.fn(); client.setTransportListener(path);
  await vi.waitFor(() => expect(path).toHaveBeenLastCalledWith({ kind: "direct" }));
  vi.spyOn(links.peer, "open").mockRejectedValueOnce(new PeerRetiredError());
  await client.invoke("submit_session_input", { text: "send once" });
  expect(links.direct.sockets.some((socket) => socket.request?.path === "/api/v1/submit_session_input")).toBe(false);
  expect(links.relay.sockets.filter((socket) => socket.request?.path === "/api/v1/submit_session_input")).toHaveLength(1);
  expect(links.peer.retire).toHaveBeenCalledOnce(); expect(links.peer.close).not.toHaveBeenCalled();
  expect(path).toHaveBeenLastCalledWith({ kind: "relay", reason: "Direct connection is renewing." }); client.close();
});

it("applies one request deadline including the wait for response headers", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  const { factory, sockets } = harness(() => {});
  const client = new EncryptedHubClient("https://hub.example", host, identity, crypto(), factory);
  const result = client.invoke("submit_session_input", { text: "send once" });
  const rejected = expect(result).rejects.toThrow("timed out; delivery may be uncertain");
  await vi.waitFor(() => expect(sockets[0].messages).toContainEqual({ type: "request_end" }));
  await vi.advanceTimersByTimeAsync(120_000); await rejected;
  expect(sockets).toHaveLength(1); expect(sockets[0].closed).toBe(true); client.close();
});
