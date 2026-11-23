import { afterEach, expect, it, vi } from "vitest";
import { EncryptedHubClient, authenticateHubHost, pairHubHost, readHubDiscoveryGroup, type SocketFactory } from "./hubEncryptedClient";
import type { CryptoApi, DeviceIdentity } from "./hubCrypto";
import { VIEWER_API_VERSION } from "./transport";
import { encodeBase64Url } from "./hub";
import { PeerRetiredError, RelayCarrier, type DirectPeer, type DirectPeerFactory } from "./hubTransport";

const viewerHealth = JSON.stringify({ version: VIEWER_API_VERSION });
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
  handshakes = 0;
  stream_id?: number;
  streams = new Map<number, Record<string, unknown>>();
  envelopes: Record<string, unknown>[] = [];
  messages: Record<string, unknown>[] = [];
  request?: Record<string, unknown>;
  constructor(readonly url: string, private respond: (socket: TestSocket, message: Record<string, unknown>) => void) { queueMicrotask(() => this.onopen?.(new Event("open"))); }
  send(record: Uint8Array) {
    const wire = decode(record);
    if (wire === "noise") { this.handshakes++; this.push("noise-reply"); return; }
    if (wire === "pair") { this.push("host-pair"); return; }
    if (wire === "confirm") { this.push("host-ack"); return; }
    const envelope = JSON.parse(wire) as Record<string, unknown>;
    this.envelopes.push(envelope);
    this.stream_id = envelope.type === "stream" ? Number(envelope.stream_id) : undefined;
    const message = this.stream_id ? envelope.frame as Record<string, unknown> : envelope;
    this.messages.push(message);
    if (this.stream_id && message.type === "device_request") this.streams.set(this.stream_id, message);
    this.request = this.stream_id ? this.streams.get(this.stream_id) : undefined;
    this.respond(this, message);
  }
  push(value: unknown, stream_id = this.stream_id) { const data = encode(stream_id ? { type: "stream", stream_id, frame: value } : value); const buffer = new ArrayBuffer(data.length); new Uint8Array(buffer).set(data); this.onmessage?.(new MessageEvent("message", { data: buffer })); }
  close() { this.closed = true; }
  fail() { this.onclose?.(new Event("close")); }
}
function harness(respond: (socket: TestSocket, message: Record<string, unknown>) => void = (socket, message) => {
  if (message.type === "request_end") {
    const events = socket.request?.path === "/api/v1/events";
    socket.push({ type: "response", status: 200, content_type: events ? "text/event-stream" : "application/json" });
    socket.push({ type: "chunk", data: bytes(events ? "event: ready\ndata: {}\n\n" : viewerHealth) });
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
  socket.push({ type: "chunk", data: bytes(events ? "event: ready\ndata: {}\n\n" : viewerHealth) });
  if (!events) socket.push({ type: "end" });
}
function directHarness(options: { reject_input?: boolean; wrong_pin?: boolean; hold_health?: boolean; hold_relay_input?: boolean } = {}) {
  const relay = harness((socket, message) => {
    if (message.type === "direct_config_request") socket.push({ type: "direct_config", ice_servers: ["stun:stun.example:3478"] });
    else if (message.type === "direct_offer") socket.push({ type: "direct_answer", sdp: "answer" });
    else if (message.type === "request_end" && !(options.hold_relay_input && socket.request?.path === "/api/v1/submit_session_input")) viewerReply(socket);
  });
  const direct = harness((socket, message) => {
    if (message.type !== "request_end") return;
    if (options.reject_input && ["/api/v1/submit_session_input", "/api/v1/submit_session_question_answers"].some((path) => path === socket.request?.path)) socket.fail();
    else if (!options.hold_health || socket.request?.path !== "/api/v1/health") viewerReply(socket);
  });
  let fail!: (error: Error) => void;
  const carrier = new RelayCarrier("direct://machine", direct.factory);
  const peer: DirectPeer = {
    offer: vi.fn().mockResolvedValue("offer"), accept: vi.fn().mockResolvedValue(undefined), close: vi.fn(), retire: vi.fn(),
    open: async (signal) => {
      const records = await carrier.open(signal);
      if (!options.wrong_pin) return records;
      return { ...records, setReceiveWindow: (count) => records.setReceiveWindow(count), waitWritable: () => records.waitWritable(), send: (record) => records.send(record), close: (error) => records.close(error), read: async () => encode("wrong-noise-reply") };
    },
  };
  const factory: DirectPeerFactory = vi.fn((_servers, _signal, failed) => { fail = failed; return peer; });
  return { relay, direct, factory, peer, interrupt: (error = new Error("Direct connection was interrupted.")) => fail(error) };
}
afterEach(() => { vi.useRealTimers(); });

it("stops at health when the machine uses the previous timeline protocol", async () => {
  const { factory, sockets } = harness((socket, message) => {
    if (message.type !== "request_end") return;
    socket.push({ type: "response", status: 200, content_type: "application/json" });
    socket.push({ type: "chunk", data: bytes(JSON.stringify({ version: VIEWER_API_VERSION - 1 })) });
    socket.push({ type: "end" });
  });
  await expect(EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, factory))
    .rejects.toThrow("unsupported viewer API version");
  expect(sockets.map((socket) => socket.request?.path)).toEqual(["/api/v1/health"]);
  expect(sockets[0].closed).toBe(true);
});

it("pairs directly through Hub and returns only a host-confirmed machine pin", async () => {
  const { factory, sockets } = harness();
  expect(await pairHubHost("https://hub.example", host.host_id, "123456", identity, crypto(), new AbortController().signal, factory)).toEqual(host);
  expect(sockets[0].url).toBe(`wss://hub.example/hub/v1/secure/${host.host_id}`);
  expect(sockets[0].closed).toBe(true);
});

it("reads discovery metadata through a pinned encrypted exchange and closes its socket", async () => {
  const group = { namespace: "personal", secret_digest: "A".repeat(43) };
  const { factory, sockets } = harness((socket, message) => {
    if (message.type === "discovery_request") socket.push({ type: "discovery_response", group });
  });
  expect(await readHubDiscoveryGroup("https://hub.example", host, identity, crypto(), new AbortController().signal, factory)).toEqual(group);
  expect(sockets[0].messages).toEqual([{ type: "discovery_request" }]);
  expect(sockets[0].closed).toBe(true);
});

it("bounds old-host discovery metadata waits and closes the abandoned encrypted exchange", async () => {
  vi.useFakeTimers();
  const { factory, sockets } = harness(() => {});
  const pending = readHubDiscoveryGroup("https://hub.example", host, identity, crypto(), new AbortController().signal, factory);
  const rejected = expect(pending).rejects.toThrow();
  await vi.advanceTimersByTimeAsync(5000);
  await rejected;
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

it("fragments bounded requests and acknowledges response chunks on the same channel", async () => {
  const { factory, sockets } = harness((socket, message) => {
    if (message.type === "request_end") {
      socket.push({ type: "response", status: 200, content_type: "application/json" });
      socket.push({ type: "chunk", data: bytes(viewerHealth) });
    }
    if (message.type === "window") socket.push({ type: "end" });
  });
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, factory);
  await client.invoke("submit_session_input", { text: "x".repeat(70_000) });
  const socket = sockets[0];
  expect(sockets).toHaveLength(1);
  expect(socket.messages.filter((message) => message.type === "request_body")).toHaveLength(3);
  expect(socket.messages).toContainEqual({ type: "window", credits: 1 });
  expect(socket.closed).toBe(false);
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
  expect(sockets[0].request).toMatchObject({ method: "POST", path: "/api/v1/events" });
  expect(sockets[0].messages).toContainEqual({
    type: "request_body", data: bytes(JSON.stringify({ subscription_prefix: client.subscription_prefix })),
  });
  sockets[0].push({ type: "chunk", data: bytes('event: session-updated\ndata: {"revision":') });
  sockets[0].push({ type: "chunk", data: bytes('"1"}\n\n') });
  await vi.waitFor(() => expect(changed).toHaveBeenCalledWith({ payload: { revision: "1" } }));
  stop(); client.close();
  expect(sockets.every((socket) => socket.closed)).toBe(true);
  await expect(client.invoke("list_sessions")).rejects.toThrow("disconnected");
});

it("rejects relay record floods instead of retaining an unbounded response", async () => {
  const { factory } = harness((socket, message) => {
    if (message.type !== "request_end") return;
    socket.push({ type: "response", status: 200, content_type: "application/json" });
    for (let index = 0; index < 200; index++) socket.push({ type: "chunk", data: bytes("x") });
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
  expect(links.direct.sockets).toHaveLength(1);
  expect(links.direct.sockets[0].request?.path).toBe("/api/v1/list_sessions");
  expect(links.relay.sockets.filter((socket) => socket.request?.path === "/api/v1/list_sessions")).toHaveLength(1);
  expect(links.relay.sockets.map((socket) => socket.handshakes)).toEqual([1]);
  expect(links.direct.sockets.map((socket) => socket.handshakes)).toEqual([1]);
  expect(links.relay.sockets[0].envelopes.filter((envelope) =>
    ["direct_config_request", "direct_offer"].includes(String((envelope.frame as Record<string, unknown>).type)),
  )).toHaveLength(2);
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
  expect(links.relay.sockets.flatMap((socket) => socket.messages)).toContainEqual({ type: "device_request", method: "POST", path: "/api/v1/list_sessions" });
  expect(links.peer.close).toHaveBeenCalled(); client.close();
});

it.each(["submit_session_input", "submit_session_question_answers"])("falls back without replaying uncertain direct %s", async (command) => {
  const links = directHarness({ reject_input: true });
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const path = vi.fn(); client.setTransportListener(path);
  await vi.waitFor(() => expect(path).toHaveBeenLastCalledWith({ kind: "direct" }));
  await expect(client.invoke(command, { text: "send once" })).rejects.toThrow("delivery may be uncertain");
  expect(links.relay.sockets.some((socket) => socket.request?.path === `/api/v1/${command}`)).toBe(false);
  expect(path).toHaveBeenLastCalledWith({ kind: "relay", reason: expect.stringContaining("delivery may be uncertain") });
  await client.invoke("list_sessions");
  expect(links.relay.sockets.flatMap((socket) => socket.messages)).toContainEqual({ type: "device_request", method: "POST", path: "/api/v1/list_sessions" }); client.close();
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
  expect(links.relay.sockets.flatMap((socket) => socket.messages)).toContainEqual({ type: "device_request", method: "POST", path: "/api/v1/list_sessions" });
  await vi.advanceTimersByTimeAsync(30_000);
  expect(path).toHaveBeenLastCalledWith({ kind: "direct" }); expect(links.factory).toHaveBeenCalledTimes(2);
  expect(links.relay.sockets.map((socket) => socket.handshakes)).toEqual([1]);
  expect(links.direct.sockets.map((socket) => socket.handshakes)).toEqual([1]);
  links.interrupt(); client.close(); await vi.advanceTimersByTimeAsync(60_000);
  expect(links.factory).toHaveBeenCalledTimes(2);
});

it("routes an unsent request through relay when its direct peer retires", async () => {
  const links = directHarness();
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const path = vi.fn(); client.setTransportListener(path);
  await vi.waitFor(() => expect(path).toHaveBeenLastCalledWith({ kind: "direct" }));
  links.interrupt(new PeerRetiredError());
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
  expect(sockets).toHaveLength(1); expect(sockets[0].closed).toBe(false);
  expect(sockets[0].messages).toContainEqual({ type: "cancel" }); client.close();
});

it("uses one handshake for health, discovery, an event stream and interleaved commands", async () => {
  const { factory, sockets } = harness((socket, message) => {
    if (message.type === "discovery_request") socket.push({ type: "discovery_response", group: null });
    if (message.type === "request_end" && ["/api/v1/events", "/api/v1/health"].includes(String(socket.request?.path))) viewerReply(socket);
  });
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, factory);
  const event = vi.fn();
  await client.listen("session-updated", event);
  await expect(client.discoveryGroup(new AbortController().signal)).resolves.toBeNull();
  const listing = client.invoke("list_sessions");
  const progress = client.invoke("get_session_index_progress");
  const socket = sockets[0];
  await vi.waitFor(() => expect(socket.messages.filter((message) => message.type === "request_end")).toHaveLength(4));
  expect(sockets).toHaveLength(1);
  expect(socket.handshakes).toBe(1);
  const opening = socket.envelopes.filter((envelope) => ["device_request", "discovery_request"].includes(
    String((envelope.frame as Record<string, unknown>).type),
  ));
  expect(opening.map((envelope) => envelope.stream_id)).toEqual([1, 2, 3, 4, 5]);
  const header = { type: "response", status: 200, content_type: "application/json" };
  socket.push(header, 5);
  socket.push(header, 4);
  socket.push({ type: "chunk", data: bytes('{"progress":1}') }, 5);
  socket.push({ type: "chunk", data: bytes("event: session-updated\ndata: 12\n\n") }, 2);
  socket.push({ type: "end" }, 5);
  await expect(progress).resolves.toEqual({ progress: 1 });
  socket.push({ type: "chunk", data: bytes('["s1"]') }, 4);
  socket.push({ type: "end" }, 4);
  await expect(listing).resolves.toEqual(["s1"]);
  expect(event).toHaveBeenCalledExactlyOnceWith({ payload: 12 });
  client.close();
});

it("shares a handshake in flight and limits the number of concurrent streams", async () => {
  const { factory, sockets } = harness(() => {});
  const client = new EncryptedHubClient("https://hub.example", host, identity, crypto(), factory);
  const results = Promise.allSettled(Array.from({ length: 16 }, () => client.invoke("list_sessions")));
  await vi.waitFor(() => expect(sockets[0]?.streams.size).toBe(16));
  await expect(client.invoke("list_sessions")).rejects.toThrow("Too many active");
  expect(sockets).toHaveLength(1);
  expect(sockets[0].handshakes).toBe(1);
  sockets[0].fail();
  expect((await results).every((result) => result.status === "rejected")).toBe(true);
  expect(sockets[0].streams.size).toBe(16);
  client.close();
});

it("discards a late discovery response after cancellation and keeps concurrent commands alive", async () => {
  const { factory, sockets } = harness(() => {});
  const client = new EncryptedHubClient("https://hub.example", host, identity, crypto(), factory);
  const abort = new AbortController();
  const discovery = client.discoveryGroup(abort.signal);
  const listing = client.invoke("list_sessions");
  await vi.waitFor(() => expect(sockets[0]?.messages).toContainEqual({ type: "request_end" }));
  abort.abort();
  await expect(discovery).rejects.toThrow("disconnected");
  const socket = sockets[0];
  await vi.waitFor(() => expect(socket.envelopes).toContainEqual({ type: "stream", stream_id: 1, frame: { type: "cancel" } }));
  socket.push({ type: "discovery_response", group: null }, 1);
  socket.push({ type: "response", status: 200, content_type: "application/json" }, 2);
  socket.push({ type: "chunk", data: bytes('["s2"]') }, 2);
  socket.push({ type: "end" }, 2);
  await expect(listing).resolves.toEqual(["s2"]);
  expect(socket.closed).toBe(false);
  expect(socket.handshakes).toBe(1);
  client.close();
});

it("accepts concurrent full response windows in one carrier burst", async () => {
  const { factory, sockets } = harness(() => {});
  const client = new EncryptedHubClient("https://hub.example", host, identity, crypto(), factory);
  const requests = Array.from({ length: 16 }, () => client.invoke("list_sessions"));
  await vi.waitFor(() => expect(sockets[0].messages.filter((frame) => frame.type === "request_end")).toHaveLength(16));
  const socket = sockets[0];
  for (const id of socket.streams.keys()) {
    socket.push({ type: "response", status: 200, content_type: "application/json" }, id);
    for (const text of ['"', "1", "2", "3", "4", "5", "6", '"']) socket.push({ type: "chunk", data: bytes(text) }, id);
    socket.push({ type: "end" }, id);
  }
  expect(await Promise.all(requests)).toEqual(Array(16).fill("123456"));
  expect(socket.handshakes).toBe(1);
  client.close();
});


it("moves events to the verified direct channel while an existing relay command finishes once", async () => {
  const links = directHarness({ hold_health: true, hold_relay_input: true });
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, links.relay.factory, links.factory);
  const reconnected = vi.fn();
  await client.listen("transport-reconnected", reconnected);
  const result = client.invoke("submit_session_input", { text: "one prompt" });
  const relay = links.relay.sockets[0];
  await vi.waitFor(() => expect(relay.request?.path).toBe("/api/v1/submit_session_input"));
  const commandId = relay.stream_id;
  await vi.waitFor(() => expect(links.direct.sockets[0].request?.path).toBe("/api/v1/health"));
  viewerReply(links.direct.sockets[0]);
  await vi.waitFor(() => expect(reconnected).toHaveBeenCalledOnce());
  expect(relay.closed).toBe(false);
  relay.push({ type: "response", status: 200, content_type: "application/json" }, commandId);
  relay.push({ type: "chunk", data: bytes('{"accepted":true}') }, commandId);
  relay.push({ type: "end" }, commandId);
  await expect(result).resolves.toEqual({ accepted: true });
  expect(relay.closed).toBe(true);
  expect(relay.messages.filter((frame) => frame.path === "/api/v1/submit_session_input")).toHaveLength(1);
  expect(links.direct.sockets[0].messages.some((frame) => frame.path === "/api/v1/submit_session_input")).toBe(false);
  client.close();
});

it("replaces a silent encrypted carrier after a missing live heartbeat", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  const { factory, sockets } = harness();
  const client = await EncryptedHubClient.connect("https://hub.example", host, identity, crypto(), undefined, factory);
  const reconnected = vi.fn();
  await client.listen("transport-reconnected", reconnected);
  await vi.advanceTimersByTimeAsync(46_001);
  expect(sockets.map((socket) => socket.handshakes)).toEqual([1, 1]);
  expect(sockets[0].closed).toBe(true);
  expect(reconnected).toHaveBeenCalledOnce();
  client.close();
});
