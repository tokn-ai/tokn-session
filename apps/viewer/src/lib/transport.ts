import { LiveSessionSocket } from "./liveSessionSocket";
import type { SessionUpdatesRequest } from "./types";
export type UnlistenFn = () => void;
export type CommandInvoker = <T>(command: string, payload?: Record<string, unknown>) => Promise<T>;
export type EventSubscriber = <T>(event: string, handler: (event: { payload: T }) => void) => Promise<UnlistenFn>;
type Handler = (event: { payload: unknown }) => void;
export type ConnectionState = "connecting" | "connected" | "reconnecting";
const MAX_EVENT_FRAME_LENGTH = 2 * 1024 * 1024;
// Full session updates can include multiple bounded tool/native payloads.
// Other notifications retain the smaller frame limit.
const MAX_SESSION_UPDATE_FRAME_LENGTH = 64 * 1024 * 1024;
export function eventFrameLimit(frame: string): number {
  return /^event: ?session-updated\n/.test(frame) ? MAX_SESSION_UPDATE_FRAME_LENGTH : MAX_EVENT_FRAME_LENGTH;
}
export const isDesktop = () => "__TAURI_INTERNALS__" in window;

export interface ViewerClient {
  readonly endpoint: string;
  invoke<T>(command: string, payload?: unknown): Promise<T>;
  listen<T>(name: string, handler: (event: { payload: T }) => void): Promise<UnlistenFn>;
  setStateListener(handler: (state: ConnectionState) => void): void;
  onClose(handler: () => void): UnlistenFn;
  release(command: string, payload?: Record<string, unknown>): Promise<void>;
  close(): void;
}

// One selected machine owns requests and subscriptions. Closing it aborts both,
// so late responses from an old machine cannot enter the next viewer instance.
export class RemoteClient {
  private listeners = new Map<string, Set<Handler>>();
  private lifetime = new AbortController();
  private requests = new Set<AbortController>();
  private started?: Promise<void>;
  private live?: LiveSessionSocket;
  private closed = false;
  private closeHandlers = new Set<() => void>();
  private onState: (state: ConnectionState) => void = () => {};

  constructor(readonly endpoint: string, private token: string) {}

  static async connect(endpoint: string, token: string, signal?: AbortSignal): Promise<RemoteClient> {
    const url = new URL(endpoint);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
      throw new Error("Enter an HTTP or HTTPS API address without credentials or query parameters.");
    }
    const client = new RemoteClient(url.toString().replace(/\/$/, ""), token);
    const abort = () => client.close();
    if (signal?.aborted) client.close();
    signal?.addEventListener("abort", abort, { once: true });
    try {
      const health = await client.fetchJson("health") as { version?: number; live_updates?: boolean };
      if (health.version !== 1) throw new Error("This server uses an unsupported viewer API version.");
      // Existing Hub and paired HTTP tunnels cannot upgrade an upstream WebSocket.
      if (health.live_updates && !/\/(?:hosts|paired)\//.test(url.pathname)) client.live = new LiveSessionSocket(client.endpoint, token, (event, payload) => {
        client.emit(event, payload);
        if (event === "transport-reconnected") client.emit("relay-changed", { session_key: null, reset: true });
      });
      return client;
    } catch (error) { client.close(); throw error; }
    finally { signal?.removeEventListener("abort", abort); }
  }

  setStateListener(handler: (state: ConnectionState) => void) { this.onState = handler; }
  private headers(): Record<string, string> {
    return this.token ? { Authorization: `Bearer ${this.token}` } : {};
  }
  private async fetchJson(path: string, payload?: unknown): Promise<unknown> {
    if (this.closed) throw new Error("Machine disconnected");
    const controller = new AbortController();
    this.requests.add(controller);
    const timeout = setTimeout(() => controller.abort(), 30_000);
    try {
      const response = await fetch(`${this.endpoint}/api/v1/${path}`, {
        method: payload === undefined ? "GET" : "POST",
        headers: { ...this.headers(), ...(payload === undefined ? {} : { "Content-Type": "application/json" }) },
        body: payload === undefined ? undefined : JSON.stringify(payload),
        signal: controller.signal,
        credentials: "omit",
        redirect: "error",
      });
      const body = await response.json();
      if (!response.ok) throw new Error(body.error ?? `Viewer API returned ${response.status}`);
      if (this.closed) throw new Error("Machine disconnected");
      return body;
    } finally { clearTimeout(timeout); this.requests.delete(controller); }
  }
  invoke<T>(command: string, payload: unknown = {}): Promise<T> {
    if (command === "subscribe_session" && this.live) return this.live.subscribe((payload as { request: SessionUpdatesRequest }).request) as Promise<T>;
    if (command === "renew_session_subscriptions" && this.live) return Promise.resolve(undefined as T);
    return this.fetchJson(command, payload).then((result) => {
      if (this.live && (command === "load_session_backward" || command === "load_session_details")) {
        const value = payload as { request: SessionUpdatesRequest | { kind: string; request: SessionUpdatesRequest } };
        const request = "kind" in value.request ? value.request.request : value.request;
        if (request.subscription_id) this.live.remember(request);
      }
      return result as T;
    });
  }
  async listen<T>(name: string, handler: (event: { payload: T }) => void): Promise<UnlistenFn> {
    if (this.closed) throw new Error("Machine disconnected");
    const handlers = this.listeners.get(name) ?? new Set<Handler>();
    const observer = handler as Handler;
    handlers.add(observer);
    this.listeners.set(name, handlers);
    this.started ??= new Promise<void>((resolve, reject) => { void this.pump(resolve, reject); });
    try { await this.started; if (name === "session-updated" && this.live) await this.live.connect(); }
    catch (error) { handlers.delete(observer); throw error; }
    return () => { handlers.delete(observer); };
  }
  private emit(name: string, payload: unknown) {
    for (const handler of this.listeners.get(name) ?? []) handler({ payload });
  }
  private async pump(ready: () => void, failed: (error: unknown) => void) {
    let connected = false;
    let attempted = false;
    while (!this.closed) {
      this.onState(attempted ? "reconnecting" : "connecting");
      attempted = true;
      const connection = new AbortController();
      const abort = () => connection.abort();
      this.lifetime.signal.addEventListener("abort", abort, { once: true });
      let watchdog = setTimeout(abort, 20_000);
      try {
        const response = await fetch(`${this.endpoint}/api/v1/events${this.live ? "?session_updates=false" : ""}`, {
          headers: this.headers(), signal: connection.signal, credentials: "omit", redirect: "error",
        });
        if (!response.ok || !response.body) throw new Error(`Live connection failed (${response.status})`);
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        try {
          while (!this.closed) {
            const { done, value } = await reader.read();
            if (done) break;
            clearTimeout(watchdog);
            watchdog = setTimeout(abort, 45_000);
            buffer += decoder.decode(value, { stream: true }).replace(/\r/g, "");
            let end: number;
            while ((end = buffer.indexOf("\n\n")) !== -1) {
              if (end > eventFrameLimit(buffer)) throw new Error("Live event exceeds size limit");
              const frame = buffer.slice(0, end); buffer = buffer.slice(end + 2);
              const parsed = parseEvent(frame);
              if (!parsed) continue;
              if (parsed.event === "ready") {
                this.onState("connected");
                if (connected) {
                  this.emit("transport-reconnected", {});
                  this.emit("relay-changed", { session_key: null, reset: true });
                }
                connected = true; ready();
              } else this.emit(parsed.event, parsed.payload);
            }
            if (buffer.length > eventFrameLimit(buffer)) throw new Error("Live event exceeds size limit");
          }
        } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
      } catch {
        // The connection banner stays visible while both initial and later
        // failures retry. Only a ready stream releases initial catalog reads.
      } finally {
        clearTimeout(watchdog);
        this.lifetime.signal.removeEventListener("abort", abort);
        connection.abort();
      }
      if (!this.closed) {
        this.onState("reconnecting");
        await new Promise<void>((resolve) => {
          const finish = () => { clearTimeout(timer); this.lifetime.signal.removeEventListener("abort", finish); resolve(); };
          const timer = setTimeout(finish, 1000);
          this.lifetime.signal.addEventListener("abort", finish, { once: true });
        });
      }
    }
    if (!connected) failed(new Error("Machine disconnected"));
  }
  onClose(handler: () => void): UnlistenFn {
    if (this.closed) return () => {};
    this.closeHandlers.add(handler);
    return () => { this.closeHandlers.delete(handler); };
  }
  // Release a view even as the selected machine aborts its ordinary requests.
  // fetch captures the old endpoint and credentials before close clears them.
  async release(command: string, payload?: Record<string, unknown>): Promise<void> {
    if (this.closed) return;
    await fetch(`${this.endpoint}/api/v1/${command}`, {
      method: "POST",
      headers: { ...this.headers(), "Content-Type": "application/json" },
      body: JSON.stringify(payload ?? {}),
      credentials: "omit",
      redirect: "error",
      keepalive: true,
    });
  }
  close() {
    if (this.closed) return;
    for (const handler of this.closeHandlers) handler();
    this.closeHandlers.clear();
    this.closed = true;
    this.live?.close();
    this.lifetime.abort();
    for (const request of this.requests) request.abort();
    this.listeners.clear();
    this.token = "";
  }
}

export function parseEvent(frame: string): { event: string; payload: unknown } | null {
  let event = "message";
  const data: string[] = [];
  for (const line of frame.split("\n")) {
    if (line.startsWith("event:")) event = line.slice(6).trim();
    if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
  }
  return data.length ? { event, payload: JSON.parse(data.join("\n")) } : null;
}

let selected: ViewerClient | undefined;
export function viewerStorageScope(): string {
  return selected?.endpoint ?? (isDesktop() ? "desktop" : window.location.origin);
}

export function selectMachine(client?: ViewerClient) {
  selected?.close();
  selected = client;
}
/** Cleanup may retire its own client without clearing a successor's selection. */
export function deselectMachine(client: ViewerClient) {
  if (selected === client) selectMachine();
}
export async function invoke<T>(command: string, payload?: Record<string, unknown>): Promise<T> {
  if (selected) return selected.invoke<T>(command, payload);
  if (isDesktop()) return (await import("@tauri-apps/api/core")).invoke<T>(command, payload);
  throw new Error("Connect to a machine first");
}
export async function listen<T>(event: string, handler: (event: { payload: T }) => void): Promise<UnlistenFn> {
  if (selected) return selected.listen<T>(event, handler);
  if (isDesktop()) return (await import("@tauri-apps/api/event")).listen<T>(event, handler);
  throw new Error("Connect to a machine first");
}

/** Capture one machine so deferred updates and cleanup cannot target its successor. */
export function captureTransport(): {
  invoke: CommandInvoker;
  listen: EventSubscriber;
  release: (command: string, payload?: Record<string, unknown>) => Promise<void>;
  on_close: (handler: () => void) => UnlistenFn;
} {
  if (!selected && isDesktop()) {
    const local: CommandInvoker = async <T>(command: string, payload?: Record<string, unknown>) =>
      (await import("@tauri-apps/api/core")).invoke<T>(command, payload);
    const subscribe: EventSubscriber = async <T>(event: string, handler: (event: { payload: T }) => void) =>
      (await import("@tauri-apps/api/event")).listen<T>(event, handler);
    return { invoke: local, listen: subscribe, release: local, on_close: () => () => {} };
  }
  const client = selected;
  const send: CommandInvoker = <T>(command: string, payload?: Record<string, unknown>) =>
    client ? client.invoke<T>(command, payload) : Promise.reject(new Error("Connect to a machine first"));
  return {
    invoke: send,
    listen: (event, handler) => client ? client.listen(event, handler) : Promise.reject(new Error("Connect to a machine first")),
    release: (command, payload) => client?.release(command, payload) ?? Promise.resolve(),
    on_close: (handler) => client?.onClose(handler) ?? (() => {}),
  };
}
