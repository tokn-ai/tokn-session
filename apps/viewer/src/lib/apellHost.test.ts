import { afterEach, expect, it, vi } from "vitest";
import { createApellHost, type AppletBridge } from "./apellHost";
import { installViewerHost, viewerCapabilities } from "./platform";
import { NativeHubClient } from "./hubNativeClient";
import { captureTransport, invoke, selectMachine } from "./transport";

const hosts: ReturnType<typeof createApellHost>[] = [];
const restores: (() => void)[] = [];
afterEach(() => { selectMachine(); for (const host of hosts.splice(0)) host.dispose?.(); for (const restore of restores.splice(0).reverse()) restore(); vi.useRealTimers(); });
const host_pin = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43) };
const descriptor = { ...host_pin, connection_id: "connection", endpoint: "https://hub.example/encrypted/machine", transport: { kind: "relay" } };
const encoded = (text: string) => btoa(String.fromCharCode(...new TextEncoder().encode(text))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=/g, "");

function fixture() {
  let event!: (event: { name: string; payload: unknown }) => void;
  const send = vi.fn<(command: string, params: Record<string, unknown>) => Promise<unknown>>().mockResolvedValue(null);
  const unsubscribe = vi.fn();
  const bridge: AppletBridge = {
    invoke: <T>(command: string, params: Record<string, unknown>) => send(command, params) as Promise<T>,
    onEvent: (handler) => { event = handler; return unsubscribe; },
  };
  const host = createApellHost(bridge); hosts.push(host); restores.push(installViewerHost(host));
  return { host, send, unsubscribe, event: (name: string, payload: unknown) => event({ name, payload }) };
}

it("advertises app trust without desktop capabilities and never routes local calls to the guest", async () => {
  const { send } = fixture();
  expect(viewerCapabilities()).toMatchObject({ native_hub: true, device_preferences: true, local_machine: false, passkeys: false, hosting: false });
  await expect(invoke("list_sessions")).rejects.toThrow("Connect to a machine first");
  await expect(captureTransport().invoke("list_sessions")).rejects.toThrow("Connect to a machine first");
  expect(send).not.toHaveBeenCalled();
});

it("reassembles UTF-8 response chunks in the frontend and acknowledges each accepted chunk", async () => {
  const value = fixture();
  value.send.mockImplementation(async (command, params) => {
    if (command !== "hub_client_request") return null;
    const id = params.response_id as string;
    value.event("hub-client-response", { response_id: id, channel_id: id, sequence: 0, kind: "chunk", data: encoded('{"preview":"你好') });
    value.event("hub-client-response", { response_id: id, channel_id: id, sequence: 1, kind: "chunk", data: encoded('"}') });
    return { response_id: id, status: 200, content_type: "application/json" };
  });
  await expect(value.host.invoke("hub_client_request", { connection_id: "connection", command: "list_sessions" })).resolves.toEqual({ preview: "你好" });
  expect(value.send.mock.calls.filter(([name]) => name === "ack_bridge_chunk").map(([, params]) => params.sequence)).toEqual([0, 1]);
});

it("retains uncertain input errors and never retries a request or accepts a changed chunk sequence", async () => {
  const value = fixture();
  value.send.mockImplementation(async (command, params) => {
    if (command !== "hub_client_request") return null;
    const id = params.response_id as string;
    value.event("hub-client-response", { response_id: id, channel_id: id, sequence: 1, kind: "chunk", data: encoded('{}') });
    return { response_id: id, status: 200, content_type: "application/json" };
  });
  await expect(value.host.invoke("hub_client_request", { connection_id: "connection", command: "submit_session_input" })).rejects.toThrow("chunk sequence");
  expect(value.send.mock.calls.filter(([name]) => name === "hub_client_request")).toHaveLength(1);
  expect(value.send.mock.calls.filter(([name]) => name === "ack_bridge_chunk")).toHaveLength(0);
});

it("loads a history response larger than one guest ABI message without a guest-sized JSON envelope", async () => {
  const value = fixture();
  const data = encoded("x".repeat(32 * 1024));
  value.send.mockImplementation(async (command, params) => {
    if (command !== "hub_client_request") return null;
    const id = params.response_id as string;
    let sequence = 0;
    const chunk = (data: string) => value.event("hub-client-response", { response_id: id, channel_id: id, sequence: sequence++, kind: "chunk", data });
    chunk(encoded('{"history":"'));
    for (let index = 0; index < 288; index += 1) chunk(data);
    chunk(encoded('"}'));
    return { response_id: id, status: 200, content_type: "application/json" };
  });
  const result = await value.host.invoke<{ history: string }>("hub_client_request", { connection_id: "connection", command: "load_session_backward" });
  expect(result.history).toHaveLength(9 * 1024 * 1024);
  expect(value.send.mock.calls.filter(([name]) => name === "ack_bridge_chunk")).toHaveLength(290);
});

it("subscribes before the pump, parses live chunks and restores a generation without replaying input", async () => {
  const value = fixture();
  value.send.mockImplementation(async (command) => {
    if (command === "hub_client_open") return descriptor;
    if (command === "hub_client_listen") {
      value.event("hub-client-stream-reset", { connection_id: "connection", generation: "first" });
      value.event("hub-client-stream-chunk", { connection_id: "connection", generation: "first", channel_id: "connection:first", sequence: 0, data: encoded('event: ready\ndata: {}\n\nevent: session-updated\ndata: {"revision":"1"}\n\n') });
    }
    return null;
  });
  const client = await NativeHubClient.connect("https://hub.example", host_pin);
  const changed = vi.fn(); const state = vi.fn(); client.setStateListener(state);
  await client.listen("session-updated", changed);
  expect(changed).toHaveBeenCalledWith({ payload: { revision: "1" } });
  expect(state).toHaveBeenLastCalledWith("connected");
  value.event("hub-client-stream-reset", { connection_id: "connection", generation: "second" });
  value.event("hub-client-stream-chunk", { connection_id: "connection", generation: "first", channel_id: "connection:first", sequence: 1, data: encoded('event: session-updated\ndata: {"revision":"old"}\n\n') });
  value.event("hub-client-stream-chunk", { connection_id: "connection", generation: "second", channel_id: "connection:second", sequence: 0, data: encoded('event: ready\ndata: {}\n\nevent: session-updated\ndata: {"revision":"2"}\n\n') });
  expect(changed).toHaveBeenCalledTimes(2);
  expect(changed).toHaveBeenLastCalledWith({ payload: { revision: "2" } });
  expect(value.send.mock.calls.some(([name]) => name === "hub_client_request")).toBe(false);
  client.close();
});

it("closes a live connection that misses readiness and releases view subscriptions", async () => {
  vi.useFakeTimers();
  const value = fixture();
  const ready = value.host.invoke("hub_client_listen", { connection_id: "connection" });
  const result = expect(ready).rejects.toThrow("did not become ready");
  await vi.advanceTimersByTimeAsync(30_000); await result;
  expect(value.send).toHaveBeenCalledWith("hub_client_close", { connection_id: "connection" });
  value.host.dispose?.(); expect(value.unsubscribe).toHaveBeenCalledOnce();
});

it("rejects malformed transport payloads without throwing out of the bridge listener", async () => {
  const value = fixture();
  value.send.mockImplementation(async (command, params) => {
    if (command !== "hub_client_request") return null;
    expect(() => value.event("hub-client-response", null)).not.toThrow();
    return { response_id: params.response_id, status: 200, content_type: "application/json" };
  });
  await expect(value.host.invoke("hub_client_request", { connection_id: "connection", command: "list_sessions" })).rejects.toThrow("invalid transport event");
  expect(value.send).toHaveBeenCalledWith("hub_client_close", { connection_id: "connection" });
  expect(value.send.mock.calls.some(([name]) => name === "ack_bridge_chunk")).toBe(false);
});

it("isolates a failing view observer while preserving the next observer and live acknowledgement", async () => {
  const value = fixture();
  const received = vi.fn();
  await value.host.listen("hub-client-event", () => { throw new Error("view callback failed"); });
  await value.host.listen("hub-client-event", received);
  value.send.mockImplementation(async (command) => {
    if (command === "hub_client_listen") {
      value.event("hub-client-stream-reset", { connection_id: "connection", generation: "first" });
      value.event("hub-client-stream-chunk", { connection_id: "connection", generation: "first", channel_id: "connection:first", sequence: 0, data: encoded('event: ready\ndata: {}\n\nevent: session-updated\ndata: {"revision":"1"}\n\n') });
    }
    return null;
  });
  await value.host.invoke("hub_client_listen", { connection_id: "connection" });
  expect(received).toHaveBeenCalledWith({ payload: { connection_id: "connection", event: "session-updated", payload: { revision: "1" } } });
  expect(value.send).toHaveBeenCalledWith("ack_bridge_chunk", { channel_id: "connection:first", sequence: 0 });
  expect(value.send.mock.calls.some(([name]) => name === "hub_client_close")).toBe(false);
});
