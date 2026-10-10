import { createUuid } from "./id";
import type { SessionUpdatesRequest } from "./types";

type Pending = { resolve: (value: unknown) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> };
/** Live data is connection-scoped; snapshots and resources stay on HTTP. */
export class LiveSessionSocket {
  private socket?: WebSocket;
  private ready?: Promise<void>;
  private closed = false;
  private connected = false;
  private retry?: ReturnType<typeof setTimeout>;
  private heartbeat?: ReturnType<typeof setInterval>;
  private pending = new Map<string, Pending>();
  private interests = new Map<string, SessionUpdatesRequest>();

  constructor(private endpoint: string, private token: string,
    private emit: (event: string, payload: unknown) => void) {}

  remember(request: SessionUpdatesRequest) {
    if (request.unsubscribe) this.interests.delete(request.subscription_id);
    else this.interests.set(request.subscription_id, { ...request, history_cursor: undefined });
  }

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
    const request_id = createUuid();
    const result = await new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(request_id); reject(new Error("Live subscription timed out")); }, 15_000);
      this.pending.set(request_id, { resolve, reject, timer });
      this.socket!.send(JSON.stringify({ kind: "subscribe", request_id, request }));
    });
    this.remember(request);
    return result;
  }

  close() {
    this.closed = true;
    clearTimeout(this.retry); clearInterval(this.heartbeat);
    for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error("Machine disconnected")); }
    this.pending.clear(); this.interests.clear(); this.socket?.close();
  }
}
