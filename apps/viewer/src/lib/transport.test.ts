import { afterEach, describe, expect, it, vi } from "vitest";
import { parseEvent, RemoteClient, selectMachine, deselectMachine, invoke, listen, viewerStorageScope, captureTransport } from "./transport";
import { invoke as nativeInvoke } from "@tauri-apps/api/core";
import { listen as nativeListen } from "@tauri-apps/api/event";
import type { BackgroundUpdatesRequest } from "./types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

const clients: RemoteClient[] = [];
afterEach(() => { for (const client of clients) client.close(); clients.length = 0; selectMachine(); LiveSocket.instances = []; vi.unstubAllGlobals(); vi.useRealTimers(); });
function client() { const result = new RemoteClient("http://machine:5558", "secret"); clients.push(result); return result; }
function stream() {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const body = new ReadableStream<Uint8Array>({ start(value) { controller = value; } });
  return { response: new Response(body), send: (value: string) => controller.enqueue(new TextEncoder().encode(value)), end: () => controller.close() };
}

class LiveSocket {
  static instances: LiveSocket[] = [];
  onopen?: () => void;
  onmessage?: (event: { data: string }) => void;
  onclose?: () => void;
  sent: Record<string, unknown>[] = [];
  constructor() { LiveSocket.instances.push(this); }
  send(value: string) {
    const frame = JSON.parse(value); this.sent.push(frame);
    if (frame.kind === "subscribe") queueMicrotask(() => this.frame({ kind: "ack", request_id: frame.request_id, result: {} }));
    if (frame.kind === "subscribe_background") queueMicrotask(() => this.frame({ kind: "ack", request_id: frame.request_id,
      result: { subscription_id: frame.request.subscription_id, generation: "compact-generation", revision: "0", scope: frame.request.scope, subscribed: !frame.request.unsubscribe } }));
  }
  frame(value: unknown) { this.onmessage?.({ data: JSON.stringify(value) }); }
  open() { this.onopen?.(); this.frame({ kind: "ready" }); }
  close() { this.onclose?.(); }
}

const background: BackgroundUpdatesRequest = { subscription_id: "compact", scope: "sidebar", session_keys: ["one"] };

describe("remote viewer transport", () => {
  it("keeps captured local requests and listeners local after selecting a remote machine", async () => {
    vi.stubGlobal("__TAURI_INTERNALS__", {});
    vi.mocked(nativeInvoke).mockResolvedValue({ local: true });
    vi.mocked(nativeListen).mockResolvedValue(() => {});
    const local = captureTransport();
    const remote = client();
    const send = vi.spyOn(remote, "invoke").mockResolvedValue({ remote: true });
    const subscribe = vi.spyOn(remote, "listen").mockResolvedValue(() => {});
    selectMachine(remote);
    expect(await local.invoke("list_sessions")).toEqual({ local: true });
    const handler = vi.fn();
    await local.listen("session-updated", handler);
    await local.listen("background-session-updated", handler);
    await local.invoke("subscribe_background_updates", { request: background });
    expect(nativeListen).toHaveBeenCalledWith("session-updated", handler);
    expect(nativeListen).toHaveBeenCalledWith("background-session-updated", handler);
    expect(nativeInvoke).toHaveBeenCalledWith("subscribe_background_updates", { request: background });
    expect(send).not.toHaveBeenCalled();
    expect(subscribe).not.toHaveBeenCalled();
    expect(await invoke("list_sessions")).toEqual({ remote: true });
  });
  it("does not let a captured disconnected reader subscribe to a later machine", async () => {
    const disconnected = captureTransport();
    const remote = client();
    const subscribe = vi.spyOn(remote, "listen").mockResolvedValue(() => {});
    selectMachine(remote);
    await expect(disconnected.invoke("list_sessions")).rejects.toThrow("Connect to a machine first");
    await expect(disconnected.listen("session-updated", vi.fn())).rejects.toThrow("Connect to a machine first");
    expect(subscribe).not.toHaveBeenCalled();
  });
  it("does not let an old owner's cleanup deselect its successor", async () => {
    const old = client(); const next = client();
    const send = vi.spyOn(next, "invoke").mockResolvedValue({ selected: true });
    const close = vi.spyOn(next, "close");
    selectMachine(old); selectMachine(next); deselectMachine(old);
    expect(close).not.toHaveBeenCalled();
    expect(await invoke("list_sessions")).toEqual({ selected: true });
    expect(send).toHaveBeenCalledOnce();
    deselectMachine(next);
    expect(close).toHaveBeenCalledOnce();
    await expect(invoke("list_sessions")).rejects.toThrow("Connect to a machine first");
  });
  it("routes a selected Hub machine before desktop local commands and partitions its viewer state", async () => {
    vi.stubGlobal("__TAURI_INTERNALS__", {});
    const remote = client();
    const send = vi.spyOn(remote, "invoke").mockResolvedValue({ sessions: [] });
    const subscribe = vi.spyOn(remote, "listen").mockResolvedValue(() => {});
    selectMachine(remote);
    expect(await invoke("list_sessions", { request: {} })).toEqual({ sessions: [] });
    await listen("session-updated", () => {});
    expect(send).toHaveBeenCalledWith("list_sessions", { request: {} });
    expect(subscribe).toHaveBeenCalledWith("session-updated", expect.any(Function));
    expect(viewerStorageScope()).toBe(remote.endpoint);
    expect(await captureTransport().invoke("list_sessions")).toEqual({ sessions: [] });
  });
  it("parses named multiline events and ignores heartbeats", () => {
    expect(parseEvent(": keep-alive")).toBeNull();
    expect(parseEvent('event: relay-changed\ndata: {"session_key":null,\ndata: "reset":true}')).toEqual({ event: "relay-changed", payload: { session_key: null, reset: true } });
  });
  it("rejects incompatible APIs before selecting a machine", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ version: 9 })));
    await expect(RemoteClient.connect("http://machine", "secret")).rejects.toThrow("unsupported");
  });
  it("cancels pending host discovery when the Hub session ends", async () => {
    let request_signal: AbortSignal | undefined;
    vi.stubGlobal("fetch", vi.fn((_url: string, options: RequestInit) => new Promise((_resolve, reject) => {
      request_signal = options.signal as AbortSignal;
      request_signal.addEventListener("abort", () => reject(new Error("aborted")));
    })));
    const lifetime = new AbortController();
    const connecting = RemoteClient.connect("https://hub.example/hosts/workstation", "secret", lifetime.signal);
    const rejected = expect(connecting).rejects.toThrow("aborted");
    lifetime.abort();
    await rejected;
    expect(request_signal?.aborted).toBe(true);
  });
  it("uses bearer headers and aborts outstanding requests on machine switch", async () => {
    let signal: AbortSignal | undefined;
    const fetcher = vi.fn((_url: string, options: RequestInit) => new Promise((_resolve, reject) => {
      signal = options.signal as AbortSignal;
      signal.addEventListener("abort", () => reject(new Error("aborted")));
    }));
    vi.stubGlobal("fetch", fetcher);
    selectMachine(client());
    const request = invoke("list_sessions", { request: {} });
    const rejected = expect(request).rejects.toThrow("aborted");
    selectMachine(client());
    await rejected;
    expect(signal?.aborted).toBe(true);
    expect(fetcher.mock.calls[0][1]).toMatchObject({ headers: { Authorization: "Bearer secret" }, credentials: "omit", redirect: "error" });
  });
  it("releases a captured view on its original machine before clearing its credentials", async () => {
    const fetcher = vi.fn().mockImplementation(() => Promise.resolve(Response.json(null)));
    vi.stubGlobal("fetch", fetcher);
    const old = client();
    selectMachine(old);
    const captured = captureTransport();
    captured.on_close(() => { void captured.release("update_session_view", { request: { view_id: "view", session_key: null } }); });
    const next = new RemoteClient("http://next:5558", "next-secret");
    clients.push(next);
    selectMachine(next);
    expect(fetcher).toHaveBeenCalledExactlyOnceWith("http://machine:5558/api/v1/update_session_view", expect.objectContaining({
      headers: { Authorization: "Bearer secret", "Content-Type": "application/json" }, keepalive: true,
    }));
    await expect(captured.invoke("load_event_page", {})).rejects.toThrow("Machine disconnected");
    expect(fetcher).toHaveBeenCalledOnce();
  });
  it("returns verified WebSocket lease acknowledgements and exposes live loss even while the SSE stream remains connected", async () => {
    vi.useFakeTimers(); vi.stubGlobal("WebSocket", LiveSocket);
    const feed = stream();
    const fetcher = vi.fn().mockResolvedValueOnce(Response.json({ version: 1, live_updates: true })).mockResolvedValueOnce(feed.response);
    vi.stubGlobal("fetch", fetcher);
    const remote = await RemoteClient.connect("http://machine:5558", "secret"); clients.push(remote);
    const lost = vi.fn(), restored = vi.fn(), relay = vi.fn();
    const listening = remote.listen("session-subscriptions-lost", lost);
    feed.send("event: ready\ndata: {}\n\n");
    const stop = await listening;
    const stopRestored = await remote.listen("transport-reconnected", restored);
    const stopRelay = await remote.listen("relay-changed", relay);
    const request = { session_key: "one", subscription_id: "subscription", level: "steps", cursor: null, detail_keys: [] };
    const subscribing = remote.invoke("subscribe_session", { request });
    const socket = LiveSocket.instances[0]; socket.open(); await subscribing;

    const renewing = remote.invoke("renew_session_subscriptions", { ids: [request.subscription_id] });
    expect(socket.sent[socket.sent.length - 1]).toEqual({ kind: "ping" });
    socket.frame({ kind: "pong", renewed_ids: [request.subscription_id], missing_ids: [] });
    expect(await renewing).toEqual({ renewed_ids: [request.subscription_id], missing_ids: [] });
    expect(lost).not.toHaveBeenCalled();
    expect(restored).not.toHaveBeenCalled();
    expect(fetcher).toHaveBeenCalledTimes(2);

    const expired = remote.invoke("renew_session_subscriptions", { ids: [request.subscription_id] });
    socket.frame({ kind: "pong", renewed_ids: [], missing_ids: [request.subscription_id] });
    expect(await expired).toEqual({ renewed_ids: [], missing_ids: [request.subscription_id] });
    expect(lost).toHaveBeenCalledWith({ payload: { subscription_ids: [request.subscription_id] } });
    await vi.advanceTimersByTimeAsync(0);
    expect(restored).toHaveBeenCalledWith({ payload: { subscription_ids: [request.subscription_id] } });
    expect(relay).not.toHaveBeenCalled();
    lost.mockClear(); socket.close();
    expect(lost).toHaveBeenCalledWith({ payload: { subscription_ids: [request.subscription_id] } });
    await expect(remote.invoke("renew_session_subscriptions", { ids: [request.subscription_id] })).rejects.toThrow("Live connection interrupted");
    expect(fetcher).toHaveBeenCalledTimes(2);
    stop(); stopRestored(); stopRelay(); remote.close(); feed.end();
  });
  it.each([
    ["load_session_backward", false], ["load_session_details", false],
    ["load_session_backward", true], ["load_session_details", true],
  ])("does not let a late %s response revive or overwrite a registration after release (successor=%s)", async (command, successor) => {
    vi.useFakeTimers(); vi.stubGlobal("WebSocket", LiveSocket);
    let finishRead!: (response: Response) => void;
    const fetcher = vi.fn().mockResolvedValueOnce(Response.json({ version: 1, live_updates: true }))
      .mockReturnValueOnce(new Promise<Response>((resolve) => { finishRead = resolve; }));
    vi.stubGlobal("fetch", fetcher);
    const remote = await RemoteClient.connect("http://machine:5558", "secret"); clients.push(remote);
    const request = { session_key: "one", subscription_id: "subscription", level: "steps", cursor: null, detail_keys: [],
      scope: { history: "latest_turn", group_keys: [] } };
    const subscribing = remote.invoke("subscribe_session", { request });
    const socket = LiveSocket.instances[0]; socket.open(); await subscribing;
    const oldRead = { ...request, scope: { ...request.scope, group_keys: ["old-group"] } };
    const payload = command === "load_session_details" ? { request: { kind: "group", request: oldRead } } : { request: oldRead };
    const reading = remote.invoke(command, payload);
    await remote.invoke("subscribe_session", { request: { ...request, unsubscribe: true } });
    const nextRequest = { ...request, session_key: "two", scope: { ...request.scope, group_keys: ["new-group"] } };
    if (successor) await remote.invoke("subscribe_session", { request: nextRequest });
    finishRead(Response.json({ revision: "late" }));
    expect(await reading).toEqual({ revision: "late" });

    const renewing = remote.invoke("renew_session_subscriptions", { ids: [request.subscription_id] });
    socket.frame({ kind: "pong", renewed_ids: [request.subscription_id], missing_ids: [] });
    expect(await renewing).toEqual(successor
      ? { renewed_ids: [request.subscription_id], missing_ids: [] }
      : { renewed_ids: [], missing_ids: [request.subscription_id] });
    socket.close(); await vi.advanceTimersByTimeAsync(1000);
    const next = LiveSocket.instances[1]; next.open(); await vi.advanceTimersByTimeAsync(0);
    expect(next.sent.filter((frame) => frame.kind === "subscribe").map((frame) => frame.request)).toEqual(successor ? [nextRequest] : []);
    remote.close();
  });
  it("keeps successful backward-read coverage on its active registration for reconnect", async () => {
    vi.useFakeTimers(); vi.stubGlobal("WebSocket", LiveSocket);
    vi.stubGlobal("fetch", vi.fn().mockResolvedValueOnce(Response.json({ version: 1, live_updates: true }))
      .mockResolvedValueOnce(Response.json({ revision: "2" })));
    const remote = await RemoteClient.connect("http://machine:5558", "secret"); clients.push(remote);
    const request = { session_key: "one", subscription_id: "subscription", level: "steps", cursor: null, detail_keys: [] };
    const subscribing = remote.invoke("subscribe_session", { request });
    const socket = LiveSocket.instances[0]; socket.open(); await subscribing;
    const coverage = { ...request, scope: { history: "retained", group_keys: ["activity:one"] }, history_cursor: "older" };
    await remote.invoke("load_session_backward", { request: coverage });
    socket.close(); await vi.advanceTimersByTimeAsync(1000);
    const next = LiveSocket.instances[1]; next.open(); await vi.advanceTimersByTimeAsync(0);
    expect(next.sent.filter((frame) => frame.kind === "subscribe").map((frame) => frame.request)).toEqual([{ ...coverage, history_cursor: undefined }]);
    remote.close();
  });
  it("shares one stream, waits for readiness, and refreshes after reconnect", async () => {
    vi.useFakeTimers();
    const first = stream(); const second = stream();
    const fetcher = vi.fn().mockResolvedValueOnce(first.response).mockResolvedValueOnce(second.response);
    vi.stubGlobal("fetch", fetcher);
    const remote = client();
    const changed = vi.fn(); const progress = vi.fn(); const restored = vi.fn();
    const one = remote.listen("relay-changed", changed);
    const two = remote.listen("session-index-progress", progress);
    const three = remote.listen("transport-reconnected", restored);
    first.send("event: rea"); first.send("dy\ndata: {}\n\n");
    const [stopOne, stopTwo, stopThree] = await Promise.all([one, two, three]);
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(restored).not.toHaveBeenCalled();
    first.send('event: session-index-progress\ndata: {"pending":1}\n\n');
    await vi.advanceTimersByTimeAsync(0);
    expect(progress).toHaveBeenCalledWith({ payload: { pending: 1 } });
    first.end();
    await vi.advanceTimersByTimeAsync(1100);
    second.send("event: ready\ndata: {}\n\n");
    await vi.advanceTimersByTimeAsync(0);
    expect(restored).toHaveBeenCalledOnce();
    expect(changed).toHaveBeenCalledWith({ payload: { session_key: null, reset: true } });
    stopOne(); stopTwo(); stopThree(); remote.close(); second.end();
  });
  it("uses the live socket for compact subscriptions and omits duplicate SSE pushes", async () => {
    const feed = stream();
    const fetcher = vi.fn((url: string) => Promise.resolve(url.endsWith("/health")
      ? Response.json({ version: 1, live_updates: true }) : feed.response));
    vi.stubGlobal("fetch", fetcher); vi.stubGlobal("WebSocket", LiveSocket);
    const remote = await RemoteClient.connect("http://machine:5558", "secret"); clients.push(remote);
    const changed = vi.fn();
    const listening = remote.listen("background-session-updated", changed);
    feed.send("event: ready\ndata: {}\n\n");
    await vi.waitFor(() => expect(LiveSocket.instances).toHaveLength(1));
    const socket = LiveSocket.instances[0]; socket.open();
    const stop = await listening;
    expect(fetcher.mock.calls[1][0]).toBe("http://machine:5558/api/v1/events?session_updates=false&background_updates=false");
    expect(await remote.invoke("subscribe_background_updates", { request: background })).toEqual({
      subscription_id: "compact", generation: "compact-generation", revision: "0", scope: "sidebar", subscribed: true,
    });
    expect(socket.sent[1]).toMatchObject({ kind: "subscribe_background", request: background });
    const renewing = remote.invoke("renew_background_subscriptions", { ids: ["compact"] });
    socket.frame({ kind: "pong", renewed_ids: [], missing_ids: [], renewed_background_ids: ["compact"], missing_background_ids: [] });
    await renewing;
    const payload = { subscription_id: "compact", session_key: "one", latest_assistant_message: { preview: "Reply" } };
    socket.frame({ kind: "event", event: "background-session-updated", payload });
    expect(changed).toHaveBeenCalledWith({ payload });
    await remote.release("subscribe_background_updates", { request: { ...background, unsubscribe: true } });
    expect(socket.sent[3]).toMatchObject({ kind: "subscribe_background", request: { ...background, unsubscribe: true } });
    socket.close();
    await remote.release("subscribe_background_updates", { request: { ...background, unsubscribe: true } });
    expect(fetcher).toHaveBeenCalledTimes(2);
    stop(); remote.close(); feed.end();
  });
  it("retains HTTP and SSE compact delivery for tunnels without a live socket", async () => {
    const feed = stream();
    const fetcher = vi.fn((url: string) => Promise.resolve(url.endsWith("/events") ? feed.response : Response.json(null)));
    vi.stubGlobal("fetch", fetcher);
    const remote = client(); const changed = vi.fn();
    const listening = remote.listen("background-session-updated", changed);
    feed.send("event: ready\ndata: {}\n\n"); const stop = await listening;
    await remote.invoke("subscribe_background_updates", { request: background });
    await remote.invoke("renew_background_subscriptions", { ids: ["compact"] });
    const payload = { subscription_id: "compact", session_key: "one" };
    feed.send(`event: background-session-updated\ndata: ${JSON.stringify(payload)}\n\n`);
    await vi.waitFor(() => expect(changed).toHaveBeenCalledWith({ payload }));
    await remote.release("subscribe_background_updates", { request: { ...background, unsubscribe: true } });
    expect(fetcher.mock.calls.map(([url]) => url)).toEqual([
      "http://machine:5558/api/v1/events", "http://machine:5558/api/v1/subscribe_background_updates",
      "http://machine:5558/api/v1/renew_background_subscriptions", "http://machine:5558/api/v1/subscribe_background_updates",
    ]);
    stop(); remote.close(); feed.end();
  });
  it("accepts a large chunk of individually bounded SSE frames", async () => {
    const feed = stream();
    const fetcher = vi.fn().mockResolvedValue(feed.response);
    vi.stubGlobal("fetch", fetcher);
    const remote = client();
    const changed = vi.fn();
    const listening = remote.listen("relay-changed", changed);
    feed.send("event: ready\ndata: {}\n\n");
    const stop = await listening;
    const payload = JSON.stringify({ session_key: null, reset: false, text: "x".repeat(700_000) });
    const frame = `event: relay-changed\ndata: ${payload}\n\n`;
    feed.send(frame.repeat(3));
    await vi.waitFor(() => expect(changed).toHaveBeenCalledTimes(3));
    expect(fetcher).toHaveBeenCalledOnce();
    stop(); remote.close(); feed.end();
  });
  it("accepts all-level session payloads above the notification limit across chunks", async () => {
    const feed = stream();
    const fetcher = vi.fn().mockResolvedValue(feed.response);
    vi.stubGlobal("fetch", fetcher);
    const remote = client();
    const changed = vi.fn();
    const listening = remote.listen("session-updated", changed);
    feed.send("event: ready\ndata: {}\n\n");
    const stop = await listening;
    const payload = { level: "all", items: [{ kind: "event", event: "x".repeat(2 * 1024 * 1024 + 1) }] };
    const frame = `event: session-updated\ndata: ${JSON.stringify(payload)}\n\n`;
    feed.send(frame.slice(0, -2));
    feed.send("\n\n");
    await vi.waitFor(() => expect(changed).toHaveBeenCalledWith({ payload }));
    expect(fetcher).toHaveBeenCalledOnce();
    stop(); remote.close(); feed.end();
  });
  it("reconnects when one SSE frame exceeds the size limit", async () => {
    vi.useFakeTimers();
    const first = stream(); const second = stream();
    const fetcher = vi.fn().mockResolvedValueOnce(first.response).mockResolvedValueOnce(second.response);
    vi.stubGlobal("fetch", fetcher);
    const remote = client();
    const listening = remote.listen("relay-changed", vi.fn());
    first.send("event: ready\ndata: {}\n\n");
    const stop = await listening;
    first.send(`event: relay-changed\ndata: ${"x".repeat(2 * 1024 * 1024)}\n\n`);
    await vi.advanceTimersByTimeAsync(1100);
    expect(fetcher).toHaveBeenCalledTimes(2);
    stop(); remote.close(); second.end();
  });
});
