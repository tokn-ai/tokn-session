import { afterEach, expect, it, vi } from "vitest";
import { LiveSessionSocket } from "./liveSessionSocket";
import type { SessionUpdatesRequest } from "./types";

class Socket {
  static instances: Socket[] = [];
  onopen?: () => void;
  onmessage?: (event: { data: string }) => void;
  onclose?: () => void;
  onerror?: () => void;
  sent: Record<string, unknown>[] = [];
  automatic_ack = true;
  constructor(readonly url: string) { Socket.instances.push(this); }
  send(value: string) {
    const frame = JSON.parse(value); this.sent.push(frame);
    if (frame.kind === "subscribe" && this.automatic_ack) queueMicrotask(() => this.frame({ kind: "ack", request_id: frame.request_id, result: {} }));
  }
  frame(value: unknown) { this.onmessage?.({ data: JSON.stringify(value) }); }
  open() { this.onopen?.(); this.frame({ kind: "ready" }); }
  close() { this.onclose?.(); }
}
afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); Socket.instances = []; });
const request: SessionUpdatesRequest = { session_key: "one", subscription_id: "subscription", level: "steps", cursor: null, detail_keys: [] };

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
  socket.frame({ kind: "pong", renewed_ids: [request.subscription_id], missing_ids: [] });
  expect(socket.sent.filter((frame) => frame.kind === "subscribe")).toHaveLength(1);
  expect(emit).not.toHaveBeenCalledWith("session-subscriptions-lost", expect.anything());
  expect(emit).not.toHaveBeenCalledWith("transport-reconnected", expect.anything());
  client.close(); await vi.advanceTimersByTimeAsync(60_000);
  expect(Socket.instances).toHaveLength(1);
});

it("restores interests after reconnect and asks HTTP to recover coverage", async () => {
  vi.useFakeTimers(); vi.stubGlobal("WebSocket", Socket);
  const emit = vi.fn(), client = new LiveSessionSocket("http://machine", "secret", emit);
  const ready = client.connect(); Socket.instances[0].open(); await ready;
  await client.subscribe(request);
  Socket.instances[0].close();
  expect(emit).toHaveBeenCalledWith("session-subscriptions-lost", { subscription_ids: [request.subscription_id] });
  await vi.advanceTimersByTimeAsync(1000);
  Socket.instances[1].open(); await vi.advanceTimersByTimeAsync(0);
  expect(Socket.instances[1].sent[1]).toMatchObject({ kind: "subscribe", request });
  expect(emit).toHaveBeenCalledWith("transport-reconnected", {});
  client.close();
});

async function connectedClient() {
  vi.useFakeTimers(); vi.stubGlobal("WebSocket", Socket);
  const emit = vi.fn(), client = new LiveSessionSocket("https://machine", "secret", emit);
  const ready = client.connect(); const socket = Socket.instances[0]; socket.open(); await ready;
  await client.subscribe(request);
  return { client, socket, emit };
}

it("waits for fresh lease acknowledgement and shares an in-flight ping without claiming foreign interests", async () => {
  const { client, socket, emit } = await connectedClient();
  const settled = vi.fn();
  const one = client.renew([request.subscription_id, "foreign", request.subscription_id]).then(settled);
  const two = client.renew([request.subscription_id]);
  await vi.advanceTimersByTimeAsync(0);
  expect(settled).not.toHaveBeenCalled();
  expect(socket.sent.filter((frame) => frame.kind === "ping")).toHaveLength(1);
  socket.frame({ kind: "pong", renewed_ids: [request.subscription_id, "foreign"], missing_ids: [] });
  await one;
  expect(settled).toHaveBeenCalledWith({ renewed_ids: [request.subscription_id], missing_ids: ["foreign"] });
  expect(await two).toEqual({ renewed_ids: [request.subscription_id], missing_ids: [] });
  expect(emit).not.toHaveBeenCalled();
  client.close();
});

it("suspends missing ownership, restores its live interest, then requests a targeted backward baseline", async () => {
  const { client, socket, emit } = await connectedClient();
  await client.subscribe({ ...request, subscription_id: "healthy", session_key: "two" });
  const renewing = client.renew([request.subscription_id, "healthy"]);
  socket.frame({ kind: "pong", renewed_ids: ["healthy"], missing_ids: [request.subscription_id, "foreign"] });
  expect(emit).toHaveBeenCalledWith("session-subscriptions-lost", { subscription_ids: [request.subscription_id] });
  expect(await renewing).toEqual({ renewed_ids: ["healthy"], missing_ids: [request.subscription_id] });
  await vi.advanceTimersByTimeAsync(0);
  expect(socket.sent.filter((frame) => frame.kind === "subscribe").map((frame) => frame.request)).toEqual([
    request, { ...request, subscription_id: "healthy", session_key: "two" }, request,
  ]);
  expect(emit).toHaveBeenCalledWith("transport-reconnected", { subscription_ids: [request.subscription_id] });
  client.close();
});

it("does not report recovery before the replacement subscription is acknowledged", async () => {
  const { client, socket, emit } = await connectedClient();
  socket.automatic_ack = false;
  const renewing = client.renew([request.subscription_id]);
  socket.frame({ kind: "pong", renewed_ids: [], missing_ids: [request.subscription_id] });
  await renewing;
  expect(emit).not.toHaveBeenCalledWith("transport-reconnected", expect.anything());
  const subscription = socket.sent[socket.sent.length - 1];
  socket.frame({ kind: "ack", request_id: subscription.request_id, result: {} });
  await vi.advanceTimersByTimeAsync(0);
  expect(emit).toHaveBeenCalledWith("transport-reconnected", { subscription_ids: [request.subscription_id] });
  client.close();
});

it("closes a silent socket at the heartbeat deadline and rejects unverified renewals", async () => {
  const { client, socket, emit } = await connectedClient();
  const close = vi.spyOn(socket, "close").mockImplementation(() => {});
  const renewing = client.renew([request.subscription_id]);
  const rejected = expect(renewing).rejects.toThrow("heartbeat timed out");
  await vi.advanceTimersByTimeAsync(15_000);
  await rejected;
  expect(emit).toHaveBeenCalledWith("session-subscriptions-lost", { subscription_ids: [request.subscription_id] });
  expect(close).toHaveBeenCalledOnce();
  await expect(client.renew([request.subscription_id])).rejects.toThrow("Live connection interrupted");
  socket.frame({ kind: "pong", renewed_ids: [request.subscription_id], missing_ids: [] });
  socket.onclose?.();
  expect(emit.mock.calls.filter(([event]) => event === "session-subscriptions-lost")).toHaveLength(1);
  await vi.advanceTimersByTimeAsync(1000);
  expect(Socket.instances).toHaveLength(2);
  client.close();
});

it("accepts old pong frames while still requiring a response before renewal", async () => {
  const { client, socket, emit } = await connectedClient();
  const renewed = client.renew([request.subscription_id]);
  socket.frame({ kind: "pong" });
  expect(await renewed).toEqual({ renewed_ids: [request.subscription_id], missing_ids: [] });
  expect(emit).not.toHaveBeenCalled();
  client.close();
});

it("rejects malformed lease acknowledgements instead of treating them as legacy health checks", async () => {
  const { client, socket, emit } = await connectedClient();
  const renewed = client.renew([request.subscription_id]);
  const rejected = expect(renewed).rejects.toThrow("Live connection interrupted");
  socket.frame({ kind: "pong", renewed_ids: [request.subscription_id] });
  await rejected;
  expect(emit).toHaveBeenCalledWith("session-subscriptions-lost", { subscription_ids: [request.subscription_id] });
  client.close();
});

it("does not resurrect an interest released while a ping is in flight", async () => {
  const { client, socket, emit } = await connectedClient();
  const renewed = client.renew([request.subscription_id]);
  await client.subscribe({ ...request, unsubscribe: true });
  socket.frame({ kind: "pong", renewed_ids: [], missing_ids: [request.subscription_id] });
  expect(await renewed).toEqual({ renewed_ids: [], missing_ids: [request.subscription_id] });
  expect(socket.sent.filter((frame) => frame.kind === "subscribe")).toHaveLength(2);
  expect(emit).not.toHaveBeenCalled();
  client.close();
});

it("ignores stale socket messages after reconnect when acknowledging or updating current leases", async () => {
  const { client, socket, emit } = await connectedClient();
  socket.close(); await vi.advanceTimersByTimeAsync(1000);
  const next = Socket.instances[1]; next.open(); await vi.advanceTimersByTimeAsync(0);
  emit.mockClear();
  const settled = vi.fn();
  const renewed = client.renew([request.subscription_id]).then(settled);
  socket.frame({ kind: "pong", renewed_ids: [request.subscription_id], missing_ids: [] });
  socket.frame({ kind: "event", event: "session-updated", payload: { revision: "stale" } });
  socket.close();
  await vi.advanceTimersByTimeAsync(0);
  expect(settled).not.toHaveBeenCalled();
  expect(emit).not.toHaveBeenCalled();
  next.frame({ kind: "pong", renewed_ids: [request.subscription_id], missing_ids: [] });
  await renewed;
  expect(settled).toHaveBeenCalledWith({ renewed_ids: [request.subscription_id], missing_ids: [] });
  client.close();
});

it("keeps source resync and data events scoped to its currently remembered interests", async () => {
  const { client, socket, emit } = await connectedClient();
  socket.frame({ kind: "event", event: "session-updated", payload: { subscription_id: "foreign", revision: "2" } });
  socket.frame({ kind: "event", event: "session-resync-required", payload: { subscription_id: "foreign", session_key: "two" } });
  expect(emit).not.toHaveBeenCalled();
  socket.frame({ kind: "event", event: "session-resync-required", payload: { subscription_id: request.subscription_id, session_key: request.session_key } });
  expect(emit.mock.calls).toEqual([
    ["session-subscriptions-lost", { subscription_ids: [request.subscription_id] }],
    ["transport-reconnected", { subscription_ids: [request.subscription_id] }],
  ]);
  client.close();
});

it("does not revive a released interest when an earlier subscribe acknowledgement arrives late", async () => {
  const { client, socket } = await connectedClient();
  socket.automatic_ack = false;
  const updating = client.subscribe({ ...request, cursor: "2" });
  const rejected = expect(updating).rejects.toThrow("Live subscription superseded");
  await vi.advanceTimersByTimeAsync(0);
  const updateFrame = socket.sent[socket.sent.length - 1];
  const releasing = client.subscribe({ ...request, unsubscribe: true });
  await vi.advanceTimersByTimeAsync(0);
  const releaseFrame = socket.sent[socket.sent.length - 1];
  socket.frame({ kind: "ack", request_id: releaseFrame.request_id, result: {} }); await releasing;
  socket.frame({ kind: "ack", request_id: updateFrame.request_id, result: {} }); await rejected;

  const renewing = client.renew([request.subscription_id]);
  socket.frame({ kind: "pong", renewed_ids: [request.subscription_id], missing_ids: [] });
  expect(await renewing).toEqual({ renewed_ids: [], missing_ids: [request.subscription_id] });
  socket.close(); await vi.advanceTimersByTimeAsync(1000);
  const next = Socket.instances[1]; next.open(); await vi.advanceTimersByTimeAsync(0);
  expect(next.sent.filter((frame) => frame.kind === "subscribe")).toHaveLength(0);
  client.close();
});

it("does not release a successor interest when an older unsubscribe acknowledgement arrives late", async () => {
  const { client, socket } = await connectedClient();
  socket.automatic_ack = false;
  const releasing = client.subscribe({ ...request, unsubscribe: true });
  const rejected = expect(releasing).rejects.toThrow("Live subscription superseded");
  await vi.advanceTimersByTimeAsync(0);
  const releaseFrame = socket.sent[socket.sent.length - 1];
  const successor = { ...request, session_key: "two" };
  const registering = client.subscribe(successor);
  await vi.advanceTimersByTimeAsync(0);
  const registrationFrame = socket.sent[socket.sent.length - 1];
  socket.frame({ kind: "ack", request_id: registrationFrame.request_id, result: {} }); await registering;
  socket.frame({ kind: "ack", request_id: releaseFrame.request_id, result: {} }); await rejected;

  socket.close(); await vi.advanceTimersByTimeAsync(1000);
  const next = Socket.instances[1]; next.open(); await vi.advanceTimersByTimeAsync(0);
  expect(next.sent.filter((frame) => frame.kind === "subscribe").map((frame) => frame.request)).toEqual([successor]);
  client.close();
});
