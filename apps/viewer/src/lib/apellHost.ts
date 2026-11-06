import type { ViewerHost } from "./platform";
import { createUuid } from "./id";
import { eventFrameLimit, parseEvent } from "./transport";

/** Only this adapter knows Apell's command/event bridge. Viewer code remains shared. */
export interface AppletBridge {
  invoke<T>(command: string, params: Record<string, unknown>): Promise<T>;
  onEvent(handler: (event: { name: string; payload: unknown }) => void): () => void;
}

export function createApellHost(bridge: AppletBridge): ViewerHost {
  const handlers = new Map<string, Set<(event: { payload: unknown }) => void>>();
  const responses = new Map<string, { connection_id: string; decoder: TextDecoder; parts: string[]; bytes: number; sequence: number; error?: Error }>();
  const streams = new Map<string, Stream>();
  let disposed = false;
  const emit = (name: string, payload: unknown) => {
    for (const handler of handlers.get(name) ?? []) {
      try { handler({ payload }); } catch { /* One view observer cannot corrupt transport continuity. */ }
    }
  };
  const invalidEvent = () => {
    const error = new Error("The applet returned an invalid transport event.");
    const connections = new Set<string>();
    for (const response of responses.values()) { response.error = error; connections.add(response.connection_id); }
    for (const [connection_id, stream] of streams) {
      clearTimeout(stream.timer); stream.reject(error); connections.add(connection_id);
    }
    streams.clear();
    for (const connection_id of connections) {
      void bridge.invoke("hub_client_close", { connection_id }).catch(() => {});
      emit("hub-client-state", { connection_id, state: "reconnecting" });
    }
  };
  const stop = bridge.onEvent(({ name, payload }) => {
    if (disposed) return;
    if (["hub-client-response", "hub-client-stream-reset", "hub-client-stream-chunk"].includes(name)
      && (!payload || typeof payload !== "object" || Array.isArray(payload))) { invalidEvent(); return; }
    if (name === "hub-client-response") {
      const frame = payload as { response_id: string; kind: string; data: string; channel_id: string; sequence: number };
      const response = responses.get(frame.response_id);
      if (!response || response.error) return;
      try {
        if (frame.kind !== "chunk") throw new Error("The applet returned an invalid response chunk.");
        if (frame.channel_id !== frame.response_id || frame.sequence !== response.sequence) throw new Error("The applet response lost its chunk sequence.");
        const bytes = decodeBytes(frame.data);
        response.bytes += bytes.byteLength;
        if (response.bytes > 128 * 1024 * 1024) throw new Error("The viewer response exceeds the supported size.");
        response.parts.push(response.decoder.decode(bytes, { stream: true }));
        response.sequence += 1;
        void bridge.invoke("ack_bridge_chunk", { channel_id: frame.channel_id, sequence: frame.sequence })
          .catch((error) => { response.error = failure(error); });
      } catch (error) { response.error = failure(error); }
      return;
    }
    if (name === "hub-client-stream-reset" || name === "hub-client-stream-chunk") {
      const frame = payload as { connection_id: string; generation: string; data: string; channel_id: string; sequence: number };
      const stream = streams.get(frame.connection_id);
      if (!stream) return;
      try {
        if (name === "hub-client-stream-reset") {
          if (!frame.generation || typeof frame.generation !== "string") throw new Error("Invalid live stream generation.");
          stream.generation = frame.generation; stream.sequence = 0; stream.buffer = ""; stream.decoder = new TextDecoder("utf-8", { fatal: true });
          emit("hub-client-state", { connection_id: frame.connection_id, state: stream.connected ? "reconnecting" : "connecting" });
        } else {
          if (!stream.generation || frame.generation !== stream.generation) return;
          if (frame.channel_id !== `${frame.connection_id}:${frame.generation}` || frame.sequence !== stream.sequence) throw new Error("The applet live stream lost its chunk sequence.");
          stream.buffer += stream.decoder.decode(decodeBytes(frame.data), { stream: true }).replace(/\r/g, "");
          stream.sequence += 1;
          void bridge.invoke("ack_bridge_chunk", { channel_id: frame.channel_id, sequence: frame.sequence }).catch((error) => {
            stream.reject(failure(error));
            void bridge.invoke("hub_client_close", { connection_id: frame.connection_id }).catch(() => {});
          });
          let end: number;
          while ((end = stream.buffer.indexOf("\n\n")) !== -1) {
            if (end > eventFrameLimit(stream.buffer)) throw new Error("Live event exceeds size limit.");
            const event = parseEvent(stream.buffer.slice(0, end));
            stream.buffer = stream.buffer.slice(end + 2);
            if (!event) continue;
            if (event.event === "ready") {
              emit("hub-client-state", { connection_id: frame.connection_id, state: "connected" });
              if (stream.connected) {
                emit("hub-client-event", { connection_id: frame.connection_id, event: "transport-reconnected", payload: {} });
                emit("hub-client-event", { connection_id: frame.connection_id, event: "relay-changed", payload: { session_key: null, reset: true } });
              }
              stream.connected = true; clearTimeout(stream.timer); stream.resolve();
            } else emit("hub-client-event", { connection_id: frame.connection_id, ...event });
          }
          if (stream.buffer.length > eventFrameLimit(stream.buffer)) throw new Error("Live event exceeds size limit.");
        }
      } catch (error) {
        clearTimeout(stream.timer); stream.reject(failure(error)); streams.delete(frame.connection_id);
        void bridge.invoke("hub_client_close", { connection_id: frame.connection_id }).catch(() => {});
        emit("hub-client-state", { connection_id: frame.connection_id, state: "reconnecting" });
      }
      return;
    }
    emit(name, payload);
  });

  async function request<T>(params: Record<string, unknown>): Promise<T> {
    const response_id = createUuid();
    const response = { connection_id: String(params.connection_id), decoder: new TextDecoder("utf-8", { fatal: true }), parts: [] as string[], bytes: 0, sequence: 0, error: undefined as Error | undefined };
    responses.set(response_id, response);
    try {
      const receipt = await bridge.invoke<{ response_id: string; status: number; content_type: string | null }>("hub_client_request", { ...params, response_id });
      if (response.error) throw response.error;
      if (disposed || receipt.response_id !== response_id || !Number.isInteger(receipt.status) || receipt.status < 200 || receipt.status > 599
        || receipt.content_type?.split(";", 1)[0].trim() !== "application/json") throw new Error("The applet returned an invalid viewer response.");
      const result = JSON.parse(response.parts.join("") + response.decoder.decode()) as T & { error?: unknown };
      if (receipt.status >= 400) throw new Error(typeof result?.error === "string" ? result.error : `Viewer API returned ${receipt.status}`);
      return result;
    } finally { responses.delete(response_id); }
  }

  async function listenStream(params: Record<string, unknown>): Promise<void> {
    const connection_id = String(params.connection_id);
    let stream = streams.get(connection_id);
    if (!stream) {
      let resolve!: () => void; let reject!: (error: Error) => void;
      const ready = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
      // Startup acknowledgement can be pending when the readiness deadline fires.
      // Attach a rejection observer immediately; the caller still awaits ready.
      void ready.catch(() => {});
      const timer = setTimeout(() => { reject(new Error("Live connection did not become ready.")); streams.delete(connection_id);
        void bridge.invoke("hub_client_close", { connection_id }).catch(() => {}); }, 30_000);
      stream = { decoder: new TextDecoder("utf-8", { fatal: true }), buffer: "", sequence: 0, connected: false, ready, resolve, reject, timer };
      streams.set(connection_id, stream);
      try { await bridge.invoke("hub_client_listen", params); }
      catch (error) { clearTimeout(timer); streams.delete(connection_id); reject(failure(error)); }
    }
    await stream.ready;
  }

  return {
    kind: "apell",
    capabilities: { local_machine: false, device_preferences: true, native_hub: true,
      passkeys: false, native_translation: false, runtime_management: false, hosting: false },
    invoke: async <T>(command: string, params: Record<string, unknown> = {}): Promise<T> => {
      if (disposed) throw new Error("The applet view is closed.");
      if (command === "hub_client_request") return request<T>(params);
      if (command === "hub_client_listen") return listenStream(params) as Promise<T>;
      if (command === "hub_client_close") {
        const stream = streams.get(String(params.connection_id));
        if (stream) { clearTimeout(stream.timer); stream.reject(new Error("Machine disconnected.")); streams.delete(String(params.connection_id)); }
        for (const response of responses.values()) if (response.connection_id === params.connection_id) response.error = new Error("Machine disconnected; delivery may be uncertain.");
      }
      return bridge.invoke(command, params);
    },
    listen: async <T>(name: string, handler: (event: { payload: T }) => void) => {
      if (disposed) throw new Error("The applet view is closed.");
      const listeners = handlers.get(name) ?? new Set();
      const observer = handler as (event: { payload: unknown }) => void;
      listeners.add(observer); handlers.set(name, listeners);
      return () => { listeners.delete(observer); };
    },
    dispose() {
      if (disposed) return;
      disposed = true; stop(); handlers.clear();
      for (const stream of streams.values()) { clearTimeout(stream.timer); stream.reject(new Error("The applet view is closed.")); }
      streams.clear(); responses.clear();
    },
  };
}

interface Stream {
  generation?: string;
  decoder: TextDecoder;
  buffer: string;
  connected: boolean;
  sequence: number;
  ready: Promise<void>;
  resolve: () => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}
const failure = (error: unknown) => error instanceof Error ? error : new Error(String(error));
function decodeBytes(value: string): Uint8Array {
  if (typeof value !== "string" || value.length > 128 * 1024 || !/^[A-Za-z0-9_-]*$/.test(value)) throw new Error("Invalid applet transport chunk.");
  const raw = atob(value.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(raw, (byte) => byte.charCodeAt(0));
}
