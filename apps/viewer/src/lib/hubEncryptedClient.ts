import { credentialJson, creationOptions, decodeBase64Url, requestOptions } from "./hub";
import type { CryptoApi, DeviceIdentity, NoiseChannel } from "./hubCrypto";
import { canonicalHubUrl, validateHost, type SavedHubHost } from "./hubDeviceStore";
import { VIEWER_API_VERSION, eventFrameLimit, parseEvent, type ConnectionState, type UnlistenFn, type ViewerClient } from "./transport";
import { browserDirectPeer, browserSocket, disconnected, PeerRetiredError, RelayCarrier, type DirectPeer, type DirectPeerFactory, type RecordCarrier, type RecordTransport } from "./hubTransport";
import type { DiscoveryGroup, TransportState } from "./types";
import { validateDiscoveryGroup } from "./hubDiscovery";
import type { SocketFactory } from "./hubTransport";
import { createUuid } from "./id";
import { HUB_CHUNK_BYTES, MAX_HUB_STREAMS, HubSession, type HubStream, type HubFrame } from "./hubSession";
export type { SocketFactory } from "./hubTransport";

const MAX_BODY = 1024 * 1024;
const MAX_RESPONSE = 128 * 1024 * 1024;
const REQUEST_TIMEOUT = 120_000;
type Message = HubFrame;
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

/** Refresh group metadata only through a channel pinned to an authorized host. */
export async function readHubDiscoveryGroup(hub_url: string, host: SavedHubHost, identity: DeviceIdentity, crypto: CryptoApi, signal: AbortSignal, factory: SocketFactory = browserSocket): Promise<DiscoveryGroup | null> {
  const controller = new AbortController();
  const abort = () => controller.abort();
  if (signal.aborted) abort();
  signal.addEventListener("abort", abort, { once: true });
  const deadline = setTimeout(abort, 5000);
  let opened: { socket: RecordTransport; channel: NoiseChannel } | undefined;
  try {
    opened = await openChannel(host, identity, crypto, controller.signal, new RelayCarrier(socketUrl(hub_url, host.host_id), factory));
    opened.socket.send(opened.channel.encrypt_json(JSON.stringify({ type: "discovery_request" })));
    const message = receiveMessage(opened.channel, await opened.socket.read(5000));
    if (controller.signal.aborted) throw aborted();
    return discoveryGroupFrom(message);
  } finally {
    opened?.socket.close(); opened?.channel.free();
    clearTimeout(deadline); signal.removeEventListener("abort", abort);
  }
}
function discoveryGroupFrom(message: HubFrame): DiscoveryGroup | null {
  if (message.type !== "discovery_response" || !("group" in message)) throw new Error("Machine returned invalid discovery metadata.");
  return message.group === null ? null : validateDiscoveryGroup(message.group);
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
  readonly subscription_prefix = createUuid();
  private lifetime = new AbortController();
  private sessions = new Map<RecordCarrier, Promise<HubSession>>();
  private opening = 0;
  private listeners = new Map<string, Set<Handler>>();
  private close_handlers = new Set<() => void>();
  private started?: Promise<void>;
  private on_state: (state: ConnectionState) => void = () => {};
  private relay: RecordCarrier;
  private direct?: DirectPeer;
  private negotiating = false;
  private direct_retry?: ReturnType<typeof setTimeout>;
  private event_request?: { stream: HubStream; carrier: RecordCarrier };
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
      if ((health as { version?: number }).version !== VIEWER_API_VERSION) throw new Error("Machine uses an unsupported viewer API version. Update the machine's viewer.");
      void client.preferDirect();
      return client;
    } catch (error) { client.close(); throw error; }
    finally { signal?.removeEventListener("abort", abort); }
  }
  private async control(header: HubFrame, carrier: RecordCarrier, timeout: number, signal = this.lifetime.signal): Promise<HubFrame> {
    if (signal.aborted) throw aborted();
    const deadline = Date.now() + timeout;
    const session = await this.session(carrier);
    if (signal.aborted || this.lifetime.signal.aborted) throw aborted();
    if (Date.now() >= deadline) throw new Error("Machine control request timed out.");
    const stream = session.open(header, new Uint8Array(), deadline);
    const abort = () => stream.close();
    signal.addEventListener("abort", abort, { once: true });
    try { return await stream.next(); }
    finally { signal.removeEventListener("abort", abort); stream.close(); }
  }
  private setTransport(transport: TransportState): void {
    this.transport = transport; this.on_transport(transport);
  }
  private fallback(error: unknown): void {
    if (this.lifetime.signal.aborted || !this.direct) return;
    const peer = this.direct; this.direct = undefined;
    this.removeSession(peer, error instanceof PeerRetiredError);
    if (error instanceof PeerRetiredError) peer.retire(); else peer.close();
    this.setTransport({ kind: "relay", reason: error instanceof Error ? error.message : "Direct connection was interrupted." });
    if (this.event_request?.carrier === peer) this.event_request.stream.close();
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
      const config = await this.control({ type: "direct_config_request" }, this.relay, 15_000);
      if (config.type !== "direct_config" || !Array.isArray(config.ice_servers) || config.ice_servers.length > 8
        || !config.ice_servers.every((url) => typeof url === "string" && /^stun:[^\s@\u0000-\u001f\u007f]+$/.test(url) && new TextEncoder().encode(url).length <= 512)) {
        throw new Error("Machine returned invalid direct connection settings.");
      }
      peer = this.direct_factory(config.ice_servers as string[], this.lifetime.signal, (error) => {
        if (this.direct === peer) this.fallback(error);
      });
      const offer = await peer.offer();
      const answer = await this.control({ type: "direct_offer", sdp: offer }, this.relay, 15_000);
      if (answer.type !== "direct_answer" || typeof answer.sdp !== "string") throw new Error("Machine returned an invalid direct connection answer.");
      await peer.accept(answer.sdp);
      // An ICE connection alone grants no trust: authenticate the host/device again.
      const health = await this.exchange("GET", "health", undefined, peer);
      if ((health as { version?: number }).version !== VIEWER_API_VERSION) throw new Error("Direct machine uses an unsupported viewer API version. Update the machine's viewer.");
      if (this.lifetime.signal.aborted) throw aborted();
      if (!this.sessions.has(peer)) throw new Error("Direct connection closed during verification.");
      this.direct = peer; this.setTransport({ kind: "direct" });
      if (this.event_request?.carrier === this.relay) this.event_request.stream.close();
      this.removeSession(this.relay, true);
    } catch (error) {
      if (peer) this.removeSession(peer, false);
      peer?.close();
      if (!this.lifetime.signal.aborted) this.setTransport({ kind: "relay", reason: error instanceof Error ? error.message : "Direct connection could not be established." });
    } finally {
      this.negotiating = false; this.scheduleDirect();
    }
  }
  private session(carrier: RecordCarrier): Promise<HubSession> {
    if (this.lifetime.signal.aborted) throw aborted();
    const existing = this.sessions.get(carrier);
    if (existing) return existing;
    const opening = openChannel(this.host, this.identity, this.crypto, this.lifetime.signal, carrier)
      .then(({ socket, channel }) => new HubSession(socket, channel, (error) => {
        if (this.sessions.get(carrier) !== opening) return;
        this.sessions.delete(carrier);
        if (this.direct === carrier) this.fallback(error);
      })).catch((error: unknown) => {
        if (this.sessions.get(carrier) === opening) this.sessions.delete(carrier);
        throw error;
      });
    this.sessions.set(carrier, opening);
    return opening;
  }

  private removeSession(carrier: RecordCarrier, drain: boolean): void {
    const session = this.sessions.get(carrier);
    this.sessions.delete(carrier);
    void session?.then((session) => drain ? session.retire() : session.close(), () => {});
  }

  async discoveryGroup(signal: AbortSignal): Promise<DiscoveryGroup | null> {
    return discoveryGroupFrom(await this.control({ type: "discovery_request" }, this.direct ?? this.relay, 5000, signal));
  }

  private async request(method: string, command: string, payload?: unknown, carrier: RecordCarrier = this.direct ?? this.relay) {
    if (this.lifetime.signal.aborted) throw aborted();
    if (this.opening >= MAX_HUB_STREAMS) throw new Error("Too many active viewer requests.");
    const body = payload === undefined ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(payload));
    if (body.length > MAX_BODY) throw new Error("Viewer request exceeds 1 MiB.");
    if (!/^[a-z_]+$/.test(command)) throw new Error("Invalid viewer command.");
    const deadline = Date.now() + REQUEST_TIMEOUT;
    this.opening++;
    let session: HubSession;
    try {
      try { session = await this.session(carrier); }
      catch (error) {
        if (!(error instanceof PeerRetiredError) || carrier === this.relay) throw error;
        // The carrier retired before any command was sent.
        if (carrier === this.direct) this.fallback(error);
        carrier = this.relay;
        session = await this.session(carrier);
      }
    } finally { this.opening--; }
    if (this.lifetime.signal.aborted) throw aborted();
    if (command === "events" && carrier !== (this.direct ?? this.relay)) throw new Error("Machine connection changed.");
    if (Date.now() >= deadline) throw new Error("Viewer request timed out.");
    const stream = session.open({ type: "device_request", method, path: `/api/v1/${command}` }, body, command === "events" ? undefined : deadline);
    if (command === "events") this.event_request = { stream, carrier };
    try {
      const response = await stream.next(REQUEST_TIMEOUT);
      if (response.type !== "response" || typeof response.status !== "number") throw new Error("Machine returned an invalid response.");
      const type = typeof response.content_type === "string" ? response.content_type.split(";")[0].trim() : "";
      if (!(response.status === 204 && !type) && type !== (command === "events" ? "text/event-stream" : "application/json")) throw new Error("Machine returned an unsupported response type.");
      return { stream, response, carrier };
    } catch (error) {
      stream.close();
      if (this.event_request?.stream === stream) this.event_request = undefined;
      throw error;
    }
  }
  private async exchange(method: string, command: string, payload?: unknown, carrier?: RecordCarrier): Promise<unknown> {
    const request = await this.request(method, command, payload, carrier);
    const decoder = new TextDecoder();
    let body = "";
    let total = 0;
    try {
      while (true) {
        const message = await request.stream.next();
        if (message.type === "end") break;
        if (message.type !== "chunk" || typeof message.data !== "string") throw new Error("Machine returned an unexpected encrypted message.");
        const bytes = new Uint8Array(decodeBase64Url(message.data));
        total += bytes.length;
        if (bytes.length > HUB_CHUNK_BYTES || total > MAX_RESPONSE) throw new Error("Viewer response exceeds its size limit.");
        body += decoder.decode(bytes, { stream: true });
        await request.stream.acknowledge();
      }
      body += decoder.decode();
      const result: unknown = request.response.status === 204 && !body ? undefined : JSON.parse(body);
      if (Number(request.response.status) < 200 || Number(request.response.status) >= 300) throw new Error((result as { error?: string }).error ?? `Viewer returned ${request.response.status}`);
      return result;
    } finally { request.stream.close(); }
  }
  invoke<T>(command: string, payload: unknown = {}): Promise<T> {
    if (command === "events") return Promise.reject(new Error("Use the live event listener for event streams."));
    return this.exchange("POST", command, payload) as Promise<T>;
  }
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
          const carrier = this.direct ?? this.relay;
          let request: Awaited<ReturnType<EncryptedHubClient["request"]>> | undefined;
          try {
            request = await this.request("POST", "events", { subscription_prefix: this.subscription_prefix }, carrier);
            if (request.response.status !== 200) throw new Error("Live viewer connection was rejected.");
            const decoder = new TextDecoder();
            let buffer = "";
            while (!this.lifetime.signal.aborted) {
              const message = await request.stream.next(45_000);
              if (message.type === "end") break;
              if (message.type !== "chunk" || typeof message.data !== "string") throw new Error("Unexpected live response message.");
              const bytes = new Uint8Array(decodeBase64Url(message.data));
              if (bytes.length > HUB_CHUNK_BYTES) throw new Error("Live response chunk exceeds its limit.");
              buffer += decoder.decode(bytes, { stream: true }).replace(/\r/g, "");
              let end: number;
              while ((end = buffer.indexOf("\n\n")) !== -1) {
                if (end > eventFrameLimit(buffer)) throw new Error("Live event exceeds its size limit.");
                const frame = parseEvent(buffer.slice(0, end)); buffer = buffer.slice(end + 2);
                if (!frame) continue;
                if (frame.event === "ready") {
                  this.on_state("connected");
                  if (connected) this.emit("transport-reconnected", {});
                  connected = true; ready();
                } else this.emit(frame.event, frame.payload);
              }
              if (buffer.length > eventFrameLimit(buffer)) throw new Error("Live event exceeds its size limit.");
              await request.stream.acknowledge();
            }
          } catch {
            // Only this read-only stream reconnects; requests are never replayed.
          }
          finally { if (this.event_request?.stream === request?.stream) this.event_request = undefined; request?.stream.close(); }
          if (!this.lifetime.signal.aborted) {
            this.on_state("reconnecting");
            if (carrier !== (this.direct ?? this.relay)) continue;
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
    for (const carrier of this.sessions.keys()) this.removeSession(carrier, false);
    this.close_handlers.clear(); this.lifetime.abort(); this.direct?.close(); this.direct = undefined; this.relay.close(); this.listeners.clear();
  }
}
