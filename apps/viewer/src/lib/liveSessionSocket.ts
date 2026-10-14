import { createUuid } from "./id";
import type { SessionSubscriptionRenewal, SessionUpdatesRequest } from "./types";

type Pending = { resolve: (value: unknown) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> };
type Interest = { request: SessionUpdatesRequest; registration: symbol };
type PendingPing = {
  subscription_ids: string[];
  promise: Promise<SessionSubscriptionRenewal>;
  resolve: (value: SessionSubscriptionRenewal) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};
type Frame = {
  kind: string;
  event?: string;
  payload?: unknown;
  request_id?: string;
  result?: unknown;
  error?: string;
  renewed_ids?: unknown;
  missing_ids?: unknown;
};
const HEARTBEAT_INTERVAL_MS = 30_000;
const HEARTBEAT_TIMEOUT_MS = 15_000;
const MAX_INTERESTS = 24;
/** Live data is connection-scoped; snapshots and resources stay on HTTP. */
export class LiveSessionSocket {
  private socket?: WebSocket;
  private ready?: Promise<void>;
  private closed = false;
  private connected = false;
  private retry?: ReturnType<typeof setTimeout>;
  private heartbeat?: ReturnType<typeof setInterval>;
  private active = false;
  private pending = new Map<string, Pending>();
  private pendingPing?: PendingPing;
  private interests = new Map<string, Interest>();
  private intents = new Map<string, symbol>();
  private recovering = new Map<string, WebSocket>();

  constructor(private endpoint: string, private token: string,
    private emit: (event: string, payload: unknown) => void) {}

  interestVersion(id: string): symbol | undefined {
    return this.interests.get(id)?.registration;
  }

  /** Reads may update one existing registration, but cannot acquire ownership. */
  remember(request: SessionUpdatesRequest, registration: symbol | undefined) {
    const interest = this.interests.get(request.subscription_id);
    if (!interest || interest.registration !== registration) return;
    if (request.unsubscribe) this.interests.delete(request.subscription_id);
    else this.interests.set(request.subscription_id, { request: { ...request, history_cursor: undefined }, registration });
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
      socket.onopen = () => { if (this.socket === socket && !this.closed) socket.send(JSON.stringify({ kind: "authenticate", token: this.token })); };
      socket.onmessage = (message) => {
        if (this.socket !== socket || this.closed || (established && !this.active)) return;
        try {
          const frame = JSON.parse(String(message.data)) as Frame;
          if (frame.kind === "ready") {
            if (established) return;
            established = true;
            this.active = true;
            clearTimeout(timer);
            const reconnect = this.connected;
            this.connected = true;
            this.heartbeat = setInterval(() => { void this.ping().catch(() => {}); }, HEARTBEAT_INTERVAL_MS);
            resolve();
            if (reconnect) {
              this.restore(socket, [...this.interests.keys()], {});
            }
          } else if (frame.kind === "pong") {
            this.acceptPong(socket, frame);
          } else if (frame.kind === "ack" && frame.request_id) {
            const pending = this.pending.get(frame.request_id);
            if (!pending) return;
            clearTimeout(pending.timer); this.pending.delete(frame.request_id);
            if (frame.error) pending.reject(new Error(frame.error)); else pending.resolve(frame.result);
          } else if (frame.kind === "event" && frame.event) {
            const payload = frame.payload as { subscription_id?: unknown } | undefined;
            const id = typeof payload?.subscription_id === "string" ? payload.subscription_id : undefined;
            if (id && !this.interests.has(id)) return;
            if (frame.event === "session-resync-required") {
              if (id) this.lost([id]);
              this.emit("transport-reconnected", id ? { subscription_ids: [id] } : frame.payload);
            }
            else this.emit(frame.event, frame.payload);
          }
        } catch { this.loseConnection(socket); socket.close(); }
      };
      socket.onerror = () => { this.loseConnection(socket); socket.close(); };
      socket.onclose = () => {
        if (this.socket !== socket) return;
        clearTimeout(timer); clearInterval(this.heartbeat);
        this.loseConnection(socket);
        this.ready = undefined;
        this.failPing(new Error("Live connection interrupted"));
        this.recovering.clear();
        for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error("Live connection interrupted")); }
        this.pending.clear();
        if (!established) reject(new Error("Could not establish live session connection"));
        if (!this.closed) this.retry = setTimeout(() => { void this.connect().catch(() => {}); }, 1000);
      };
    });
    return this.ready;
  }

  async subscribe(request: SessionUpdatesRequest): Promise<unknown> {
    const id = request.subscription_id, intent = Symbol();
    this.intents.set(id, intent);
    if (request.unsubscribe) this.interests.delete(id);
    else {
      const interest = this.interests.get(id);
      if (interest) this.interests.set(id, { ...interest, registration: intent });
    }
    try {
      await this.connect();
      if (this.intents.get(id) !== intent) throw new Error("Live subscription superseded");
      const socket = this.socket!;
      const result = await this.sendSubscription(socket, request);
      if (this.socket !== socket || !this.active || this.closed) throw new Error("Live connection interrupted");
      if (this.intents.get(id) !== intent) throw new Error("Live subscription superseded");
      if (!request.unsubscribe) this.interests.set(id, { request: { ...request, history_cursor: undefined }, registration: intent });
      return result;
    } finally { if (this.intents.get(id) === intent) this.intents.delete(id); }
  }

  /** A successful renewal requires a fresh server pong; HTTP never fetches data. */
  async renew(ids: string[]): Promise<SessionSubscriptionRenewal> {
    const requested = [...new Set(ids)];
    if (!requested.length) return { renewed_ids: [], missing_ids: [] };
    if (!this.active || this.closed) throw new Error("Live connection interrupted");
    const socket = this.socket;
    const ack = await this.ping();
    if (this.socket !== socket || !this.active || this.closed) throw new Error("Live connection interrupted");
    const renewed = new Set(ack.renewed_ids);
    return {
      renewed_ids: requested.filter((id) => renewed.has(id) && this.interests.has(id)),
      missing_ids: requested.filter((id) => !renewed.has(id) || !this.interests.has(id)),
    };
  }

  private sendSubscription(socket: WebSocket, request: SessionUpdatesRequest): Promise<unknown> {
    if (this.socket !== socket || !this.active || this.closed) return Promise.reject(new Error("Live connection interrupted"));
    const request_id = createUuid();
    return new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(request_id); reject(new Error("Live subscription timed out")); }, 15_000);
      this.pending.set(request_id, { resolve, reject, timer });
      try { socket.send(JSON.stringify({ kind: "subscribe", request_id, request })); }
      catch { clearTimeout(timer); this.pending.delete(request_id); reject(new Error("Live connection interrupted")); this.loseConnection(socket); socket.close(); }
    });
  }

  private ping(): Promise<SessionSubscriptionRenewal> {
    if (this.pendingPing) return this.pendingPing.promise;
    if (!this.active || this.closed || !this.socket) return Promise.reject(new Error("Live connection interrupted"));
    const socket = this.socket;
    let resolve!: PendingPing["resolve"], reject!: PendingPing["reject"];
    const promise = new Promise<SessionSubscriptionRenewal>((yes, no) => { resolve = yes; reject = no; });
    const timer = setTimeout(() => {
      this.failPing(new Error("Live heartbeat timed out"));
      this.loseConnection(socket);
      socket.close();
    }, HEARTBEAT_TIMEOUT_MS);
    this.pendingPing = { subscription_ids: [...this.interests.keys()], promise, resolve, reject, timer };
    try { socket.send(JSON.stringify({ kind: "ping" })); }
    catch { this.failPing(new Error("Live connection interrupted")); this.loseConnection(socket); socket.close(); }
    return promise;
  }

  private acceptPong(socket: WebSocket, frame: Frame) {
    const ping = this.pendingPing;
    if (!ping) return;
    let renewed: Set<string>;
    if (frame.renewed_ids === undefined && frame.missing_ids === undefined) {
      // Older hosts acknowledge connection health without lease inventory.
      renewed = new Set(ping.subscription_ids);
    } else {
      const valid = (ids: unknown): ids is string[] => Array.isArray(ids) && ids.length <= MAX_INTERESTS && ids.every((id) => typeof id === "string");
      if (!valid(frame.renewed_ids) || !valid(frame.missing_ids)) throw new Error("Invalid live lease acknowledgement");
      const missing = new Set(frame.missing_ids);
      renewed = new Set(frame.renewed_ids.filter((id) => !missing.has(id)));
    }
    clearTimeout(ping.timer);
    this.pendingPing = undefined;
    const ack = {
      renewed_ids: ping.subscription_ids.filter((id) => renewed.has(id)),
      missing_ids: ping.subscription_ids.filter((id) => !renewed.has(id)),
    };
    ping.resolve(ack);
    const missing = ack.missing_ids.filter((id) => this.interests.has(id));
    this.lost(missing);
    if (missing.length) this.restore(socket, missing, { subscription_ids: missing });
  }

  private lost(ids: string[]) {
    if (ids.length) this.emit("session-subscriptions-lost", { subscription_ids: ids });
  }

  private loseConnection(socket: WebSocket) {
    if (this.socket !== socket || !this.active) return;
    this.active = false;
    clearInterval(this.heartbeat);
    if (!this.closed) this.lost([...this.interests.keys()]);
  }

  private restore(socket: WebSocket, ids: string[], payload: unknown) {
    const requests = ids.flatMap((id) => {
      const interest = this.interests.get(id);
      if (!interest || this.recovering.has(id)) return [];
      this.recovering.set(id, socket);
      return [interest.request];
    });
    if (!requests.length) {
      if (!ids.length && this.socket === socket && this.active && !this.closed) this.emit("transport-reconnected", payload);
      return;
    }
    void Promise.all(requests.map((request) => this.sendSubscription(socket, request)))
      .then(() => {
        if (this.socket !== socket || !this.active || this.closed) return;
        if (requests.some((request) => this.interests.has(request.subscription_id))) this.emit("transport-reconnected", payload);
      })
      .catch(() => { if (this.socket === socket) socket.close(); })
      .finally(() => { for (const request of requests) if (this.recovering.get(request.subscription_id) === socket) this.recovering.delete(request.subscription_id); });
  }

  private failPing(error: Error) {
    if (!this.pendingPing) return;
    clearTimeout(this.pendingPing.timer);
    this.pendingPing.reject(error);
    this.pendingPing = undefined;
  }

  close() {
    this.closed = true;
    clearTimeout(this.retry); clearInterval(this.heartbeat);
    this.failPing(new Error("Machine disconnected"));
    for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error("Machine disconnected")); }
    this.pending.clear(); this.interests.clear(); this.intents.clear(); this.socket?.close();
  }
}
