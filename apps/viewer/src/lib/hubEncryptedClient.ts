import { credentialJson, creationOptions, decodeBase64Url, encodeBase64Url, requestOptions } from "./hub";
import type { CryptoApi, DeviceIdentity, NoiseChannel } from "./hubCrypto";
import { canonicalHubUrl, validateHost, type SavedHubHost } from "./hubDeviceStore";
import { eventFrameLimit, parseEvent, type ConnectionState, type UnlistenFn, type ViewerClient } from "./transport";

const MAX_RECORD = 65_535;
const MAX_BODY = 1024 * 1024;
const MAX_CHUNK = 32 * 1024;
const MAX_RESPONSE = 128 * 1024 * 1024;
const MAX_QUEUE = 16;
const REQUEST_TIMEOUT = 120_000;
type Message = { type: string; [field: string]: unknown };
type Handler = (event: { payload: unknown }) => void;
export type SocketFactory = (url: string) => WebSocket;
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

function aborted(): Error { return new Error("Machine disconnected"); }
function socketUrl(hub_url: string, host_id: string): string {
  const url = new URL(canonicalHubUrl(hub_url));
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.pathname = `/hub/v1/secure/${encodeURIComponent(host_id)}`;
  return url.toString();
}

/** One ordered record reader. Bounded queues stop a relay from spooling bytes. */
class RecordSocket {
  private queue: Uint8Array[] = [];
  private pending?: { resolve: (record: Uint8Array) => void; reject: (error: Error) => void };
  private failure?: Error;
  private timer?: ReturnType<typeof setTimeout>;
  private abort: () => void;
  private constructor(private socket: WebSocket, private signal: AbortSignal) {
    this.abort = () => this.close(aborted());
    signal.addEventListener("abort", this.abort, { once: true });
    socket.binaryType = "arraybuffer";
    socket.onmessage = (event) => {
      if (!(event.data instanceof ArrayBuffer) || event.data.byteLength === 0 || event.data.byteLength > MAX_RECORD) {
        this.close(new Error("Hub sent an invalid encrypted record.")); return;
      }
      const record = new Uint8Array(event.data);
      if (this.pending) {
        const { resolve } = this.pending;
        this.pending = undefined;
        clearTimeout(this.timer);
        resolve(record);
      } else if (this.queue.length >= MAX_QUEUE) this.close(new Error("Encrypted response queue exceeds its limit."));
      else this.queue.push(record);
    };
    socket.onerror = () => this.close(new Error("Could not reach this machine through the Hub."));
    socket.onclose = () => this.close(new Error("Encrypted connection closed; request delivery may be uncertain."));
  }
  static async open(url: string, signal: AbortSignal, factory: SocketFactory): Promise<RecordSocket> {
    if (signal.aborted) throw aborted();
    const socket = factory(url);
    const stream = new RecordSocket(socket, signal);
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => { const error = new Error("Hub connection timed out."); stream.close(error); fail(error); }, 15_000);
      const abort = () => { clearTimeout(timer); reject(aborted()); };
      signal.addEventListener("abort", abort, { once: true });
      socket.onopen = () => { clearTimeout(timer); signal.removeEventListener("abort", abort); resolve(); };
      const fail = (error: Error) => { clearTimeout(timer); signal.removeEventListener("abort", abort); reject(error); };
      socket.onerror = () => { const error = new Error("Could not reach this machine through the Hub."); stream.close(error); fail(error); };
      socket.onclose = () => { const error = new Error("The Hub closed the encrypted connection."); stream.close(error); fail(error); };
    });
    socket.onerror = () => stream.close(new Error("Encrypted connection failed; request delivery may be uncertain."));
    socket.onclose = () => stream.close(new Error("Encrypted connection closed; request delivery may be uncertain."));
    return stream;
  }
  send(record: Uint8Array): void {
    if (this.failure || this.signal.aborted) throw this.failure ?? aborted();
    if (!record.length || record.length > MAX_RECORD || this.socket.bufferedAmount + record.length > 2 * MAX_BODY) {
      this.close(new Error("Encrypted write exceeds its buffer limit.")); throw this.failure!;
    }
    this.socket.send(record);
  }
  read(timeout = 90_000): Promise<Uint8Array> {
    const next = this.queue.shift();
    if (next) return Promise.resolve(next);
    if (this.failure) return Promise.reject(this.failure);
    if (this.pending) return Promise.reject(new Error("Concurrent encrypted record reads are forbidden."));
    return new Promise((resolve, reject) => {
      this.pending = { resolve, reject };
      this.timer = setTimeout(() => this.close(new Error("Encrypted response timed out; requests are not retried.")), timeout);
    });
  }
  close(error = aborted()): void {
    if (this.failure) return;
    this.failure = error;
    clearTimeout(this.timer);
    this.signal.removeEventListener("abort", this.abort);
    this.pending?.reject(error);
    this.pending = undefined;
    this.queue = [];
    this.socket.onmessage = this.socket.onerror = this.socket.onclose = this.socket.onopen = null;
    this.socket.close();
  }
}
const browserSocket: SocketFactory = (url) => new WebSocket(url);

export async function pairHubHost(hub_url: string, host_id: string, code: string, identity: DeviceIdentity, crypto: CryptoApi, signal: AbortSignal, factory: SocketFactory = browserSocket): Promise<SavedHubHost> {
  const socket = await RecordSocket.open(socketUrl(hub_url, host_id), signal, factory);
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

async function openChannel(hub_url: string, host: SavedHubHost, identity: DeviceIdentity, crypto: CryptoApi, signal: AbortSignal, factory: SocketFactory): Promise<{ socket: RecordSocket; channel: NoiseChannel }> {
  validateHost(host);
  const socket = await RecordSocket.open(socketUrl(hub_url, host.host_id), signal, factory);
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
  const { socket, channel } = await openChannel(hub_url, host, identity, crypto, signal, factory);
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
  constructor(private hub_url: string, readonly host: SavedHubHost, private identity: DeviceIdentity, private crypto: CryptoApi, private factory: SocketFactory = browserSocket) {
    this.hub_url = canonicalHubUrl(hub_url);
    this.endpoint = `${this.hub_url}/encrypted/${host.host_id}`;
  }
  static async connect(hub_url: string, host: SavedHubHost, identity: DeviceIdentity, crypto: CryptoApi, signal?: AbortSignal, factory: SocketFactory = browserSocket): Promise<EncryptedHubClient> {
    const client = new EncryptedHubClient(hub_url, host, identity, crypto, factory);
    const abort = () => client.close();
    if (signal?.aborted) throw aborted();
    signal?.addEventListener("abort", abort, { once: true });
    try {
      const health = await client.exchange("GET", "health");
      if ((health as { version?: number }).version !== 1) throw new Error("Machine uses an unsupported viewer API version.");
      return client;
    } catch (error) { client.close(); throw error; }
    finally { signal?.removeEventListener("abort", abort); }
  }
  private async request(method: string, command: string, payload?: unknown) {
    if (this.lifetime.signal.aborted) throw aborted();
    const body = payload === undefined ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(payload));
    if (body.length > MAX_BODY) throw new Error("Viewer request exceeds 1 MiB.");
    if (!/^[a-z_]+$/.test(command)) throw new Error("Invalid viewer command.");
    if (this.sockets.size >= 24) throw new Error("Too many active viewer requests.");
    const controller = new AbortController();
    this.sockets.add(controller);
    const abort = () => controller.abort();
    this.lifetime.signal.addEventListener("abort", abort, { once: true });
    let socket: RecordSocket | undefined;
    let channel: NoiseChannel | undefined;
    const close = () => { controller.abort(); socket?.close(); channel?.free(); channel = undefined; this.sockets.delete(controller); this.lifetime.signal.removeEventListener("abort", abort); };
    try {
      ({ socket, channel } = await openChannel(this.hub_url, this.host, this.identity, this.crypto, controller.signal, this.factory));
      socket.send(channel.encrypt_json(JSON.stringify({ type: "device_request", method, path: `/api/v1/${command}` })));
      for (let offset = 0; offset < body.length; offset += MAX_CHUNK) {
        socket.send(channel.encrypt_json(JSON.stringify({ type: "request_body", data: encodeBase64Url(body.slice(offset, offset + MAX_CHUNK).buffer) })));
      }
      socket.send(channel.encrypt_json(JSON.stringify({ type: "request_end" })));
      const response = receiveMessage(channel, await socket.read(REQUEST_TIMEOUT));
      if (response.type !== "response" || typeof response.status !== "number") throw new Error("Machine returned an invalid response.");
      const type = typeof response.content_type === "string" ? response.content_type.split(";")[0].trim() : "";
      if (!(response.status === 204 && !type) && type !== (command === "events" ? "text/event-stream" : "application/json")) throw new Error("Machine returned an unsupported response type.");
      return { socket, channel, response, close };
    } catch (error) { close(); throw error; }
  }
  private async exchange(method: string, command: string, payload?: unknown): Promise<unknown> {
    const request = await this.request(method, command, payload);
    const timer = setTimeout(request.close, REQUEST_TIMEOUT);
    const decoder = new TextDecoder();
    let body = "";
    let total = 0;
    try {
      while (true) {
        const message = receiveMessage(request.channel, await request.socket.read());
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
    } finally { clearTimeout(timer); request.close(); }
  }
  invoke<T>(command: string, payload: unknown = {}): Promise<T> { return this.exchange("POST", command, payload) as Promise<T>; }
  setStateListener(handler: (state: ConnectionState) => void): void { this.on_state = handler; }
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
            if (request.response.status !== 200) throw new Error("Live viewer connection was rejected.");
            const decoder = new TextDecoder();
            let buffer = "";
            while (!this.lifetime.signal.aborted) {
              const message = receiveMessage(request.channel, await request.socket.read(45_000));
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
          } catch { /* Only this read-only stream reconnects; requests are never replayed. */ }
          finally { request?.close(); }
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
    this.close_handlers.clear(); this.lifetime.abort(); this.listeners.clear();
  }
}
