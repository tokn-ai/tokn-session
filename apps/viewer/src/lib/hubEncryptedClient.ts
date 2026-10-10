import { credentialJson, creationOptions, decodeBase64Url, encodeBase64Url, requestOptions } from "./hub";
import type { CryptoApi, DeviceIdentity, NoiseChannel } from "./hubCrypto";
import { canonicalHubUrl, validateHost, type SavedHubHost } from "./hubDeviceStore";
import { eventFrameLimit, parseEvent, type ConnectionState, type UnlistenFn, type ViewerClient } from "./transport";
import { browserDirectPeer, browserSocket, disconnected, PeerRetiredError, RelayCarrier, type DirectPeer, type DirectPeerFactory, type RecordCarrier, type RecordTransport } from "./hubTransport";
import type { TransportState } from "./types";
import type { SocketFactory } from "./hubTransport";
export type { SocketFactory } from "./hubTransport";

const MAX_BODY = 1024 * 1024;
const MAX_CHUNK = 32 * 1024;
const MAX_RESPONSE = 128 * 1024 * 1024;
const REQUEST_TIMEOUT = 120_000;
type Message = { type: string; [field: string]: unknown };
type Handler = (event: { payload: unknown }) => void;
export interface PasskeyProvider {
  create(options: Parameters<typeof creationOptions>[0], signal: AbortSignal): Promise<Record<string, unknown>>;
  get(options: Parameters<typeof requestOptions>[0], signal: AbortSignal): Promise<Record<string, unknown>>;
}
export const browserPasskeys: PasskeyProvider = {
  async create(options, signal) {
    if (!navigator.credentials || !window.PublicKeyCredential) throw new Error("Passkeys require HTTPS (or localhost) and a supported browser.");
    const credential = await navigator.credentials.create({ publicKey: creationOptions(options), signal });
    if (!credential) throw new Error("No passkey was created.");
    return credentialJson(credential as PublicKeyCredential);
  },
  async get(options, signal) {
    if (!navigator.credentials || !window.PublicKeyCredential) throw new Error("Passkeys require HTTPS (or localhost) and a supported browser.");
    const credential = await navigator.credentials.get({ publicKey: requestOptions(options), signal });
    if (!credential) throw new Error("No passkey was selected.");
    return credentialJson(credential as PublicKeyCredential);
  },
};

const aborted = disconnected;
function socketUrl(hub_url: string, host_id: string): string {
  const url = new URL(canonicalHubUrl(hub_url));
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.pathname = `/hub/v1/secure/${encodeURIComponent(host_id)}`;
  return url.toString();
}

export async function pairHubHost(hub_url: string, host_id: string, code: string, identity: DeviceIdentity, crypto: CryptoApi, signal: AbortSignal, factory: SocketFactory = browserSocket): Promise<SavedHubHost> {
  const socket = await new RelayCarrier(socketUrl(hub_url, host_id), factory).open(signal);
  let pairing: ReturnType<CryptoApi["ClientPairing"]["start"]> | undefined;
  let confirmation: ReturnType<NonNullable<typeof pairing>["confirm"]> | undefined;
  try {
    pairing = crypto.ClientPairing.start(host_id, identity, code, Math.floor(Date.now() / 1000));
    socket.send(pairing.record());
    const reply = await socket.read(15_000);
    const current_pairing = pairing; pairing = undefined;
    confirmation = current_pairing.confirm(reply);
    socket.send(confirmation.record());
    const ack = await socket.read(15_000);
    const current_confirmation = confirmation; confirmation = undefined;
    const result = JSON.parse(current_confirmation.finish(ack)) as SavedHubHost;
    if (result.host_id !== host_id) throw new Error("Pairing confirmed a different machine.");
    return validateHost(result);
  } finally { socket.close(); pairing?.free(); confirmation?.free(); }
}

async function openChannel(host: SavedHubHost, identity: DeviceIdentity, crypto: CryptoApi, signal: AbortSignal, carrier: RecordCarrier): Promise<{ socket: RecordTransport; channel: NoiseChannel }> {
  validateHost(host);
  const socket = await carrier.open(signal);
  let initiator: ReturnType<CryptoApi["NoiseInitiator"]["start"]> | undefined;
  try {
    initiator = crypto.NoiseInitiator.start(identity, host.host_public_key);
    socket.send(initiator.record());
    const reply = await socket.read(10_000);
    const current = initiator; initiator = undefined;
    const channel = current.finish(reply);
    if (channel.remote_public_key() !== host.host_public_key) { channel.free(); throw new Error("Machine encryption key did not match its saved identity."); }
    return { socket, channel };
  } catch (error) { socket.close(); throw error; }
  finally { initiator?.free(); }
}
function receiveMessage(channel: NoiseChannel, record: Uint8Array): Message {
  const message = JSON.parse(channel.decrypt_json(record)) as Message;
  if (message.type === "error") throw new Error(String(message.message));
  return message;
}

/** Passkey ceremonies remain inside one host-authenticated Noise channel. */
export async function authenticateHubHost(hub_url: string, host: SavedHubHost, identity: DeviceIdentity, crypto: CryptoApi, register: boolean, signal: AbortSignal, provider: PasskeyProvider = browserPasskeys, factory: SocketFactory = browserSocket): Promise<void> {
  const { socket, channel } = await openChannel(host, identity, crypto, signal, new RelayCarrier(socketUrl(hub_url, host.host_id), factory));
  try {
    const flow = register ? "register" : "login";
    socket.send(channel.encrypt_json(JSON.stringify({ type: "auth_request", operation: `${flow}_start`, payload: {} })));
    const start = receiveMessage(channel, await socket.read(30_000));
    if (start.type !== "auth_response") throw new Error("Machine returned an unexpected passkey challenge.");
    const payload = start.payload as { options: { publicKey: Parameters<typeof creationOptions>[0] | Parameters<typeof requestOptions>[0] } };
    const credential = register
      ? await provider.create(payload.options.publicKey as Parameters<typeof creationOptions>[0], signal)
      : await provider.get(payload.options.publicKey as Parameters<typeof requestOptions>[0], signal);
    if (signal.aborted) throw aborted();
    socket.send(channel.encrypt_json(JSON.stringify({ type: "auth_request", operation: `${flow}_finish`, payload: { credential } })));
    const finish = receiveMessage(channel, await socket.read(30_000));
    if (finish.type !== "auth_response" || (finish.payload as { authorized?: boolean }).authorized !== true) {
      throw new Error("Machine did not confirm passkey authentication.");
    }
    const confirmed = finish.payload as { device_public_key?: string };
    if (confirmed.device_public_key && confirmed.device_public_key !== identity.public_key()) throw new Error("Machine authorized a different device identity.");
  } finally { socket.close(); channel.free(); }
}

export class EncryptedHubClient implements ViewerClient {
  readonly endpoint: string;
  private lifetime = new AbortController();
  private sockets = new Set<AbortController>();
  private listeners = new Map<string, Set<Handler>>();
  private close_handlers = new Set<() => void>();
  private started?: Promise<void>;
  private on_state: (state: ConnectionState) => void = () => {};
  private relay: RecordCarrier;
  private direct?: DirectPeer;
  private negotiating = false;
  private direct_retry?: ReturnType<typeof setTimeout>;
  private event_request?: { close: () => void };
  private transport: TransportState = { kind: "relay" };
  private on_transport: (transport: TransportState) => void = () => {};
  constructor(private hub_url: string, readonly host: SavedHubHost, private identity: DeviceIdentity, private crypto: CryptoApi, factory: SocketFactory = browserSocket, private direct_factory: DirectPeerFactory | undefined = browserDirectPeer) {
    this.hub_url = canonicalHubUrl(hub_url);
    this.endpoint = `${this.hub_url}/encrypted/${host.host_id}`;
    this.relay = new RelayCarrier(socketUrl(this.hub_url, host.host_id), factory);
  }
  static async connect(hub_url: string, host: SavedHubHost, identity: DeviceIdentity, crypto: CryptoApi, signal?: AbortSignal, factory: SocketFactory = browserSocket, direct_factory: DirectPeerFactory | undefined = browserDirectPeer): Promise<EncryptedHubClient> {
    const client = new EncryptedHubClient(hub_url, host, identity, crypto, factory, direct_factory);
    const abort = () => client.close();
    if (signal?.aborted) throw aborted();
    signal?.addEventListener("abort", abort, { once: true });
    try {
      const health = await client.exchange("GET", "health");
      if ((health as { version?: number }).version !== 1) throw new Error("Machine uses an unsupported viewer API version.");
      void client.preferDirect();
      return client;
    } catch (error) { client.close(); throw error; }
    finally { signal?.removeEventListener("abort", abort); }
  }
  private async negotiate(message: Message): Promise<Message> {
    const controller = new AbortController();
    const abort = () => controller.abort();
    this.lifetime.signal.addEventListener("abort", abort, { once: true });
    const timer = setTimeout(abort, 15_000);
    let connection: Awaited<ReturnType<typeof openChannel>> | undefined;
    try {
      if (this.lifetime.signal.aborted) throw aborted();
      connection = await openChannel(this.host, this.identity, this.crypto, controller.signal, this.relay);
      connection.socket.send(connection.channel.encrypt_json(JSON.stringify(message)));
      return receiveMessage(connection.channel, await connection.socket.read(15_000));
    } finally {
      clearTimeout(timer); this.lifetime.signal.removeEventListener("abort", abort);
      controller.abort(); connection?.socket.close(); connection?.channel.free();
    }
  }
  private setTransport(transport: TransportState): void {
    this.transport = transport; this.on_transport(transport);
  }
  private fallback(error: unknown): void {
    if (this.lifetime.signal.aborted || !this.direct) return;
    const peer = this.direct; this.direct = undefined;
    if (error instanceof PeerRetiredError) peer.retire(); else peer.close();
    this.setTransport({ kind: "relay", reason: error instanceof Error ? error.message : "Direct connection was interrupted." });
    this.event_request?.close();
    this.scheduleDirect();
  }
  private scheduleDirect(): void {
    if (!this.direct_factory || this.direct || this.negotiating || this.direct_retry || this.lifetime.signal.aborted) return;
    this.direct_retry = setTimeout(() => {
      this.direct_retry = undefined; void this.preferDirect();
    }, 30_000);
  }
  private async preferDirect(): Promise<void> {
    if (this.direct || this.negotiating || this.lifetime.signal.aborted) return;
    if (!this.direct_factory) {
      this.setTransport({ kind: "relay", reason: "WebRTC is unavailable in this browser." }); return;
    }
    clearTimeout(this.direct_retry); this.direct_retry = undefined; this.negotiating = true;
    let peer: DirectPeer | undefined;
    try {
      const config = await this.negotiate({ type: "direct_config_request" });
      if (config.type !== "direct_config" || !Array.isArray(config.ice_servers) || config.ice_servers.length > 8
        || !config.ice_servers.every((url) => typeof url === "string" && /^stun:[^\s@\u0000-\u001f\u007f]+$/.test(url) && new TextEncoder().encode(url).length <= 512)) {
        throw new Error("Machine returned invalid direct connection settings.");
      }
      peer = this.direct_factory(config.ice_servers as string[], this.lifetime.signal, (error) => {
        if (this.direct === peer) this.fallback(error);
      });
      const offer = await peer.offer();
      const answer = await this.negotiate({ type: "direct_offer", sdp: offer });
      if (answer.type !== "direct_answer" || typeof answer.sdp !== "string") throw new Error("Machine returned an invalid direct connection answer.");
      await peer.accept(answer.sdp);
      // An ICE connection alone grants no trust: authenticate the host/device again.
      const health = await this.exchange("GET", "health", undefined, peer);
      if ((health as { version?: number }).version !== 1) throw new Error("Direct machine uses an unsupported viewer API version.");
      if (this.lifetime.signal.aborted) throw aborted();
      this.direct = peer; this.setTransport({ kind: "direct" });
      this.event_request?.close();
    } catch (error) {
      peer?.close();
      if (!this.lifetime.signal.aborted) this.setTransport({ kind: "relay", reason: error instanceof Error ? error.message : "Direct connection could not be established." });
    } finally {
      this.negotiating = false; this.scheduleDirect();
    }
  }
  private async request(method: string, command: string, payload?: unknown, carrier: RecordCarrier = this.direct ?? this.relay) {
    if (this.lifetime.signal.aborted) throw aborted();
    const body = payload === undefined ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(payload));
    if (body.length > MAX_BODY) throw new Error("Viewer request exceeds 1 MiB.");
    if (!/^[a-z_]+$/.test(command)) throw new Error("Invalid viewer command.");
    if (this.sockets.size >= 24) throw new Error("Too many active viewer requests.");
    const controller = new AbortController();
    this.sockets.add(controller);
    const abort = () => controller.abort();
    this.lifetime.signal.addEventListener("abort", abort, { once: true });
    let socket: RecordTransport | undefined;
    let channel: NoiseChannel | undefined;
    let selected_carrier = carrier;
    const timer = command === "events" ? undefined : setTimeout(() => {
      socket?.close(new Error("Viewer request timed out; delivery may be uncertain.")); controller.abort();
    }, REQUEST_TIMEOUT);
    const close = () => { clearTimeout(timer); controller.abort(); socket?.close(); channel?.free(); channel = undefined; this.sockets.delete(controller); this.lifetime.signal.removeEventListener("abort", abort); };
    try {
      try {
        ({ socket, channel } = await openChannel(this.host, this.identity, this.crypto, controller.signal, selected_carrier));
      } catch (error) {
        if (!(error instanceof PeerRetiredError)) throw error;
        if (selected_carrier === this.direct) this.fallback(error);
        // Retirement fails before a channel exists, so no request was sent.
        selected_carrier = this.relay;
        ({ socket, channel } = await openChannel(this.host, this.identity, this.crypto, controller.signal, selected_carrier));
      }
      socket.send(channel.encrypt_json(JSON.stringify({ type: "device_request", method, path: `/api/v1/${command}` })));
      for (let offset = 0; offset < body.length; offset += MAX_CHUNK) {
        socket.send(channel.encrypt_json(JSON.stringify({ type: "request_body", data: encodeBase64Url(body.slice(offset, offset + MAX_CHUNK).buffer) })));
      }
      socket.send(channel.encrypt_json(JSON.stringify({ type: "request_end" })));
      const response = receiveMessage(channel, await socket.read(REQUEST_TIMEOUT));
      if (response.type !== "response" || typeof response.status !== "number") throw new Error("Machine returned an invalid response.");
      const type = typeof response.content_type === "string" ? response.content_type.split(";")[0].trim() : "";
      if (!(response.status === 204 && !type) && type !== (command === "events" ? "text/event-stream" : "application/json")) throw new Error("Machine returned an unsupported response type.");
      return { socket, channel, response, close, carrier: selected_carrier };
    } catch (error) { close(); if (selected_carrier === this.direct) this.fallback(error); throw error; }
  }
  private async exchange(method: string, command: string, payload?: unknown, carrier?: RecordCarrier): Promise<unknown> {
    const request = await this.request(method, command, payload, carrier);
    const decoder = new TextDecoder();
    let body = "";
    let total = 0;
    try {
      while (true) {
        const message = await this.receive(request);
        if (message.type === "end") break;
        if (message.type !== "chunk" || typeof message.data !== "string") throw new Error("Machine returned an unexpected encrypted message.");
        const bytes = new Uint8Array(decodeBase64Url(message.data));
        total += bytes.length;
        if (bytes.length > MAX_CHUNK || total > MAX_RESPONSE) throw new Error("Viewer response exceeds its size limit.");
        body += decoder.decode(bytes, { stream: true });
        request.socket.send(request.channel.encrypt_json(JSON.stringify({ type: "window", credits: 1 })));
      }
      body += decoder.decode();
      const result: unknown = request.response.status === 204 && !body ? undefined : JSON.parse(body);
      if (Number(request.response.status) < 200 || Number(request.response.status) >= 300) throw new Error((result as { error?: string }).error ?? `Viewer returned ${request.response.status}`);
      return result;
    } finally { request.close(); }
  }
  private async receive(request: Awaited<ReturnType<EncryptedHubClient["request"]>>, timeout?: number): Promise<Message> {
    try { return receiveMessage(request.channel, await request.socket.read(timeout)); }
    catch (error) { if (request.carrier === this.direct) this.fallback(error); throw error; }
  }
  invoke<T>(command: string, payload: unknown = {}): Promise<T> { return this.exchange("POST", command, payload) as Promise<T>; }
  setStateListener(handler: (state: ConnectionState) => void): void { this.on_state = handler; }
  setTransportListener(handler: (transport: TransportState) => void): void { this.on_transport = handler; handler(this.transport); }
  private emit(name: string, payload: unknown): void { for (const handler of this.listeners.get(name) ?? []) handler({ payload }); }
  async listen<T>(name: string, handler: (event: { payload: T }) => void): Promise<UnlistenFn> {
    if (this.lifetime.signal.aborted) throw aborted();
    const handlers = this.listeners.get(name) ?? new Set<Handler>();
    const observer = handler as Handler;
    handlers.add(observer); this.listeners.set(name, handlers);
    this.started ??= this.pump();
    try { await this.started; } catch (error) { handlers.delete(observer); throw error; }
    return () => handlers.delete(observer);
  }
  private pump(): Promise<void> {
    return new Promise((ready, reject) => {
      void (async () => {
        let connected = false;
        let attempted = false;
        while (!this.lifetime.signal.aborted) {
          this.on_state(attempted ? "reconnecting" : "connecting"); attempted = true;
          let request: Awaited<ReturnType<EncryptedHubClient["request"]>> | undefined;
          try {
            request = await this.request("GET", "events");
            this.event_request = request;
            if (request.response.status !== 200) throw new Error("Live viewer connection was rejected.");
            const decoder = new TextDecoder();
            let buffer = "";
            while (!this.lifetime.signal.aborted) {
              const message = await this.receive(request, 45_000);
              if (message.type === "end") break;
              if (message.type !== "chunk" || typeof message.data !== "string") throw new Error("Unexpected live response message.");
              const bytes = new Uint8Array(decodeBase64Url(message.data));
              if (bytes.length > MAX_CHUNK) throw new Error("Live response chunk exceeds its limit.");
              buffer += decoder.decode(bytes, { stream: true }).replace(/\r/g, "");
              let end: number;
              while ((end = buffer.indexOf("\n\n")) !== -1) {
                if (end > eventFrameLimit(buffer)) throw new Error("Live event exceeds its size limit.");
                const frame = parseEvent(buffer.slice(0, end)); buffer = buffer.slice(end + 2);
                if (!frame) continue;
                if (frame.event === "ready") {
                  this.on_state("connected");
                  if (connected) { this.emit("transport-reconnected", {}); this.emit("relay-changed", { session_key: null, reset: true }); }
                  connected = true; ready();
                } else this.emit(frame.event, frame.payload);
              }
              if (buffer.length > eventFrameLimit(buffer)) throw new Error("Live event exceeds its size limit.");
              request.socket.send(request.channel.encrypt_json(JSON.stringify({ type: "window", credits: 1 })));
            }
          } catch (error) {
            if (request?.carrier === this.direct) this.fallback(error);
            // Only this read-only stream reconnects; requests are never replayed.
          }
          finally { if (this.event_request === request) this.event_request = undefined; request?.close(); }
          if (!this.lifetime.signal.aborted) {
            this.on_state("reconnecting");
            await new Promise<void>((resolve) => {
              const finish = () => { clearTimeout(timer); this.lifetime.signal.removeEventListener("abort", finish); resolve(); };
              const timer = setTimeout(finish, 1000);
              this.lifetime.signal.addEventListener("abort", finish, { once: true });
            });
          }
        }
        if (!connected) reject(aborted());
      })().catch(reject);
    });
  }
  onClose(handler: () => void): UnlistenFn { if (this.lifetime.signal.aborted) return () => {}; this.close_handlers.add(handler); return () => this.close_handlers.delete(handler); }
  async release(command: string, payload?: Record<string, unknown>): Promise<void> { if (!this.lifetime.signal.aborted) await this.invoke(command, payload); }
  close(): void {
    if (this.lifetime.signal.aborted) return;
    for (const handler of this.close_handlers) handler();
    clearTimeout(this.direct_retry); this.direct_retry = undefined;
    this.close_handlers.clear(); this.lifetime.abort(); this.direct?.close(); this.direct = undefined; this.relay.close(); this.listeners.clear();
  }
}
