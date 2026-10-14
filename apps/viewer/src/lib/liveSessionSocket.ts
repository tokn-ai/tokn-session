import { createUuid } from "./id";
import type { BackgroundUpdatesRequest, SessionUpdatesRequest } from "./types";

type Pending = { resolve: (value: unknown) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> };
/** Live data is connection-scoped; snapshots and resources stay on HTTP. */
export class LiveSessionSocket {
  private socket?: WebSocket;
  private ready?: Promise<void>;
  private closed = false;
  private connected = false;
  private open = false;
  private retry?: ReturnType<typeof setTimeout>;
  private heartbeat?: ReturnType<typeof setInterval>;
  private pending = new Map<string, Pending>();
  private interests = new Map<string, SessionUpdatesRequest>();
  private backgroundInterests = new Map<string, BackgroundUpdatesRequest>();

  constructor(private endpoint: string, private token: string,
    private emit: (event: string, payload: unknown) => void) {}

  remember(request: SessionUpdatesRequest) {
    if (request.unsubscribe) this.interests.delete(request.subscription_id);
    else this.interests.set(request.subscription_id, { ...request, history_cursor: undefined });
  }

  isConnected() { return this.open && !this.closed; }

  connect(): Promise<void> {
    if (this.closed) return Promise.reject(new Error("Machine disconnected"));
    if (this.ready) return this.ready;
    this.ready = new Promise<void>((resolve, reject) => {
      const url = new URL(`${this.endpoint}/api/v1/live`);
      url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
      const socket = new WebSocket(url);
      this.socket = socket;
      let established = false;
      const timer = setTimeout(() => socket.close(), 10_000);
      socket.onopen = () => socket.send(JSON.stringify({ kind: "authenticate", token: this.token }));
      socket.onmessage = (message) => {
        try {
          const frame = JSON.parse(String(message.data)) as { kind: string; event?: string; payload?: unknown; request_id?: string; result?: unknown; error?: string };
          if (frame.kind === "ready") {
            established = true;
            clearTimeout(timer);
            const reconnect = this.connected;
            this.connected = true;
            this.open = true;
            this.heartbeat = setInterval(() => socket.send(JSON.stringify({ kind: "ping" })), 30_000);
            resolve();
            if (reconnect) {
              void Promise.all([...this.interests.values()].map((request) => this.subscribe(request)))
                .then(() => this.emit("transport-reconnected", {})).catch(() => socket.close());
            }
          } else if (frame.kind === "ack" && frame.request_id) {
            const pending = this.pending.get(frame.request_id);
            if (!pending) return;
            clearTimeout(pending.timer); this.pending.delete(frame.request_id);
            if (frame.error) pending.reject(new Error(frame.error)); else pending.resolve(frame.result);
          } else if (frame.kind === "event" && frame.event) {
            if (frame.event === "session-resync-required") this.emit("transport-reconnected", frame.payload);
            else this.emit(frame.event, frame.payload);
          }
        } catch { socket.close(); }
      };
      socket.onerror = () => socket.close();
      socket.onclose = () => {
        clearTimeout(timer); clearInterval(this.heartbeat);
        this.open = false;
        // Compact subscriptions belong to this connection. The viewer creates
        // a fresh live-only identity after rich interests recover.
        this.backgroundInterests.clear();
        this.ready = undefined;
        for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error("Live connection interrupted")); }
        this.pending.clear();
        if (!established) reject(new Error("Could not establish live session connection"));
        if (!this.closed) this.retry = setTimeout(() => { void this.connect().catch(() => {}); }, 1000);
      };
    });
    return this.ready;
  }

  async subscribe(request: SessionUpdatesRequest): Promise<unknown> {
    await this.connect();
    const result = await this.sendSubscription("subscribe", request);
    this.remember(request);
    return result;
  }

  async subscribeBackground(request: BackgroundUpdatesRequest): Promise<unknown> {
    // Send cleanup synchronously when possible, before machine close aborts
    // pending requests. Disconnection itself retires connection-owned leases.
    if (!this.isConnected()) await this.connect();
    const result = await this.sendSubscription("subscribe_background", request);
    if (request.unsubscribe) this.backgroundInterests.delete(request.subscription_id);
    else this.backgroundInterests.set(request.subscription_id, request);
    return result;
  }

  private sendSubscription(kind: "subscribe" | "subscribe_background", request: SessionUpdatesRequest | BackgroundUpdatesRequest): Promise<unknown> {
    if (!this.isConnected()) return Promise.reject(new Error("Live connection interrupted"));
    const request_id = createUuid();
    return new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(request_id); reject(new Error("Live subscription timed out")); }, 15_000);
      this.pending.set(request_id, { resolve, reject, timer });
      try { this.socket!.send(JSON.stringify({ kind, request_id, request })); }
      catch (error) {
        clearTimeout(timer); this.pending.delete(request_id);
        reject(error instanceof Error ? error : new Error("Live connection interrupted"));
      }
    });
  }

  close() {
    this.closed = true;
    clearTimeout(this.retry); clearInterval(this.heartbeat);
    for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error("Machine disconnected")); }
    this.pending.clear(); this.interests.clear(); this.backgroundInterests.clear(); this.socket?.close();
  }
}
