import { beforeEach, expect, it, vi } from "vitest";
import { invoke, type CommandInvoker } from "./transport";
import { inspectSessionEvent, loadGroupDetails, loadSessionUpdates, loadToolDetails, renewSessionSubscriptions } from "./tauri";
import type { SessionUpdatesRequest, SessionUpdate } from "./types";

vi.mock("./transport", () => ({ invoke: vi.fn(), listen: vi.fn(), isDesktop: () => false }));
beforeEach(() => vi.mocked(invoke).mockReset());
const request: SessionUpdatesRequest = { subscription_id: "modern", session_key: "one", level: "steps", cursor: null, detail_keys: [], scope: { history: "latest_turn", group_keys: [] } };
const update: SessionUpdate = { ...request, generation: "source", revision: "1", snapshot: true, base_revision: null,
  items: [], groups: [], removed_items: [], item_order: [], state: { total_events: 0, previous_cursor: null, next_cursor: null, history_status: "complete" } };

it("subscribes before backward loading and never fetches a legacy update on modern servers", async () => {
  vi.mocked(invoke).mockResolvedValueOnce({ revision: "0" }).mockResolvedValueOnce(update);
  expect(await loadSessionUpdates(request)).toBe(update);
  expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toEqual(["subscribe_session", "load_session_backward"]);
});
it("loads an explicit complete group through details without reading backward", async () => {
  vi.mocked(invoke).mockResolvedValue(update);
  const group = { ...request, scope: { ...request.scope!, group_keys: ["activity:a"] } };
  await loadGroupDetails(group);
  expect(invoke).toHaveBeenCalledExactlyOnceWith("load_session_details", { request: { kind: "group", request: group } });
});
it("keeps tool display and source inspection requests distinct", async () => {
  vi.mocked(invoke).mockResolvedValue({});
  const event = { session_key: "one", event_key: "tool" };
  await loadToolDetails(event); await inspectSessionEvent(event);
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["load_session_details", { request: { kind: "tool", request: event } }],
    ["inspect_session_event", { request: event }],
  ]);
});
it("unsubscribes and renews without fetching any display data", async () => {
  vi.mocked(invoke).mockResolvedValue(null);
  await loadSessionUpdates({ ...request, unsubscribe: true });
  expect(await renewSessionSubscriptions([request.subscription_id])).toEqual([]);
  expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toEqual(["subscribe_session", "renew_session_subscriptions"]);
});
it("renews legacy leases with their revision and returns recovery changes", async () => {
  const legacy = { ...request, subscription_id: "legacy" };
  vi.mocked(invoke).mockRejectedValueOnce(new Error("Unknown viewer API route")).mockResolvedValueOnce({ ...update, subscription_id: "legacy" });
  await loadSessionUpdates(legacy);
  vi.mocked(invoke).mockRejectedValueOnce(new Error("Unknown viewer API route")).mockResolvedValueOnce({ ...update, subscription_id: "legacy", revision: "2" });
  const changes = await renewSessionSubscriptions(["legacy"]);
  expect(changes[0].revision).toBe("2");
  expect(invoke).toHaveBeenLastCalledWith("load_session_updates", { request: { ...legacy, cursor: "1" } });
});

function machineInvoker() {
  const calls = vi.fn<(command: string, payload?: Record<string, unknown>) => Promise<unknown>>();
  const send: CommandInvoker = <T,>(command: string, payload?: Record<string, unknown>) => calls(command, payload) as Promise<T>;
  return { send, calls };
}

it("keeps subscription and deferred backward loading on the captured machine", async () => {
  let completeSubscription!: (value: unknown) => void;
  const { send, calls } = machineInvoker();
  calls.mockReturnValueOnce(new Promise((resolve) => { completeSubscription = resolve; }))
    .mockResolvedValueOnce(update);
  const loading = loadSessionUpdates(request, send);
  expect(calls).toHaveBeenCalledExactlyOnceWith("subscribe_session", { request });
  expect(invoke).not.toHaveBeenCalled();
  completeSubscription(null);
  expect(await loading).toBe(update);
  expect(calls.mock.calls.map(([command]) => command)).toEqual(["subscribe_session", "load_session_backward"]);
  expect(invoke).not.toHaveBeenCalled();
});

it("keeps details and inspection compatibility fallbacks on the captured machine", async () => {
  const { send, calls } = machineInvoker();
  const event = { session_key: "one", event_key: "tool" };
  for (const load of [
    () => loadGroupDetails(request, send),
    () => loadToolDetails(event, send),
    () => inspectSessionEvent(event, send),
  ]) {
    calls.mockRejectedValueOnce(new Error("Unknown viewer API route")).mockResolvedValueOnce(update);
    await load();
  }
  expect(calls.mock.calls.map(([command]) => command)).toEqual([
    "load_session_details", "load_session_updates",
    "load_session_details", "load_event_detail",
    "inspect_session_event", "load_event_detail",
  ]);
  expect(invoke).not.toHaveBeenCalled();
});

it("isolates legacy lease renewal when different machines use the same subscription ID", async () => {
  const first = machineInvoker();
  const second = machineInvoker();
  const firstRequest = { ...request, subscription_id: "shared", session_key: "first" };
  const secondRequest = { ...request, subscription_id: "shared", session_key: "second" };
  for (const [machine, subscription, revision] of [[first, firstRequest, "11"], [second, secondRequest, "22"]] as const) {
    machine.calls.mockRejectedValueOnce(new Error("Unknown viewer API route"))
      .mockResolvedValueOnce({ ...update, ...subscription, revision });
    await loadSessionUpdates(subscription, machine.send);
  }
  first.calls.mockRejectedValueOnce(new Error("Unknown viewer API route"))
    .mockResolvedValueOnce({ ...update, ...firstRequest, revision: "12" });
  await renewSessionSubscriptions(["shared"], first.send);
  expect(first.calls).toHaveBeenLastCalledWith("load_session_updates", { request: { ...firstRequest, cursor: "11" } });
  second.calls.mockRejectedValueOnce(new Error("Unknown viewer API route"))
    .mockResolvedValueOnce({ ...update, ...secondRequest, revision: "23" });
  await renewSessionSubscriptions(["shared"], second.send);
  expect(second.calls).toHaveBeenLastCalledWith("load_session_updates", { request: { ...secondRequest, cursor: "22" } });
  expect(invoke).not.toHaveBeenCalled();
});
