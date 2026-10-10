/** Carriers move bounded ordered ciphertext records; Noise owns authentication. */
export interface RecordTransport {
  send(record: Uint8Array): void;
  read(timeout?: number): Promise<Uint8Array>;
  close(error?: Error): void;
}

export interface RecordCarrier {
  open(signal: AbortSignal): Promise<RecordTransport>;
  close(): void;
}

export type SocketFactory = (url: string) => WebSocket;
export const browserSocket: SocketFactory = (url) => new WebSocket(url);
export const MAX_RECORD = 65_535;
const MAX_QUEUE = 16;
const MAX_BUFFER = 2 * 1024 * 1024;
const CONNECT_TIMEOUT = 15_000;
const MAX_SDP = 32 * 1024;

export function disconnected(): Error { return new Error("Machine disconnected"); }

type RecordEndpoint = Pick<WebSocket, "binaryType" | "bufferedAmount" | "onmessage" | "onerror" | "onclose" | "onopen" | "close"> & {
  send(record: Uint8Array): void;
};

/** Both adapters enforce the same bounds, cancellation, and single-reader rule. */
class BoundedRecords implements RecordTransport {
  private queue: Uint8Array[] = [];
  private pending?: { resolve: (record: Uint8Array) => void; reject: (error: Error) => void };
  private failure?: Error;
  private timer?: ReturnType<typeof setTimeout>;
  private opening?: (error: Error) => void;
  private abort: () => void;

  constructor(private endpoint: RecordEndpoint, private signal: AbortSignal) {
    this.abort = () => this.close(disconnected());
    signal.addEventListener("abort", this.abort, { once: true });
    endpoint.binaryType = "arraybuffer";
    endpoint.onmessage = (event) => {
      if (!(event.data instanceof ArrayBuffer) || !event.data.byteLength || event.data.byteLength > MAX_RECORD) {
        this.close(new Error("Machine sent an invalid encrypted record.")); return;
      }
      const record = new Uint8Array(event.data);
      if (this.pending) {
        const { resolve } = this.pending; this.pending = undefined;
        clearTimeout(this.timer); resolve(record);
      } else if (this.queue.length >= MAX_QUEUE) this.close(new Error("Encrypted response queue exceeds its limit."));
      else this.queue.push(record);
    };
    endpoint.onerror = () => this.close(new Error("Encrypted connection failed; request delivery may be uncertain."));
    endpoint.onclose = () => this.close(new Error("Encrypted connection closed; request delivery may be uncertain."));
  }

  async waitForOpen(is_open: () => boolean, description: string): Promise<void> {
    if (this.signal.aborted || this.failure) { this.close(); throw this.failure ?? disconnected(); }
    if (is_open()) return;
    await new Promise<void>((resolve, reject) => {
      const finish = (error?: Error) => {
        this.opening = undefined;
        clearTimeout(timer); this.signal.removeEventListener("abort", abort);
        this.endpoint.onopen = null;
        this.endpoint.onerror = () => this.close(new Error("Encrypted connection failed; request delivery may be uncertain."));
        this.endpoint.onclose = () => this.close(new Error("Encrypted connection closed; request delivery may be uncertain."));
        if (error) { this.close(error); reject(error); } else resolve();
      };
      const timer = setTimeout(() => finish(new Error(`${description} connection timed out.`)), CONNECT_TIMEOUT);
      const abort = () => finish(disconnected());
      this.signal.addEventListener("abort", abort, { once: true });
      this.opening = (error) => finish(error);
      this.endpoint.onopen = () => finish();
      this.endpoint.onerror = () => finish(new Error(`Could not reach this machine through ${description}.`));
      this.endpoint.onclose = () => finish(new Error("Encrypted connection closed; request delivery may be uncertain."));
    });
  }

  send(record: Uint8Array): void {
    if (this.failure || this.signal.aborted) throw this.failure ?? disconnected();
    if (!record.length || record.length > MAX_RECORD || this.endpoint.bufferedAmount + record.length > MAX_BUFFER) {
      this.close(new Error("Encrypted write exceeds its buffer limit.")); throw this.failure!;
    }
    this.endpoint.send(record);
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

  close(error = disconnected()): void {
    if (this.failure) return;
    this.failure = error; clearTimeout(this.timer);
    this.opening?.(error);
    this.signal.removeEventListener("abort", this.abort);
    this.pending?.reject(error); this.pending = undefined; this.queue = [];
    this.endpoint.onmessage = this.endpoint.onerror = this.endpoint.onclose = this.endpoint.onopen = null;
    this.endpoint.close();
  }
}

export class RelayCarrier implements RecordCarrier {
  constructor(private url: string, private factory: SocketFactory = browserSocket) {}
  async open(signal: AbortSignal): Promise<RecordTransport> {
    if (signal.aborted) throw disconnected();
    const socket = this.factory(this.url);
    const records = new BoundedRecords(socket, signal);
    await records.waitForOpen(() => socket.readyState === WebSocket.OPEN, "the Hub");
    return records;
  }
  close(): void { /* Each exchange closes its own relay socket. */ }
}

export interface DirectPeer extends RecordCarrier {
  offer(): Promise<string>;
  accept(sdp: string): Promise<void>;
  /** Stop admitting exchanges while existing channels drain. */
  retire(): void;
}
export class PeerRetiredError extends Error {
  constructor() { super("Direct connection is renewing."); }
}
export type DirectPeerFactory = (ice_servers: string[], signal: AbortSignal, failed: (error: Error) => void) => DirectPeer;

function validateSdp(sdp: string): void {
  if (!sdp || new TextEncoder().encode(sdp).length > MAX_SDP) throw new Error("Direct connection description exceeds its limit.");
}

/** One peer per machine, with a fresh reliable channel for every Noise exchange. */
export class WebRtcPeer implements DirectPeer {
  private peer: RTCPeerConnection;
  private initial?: RTCDataChannel;
  private channels = new Set<RTCDataChannel>();
  private closed = false;
  private retired = false;
  private retirement_timer?: ReturnType<typeof setTimeout>;
  private abort: () => void;
  private lifetime = new AbortController();
  private opened = 0;
  constructor(ice_servers: string[], private signal: AbortSignal, private failed: (error: Error) => void, factory: (config: RTCConfiguration) => RTCPeerConnection = (config) => new RTCPeerConnection(config)) {
    if (signal.aborted) throw disconnected();
    this.peer = factory({ iceServers: ice_servers.map((url) => ({ urls: url })) });
    this.abort = () => this.close();
    signal.addEventListener("abort", this.abort, { once: true });
    this.peer.onconnectionstatechange = () => {
      if (!this.closed && ["failed", "closed", "disconnected"].includes(this.peer.connectionState)) {
        const error = new Error("Direct connection was interrupted."); this.close(); this.failed(error);
      }
    };
    // A data channel adds the SCTP section to the offer. The first exchange uses it.
    this.initial = this.createChannel();
  }

  private createChannel(): RTCDataChannel {
    // Bound upstream SCTP channel bookkeeping by periodically replacing the peer.
    if (this.opened >= 512) {
      const error = new PeerRetiredError(); this.retire(); this.failed(error); throw error;
    }
    this.opened++;
    const channel = this.peer.createDataChannel("tokn-record-v1", { ordered: true });
    this.channels.add(channel);
    channel.addEventListener("close", () => {
      this.channels.delete(channel);
      if (this.retired && !this.channels.size) this.close();
    }, { once: true });
    return channel;
  }

  async offer(): Promise<string> {
    await this.peer.setLocalDescription(await this.peer.createOffer());
    if (this.peer.iceGatheringState !== "complete") {
      await new Promise<void>((resolve, reject) => {
        const finish = (error?: Error) => {
          clearTimeout(timer); this.lifetime.signal.removeEventListener("abort", abort);
          this.peer.removeEventListener("icegatheringstatechange", changed);
          error ? reject(error) : resolve();
        };
        const changed = () => { if (this.peer.iceGatheringState === "complete") finish(); };
        const abort = () => finish(disconnected());
        const timer = setTimeout(() => finish(new Error("Direct connection negotiation timed out.")), CONNECT_TIMEOUT);
        this.lifetime.signal.addEventListener("abort", abort, { once: true });
        this.peer.addEventListener("icegatheringstatechange", changed);
        if (this.closed) abort(); else changed();
      });
    }
    if (this.closed) throw disconnected();
    const sdp = this.peer.localDescription?.sdp ?? ""; validateSdp(sdp); return sdp;
  }

  async accept(sdp: string): Promise<void> {
    validateSdp(sdp);
    if (this.closed) throw disconnected();
    await this.peer.setRemoteDescription({ type: "answer", sdp });
  }

  async open(signal: AbortSignal): Promise<RecordTransport> {
    if (this.closed || signal.aborted) throw disconnected();
    if (this.retired) throw new PeerRetiredError();
    const channel = this.initial ?? this.createChannel(); this.initial = undefined;
    const records = new BoundedRecords(channel as unknown as RecordEndpoint, signal);
    await records.waitForOpen(() => channel.readyState === "open", "the direct connection");
    // Every supported carrier must preserve the full shared Noise record bound.
    const max_message = this.peer.sctp?.maxMessageSize;
    if (max_message && max_message < MAX_RECORD) {
      const error = new Error("Direct connection cannot carry the required encrypted record size.");
      records.close(error); throw error;
    }
    return records;
  }

  retire(): void {
    if (this.closed || this.retired) return;
    this.retired = true;
    if (!this.channels.size) this.close();
    else this.retirement_timer = setTimeout(() => this.close(), 120_000);
  }

  close(): void {
    if (this.closed) return;
    clearTimeout(this.retirement_timer);
    this.closed = true; this.signal.removeEventListener("abort", this.abort); this.lifetime.abort();
    this.peer.onconnectionstatechange = null;
    for (const channel of this.channels) channel.close();
    this.channels.clear(); this.peer.close();
  }
}

export const browserDirectPeer: DirectPeerFactory | undefined = typeof RTCPeerConnection === "undefined"
  ? undefined : (ice_servers, signal, failed) => new WebRtcPeer(ice_servers, signal, failed);
