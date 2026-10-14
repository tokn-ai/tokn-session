import { afterEach, expect, it, vi } from "vitest";
import { LiveSessionSocket } from "./liveSessionSocket";
import type { BackgroundUpdatesRequest, SessionUpdatesRequest } from "./types";

class Socket {
  static instances: Socket[] = [];
  onopen?: () => void;
  onmessage?: (event: { data: string }) => void;
  onclose?: () => void;
  onerror?: () => void;
  sent: Record<string, unknown>[] = [];
  constructor(readonly url: string) { Socket.instances.push(this); }
  send(value: string) {
    const frame = JSON.parse(value); this.sent.push(frame);
    if (frame.kind === "subscribe" || frame.kind === "subscribe_background") queueMicrotask(() => this.frame({ kind: "ack", request_id: frame.request_id, result: {} }));
  }
  frame(value: unknown) { this.onmessage?.({ data: JSON.stringify(value) }); }
  open() { this.onopen?.(); this.frame({ kind: "ready" }); }
  close() { this.onclose?.(); }
}
afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); Socket.instances = []; });
const request: SessionUpdatesRequest = { session_key: "one", subscription_id: "subscription", level: "steps", cursor: null, detail_keys: [] };
const background: BackgroundUpdatesRequest = { subscription_id: "compact", scope: "sidebar", session_keys: ["one"] };

it("authenticates in a frame, subscribes without HTTP, and renews without requesting data", async () => {
  vi.useFakeTimers(); vi.stubGlobal("WebSocket", Socket);
  const emit = vi.fn(), client = new LiveSessionSocket("https://machine", "secret", emit);
  const ready = client.connect(); const socket = Socket.instances[0]; socket.open(); await ready;
  expect(String(socket.url)).toBe("wss://machine/api/v1/live");
  expect(socket.sent[0]).toEqual({ kind: "authenticate", token: "secret" });
  await client.subscribe(request);
  expect(socket.sent[1]).toMatchObject({ kind: "subscribe", request });
  socket.frame({ kind: "event", event: "session-updated", payload: { revision: "2" } });
  expect(emit).toHaveBeenCalledWith("session-updated", { revision: "2" });
  await vi.advanceTimersByTimeAsync(30_000);
  expect(socket.sent[socket.sent.length - 1]).toEqual({ kind: "ping" });
  client.close(); await vi.advanceTimersByTimeAsync(60_000);
  expect(Socket.instances).toHaveLength(1);
});

it("restores interests after reconnect and asks HTTP to recover coverage", async () => {
  vi.useFakeTimers(); vi.stubGlobal("WebSocket", Socket);
  const emit = vi.fn(), client = new LiveSessionSocket("http://machine", "secret", emit);
  const ready = client.connect(); Socket.instances[0].open(); await ready;
  await client.subscribe(request);
  Socket.instances[0].close(); await vi.advanceTimersByTimeAsync(1000);
  Socket.instances[1].open(); await vi.advanceTimersByTimeAsync(0);
  expect(Socket.instances[1].sent[1]).toMatchObject({ kind: "subscribe", request });
  expect(emit).toHaveBeenCalledWith("transport-reconnected", {});
  client.close();
});

it("uses compact subscriptions without replaying their old identity after reconnect", async () => {
  vi.useFakeTimers(); vi.stubGlobal("WebSocket", Socket);
  const emit = vi.fn(), client = new LiveSessionSocket("https://machine", "secret", emit);
  const ready = client.connect(); Socket.instances[0].open(); await ready;
  expect(client.isConnected()).toBe(true);
  await client.subscribe(request);
  await client.subscribeBackground(background);
  expect(Socket.instances[0].sent[2]).toMatchObject({ kind: "subscribe_background", request: background });
  const payload = { subscription_id: "compact", session_key: "one", latest_assistant_message: { preview: "Main reply" } };
  Socket.instances[0].frame({ kind: "event", event: "background-session-updated", payload });
  expect(emit).toHaveBeenCalledWith("background-session-updated", payload);
  Socket.instances[0].close();
  expect(client.isConnected()).toBe(false);
  await vi.advanceTimersByTimeAsync(1000);
  Socket.instances[1].open(); await vi.advanceTimersByTimeAsync(0);
  expect(Socket.instances[1].sent).toHaveLength(2);
  expect(Socket.instances[1].sent[1]).toMatchObject({ kind: "subscribe", request });
  expect(emit).toHaveBeenCalledWith("transport-reconnected", {});
  const renewed = { ...background, subscription_id: "new-compact" };
  await client.subscribeBackground(renewed);
  expect(Socket.instances[1].sent[2]).toMatchObject({ kind: "subscribe_background", request: renewed });
  client.close();
});

it("sends compact cleanup before close retires pending requests", async () => {
  vi.useFakeTimers(); vi.stubGlobal("WebSocket", Socket);
  const client = new LiveSessionSocket("https://machine", "secret", vi.fn());
  const ready = client.connect(); Socket.instances[0].open(); await ready;
  const cleanup = client.subscribeBackground({ ...background, unsubscribe: true });
  expect(Socket.instances[0].sent[1]).toMatchObject({ kind: "subscribe_background", request: { ...background, unsubscribe: true } });
  const rejected = expect(cleanup).rejects.toThrow("Machine disconnected");
  client.close(); await rejected;
});
