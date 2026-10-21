import { beforeEach, expect, it, vi } from "vitest";
import { invoke, type CommandInvoker } from "./transport";
import { getSessionModelStatus, listSessionModels, inspectSessionEvent, loadGroupDetails, loadSessionUpdates, loadToolDetails, renewSessionSubscriptions, submitSessionInput } from "./tauri";
import type { SessionUpdatesRequest, SessionUpdate } from "./types";

vi.mock("./transport", () => ({ invoke: vi.fn(), listen: vi.fn(), isDesktop: () => false }));
beforeEach(() => vi.mocked(invoke).mockReset());

it("routes model reads and mutation through the captured session machine without local fallback", async () => {
  const { send, calls } = machineInvoker();
  calls.mockResolvedValue({});
  const target = { session_key: "one" };
  const message = { ...target, request_id: "uuid", text: "next", model_selection: { model_provider: "anthropic", model_id: "sonnet", expected_revision: "revision", runtime_generation: "pi-runtime" } };
  await getSessionModelStatus(target, send);
  await listSessionModels(target, send);
  await submitSessionInput(message, send);
  expect(calls.mock.calls).toEqual([
    ["get_session_model_status", { request: target }],
    ["list_session_models", { request: target }],
    ["submit_session_input", { request: message }],
  ]);
  expect(invoke).not.toHaveBeenCalled();
});
const request: SessionUpdatesRequest = { subscription_id: "modern", session_key: "one", level: "steps", cursor: null, detail_keys: [], scope: { history: "latest_turn", group_keys: [] } };
const update: SessionUpdate = { ...request, generation: "source", revision: "1", snapshot: true, base_revision: null,
  items: [], groups: [], removed_items: [], item_order: [], state: { total_events: 0, previous_cursor: null, next_cursor: null, history_status: "complete" } };

it("subscribes before backward loading and never fetches a legacy update on modern servers", async () => {
  vi.mocked(invoke).mockResolvedValueOnce({ revision: "0" }).mockResolvedValueOnce(update);
  expect(await loadSessionUpdates(request)).toBe(update);
  expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toEqual(["subscribe_session", "load_session_backward"]);
});
it("subscribes before loading an explicit complete group without reading backward", async () => {
  let completeSubscription!: (value: unknown) => void;
  vi.mocked(invoke).mockReturnValueOnce(new Promise((resolve) => { completeSubscription = resolve; }))
    .mockResolvedValueOnce(update);
  const group = { ...request, scope: { ...request.scope!, group_keys: ["activity:a"] } };
  const loading = loadGroupDetails(group);
  expect(invoke).toHaveBeenCalledExactlyOnceWith("subscribe_session", { request: group });
  completeSubscription({ revision: "0" });
  expect(await loading).toBe(update);
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["subscribe_session", { request: group }],
    ["load_session_details", { request: { kind: "group", request: group } }],
  ]);
});

it.each(["subscribe_session", "load_session_details"])("falls back and renews a legacy group when %s is unavailable", async (unavailable) => {
  const { send, calls } = machineInvoker();
  const group = { ...request, subscription_id: "legacy-group", scope: { ...request.scope!, group_keys: ["activity:a"] } };
  if (unavailable === "load_session_details") calls.mockResolvedValueOnce({ revision: "0" });
  calls.mockRejectedValueOnce(new Error("Unknown viewer API route"))
    .mockResolvedValueOnce({ ...update, subscription_id: group.subscription_id });
  await loadGroupDetails(group, send);
  expect(calls.mock.calls.map(([command]) => command)).toEqual(unavailable === "subscribe_session"
    ? ["subscribe_session", "load_session_updates"]
    : ["subscribe_session", "load_session_details", "load_session_updates"]);
  calls.mockRejectedValueOnce(new Error("Unknown viewer API route"))
    .mockResolvedValueOnce({ ...update, subscription_id: group.subscription_id, revision: "2" });
  const renewed = await renewSessionSubscriptions([group.subscription_id], send);
  expect(renewed.renewed_ids).toEqual([group.subscription_id]);
  expect(calls).toHaveBeenLastCalledWith("load_session_updates", { request: { ...group, cursor: "1" } });
  expect(invoke).not.toHaveBeenCalled();
});

it("does not load a group after its live registration fails for a non-compatibility reason", async () => {
  vi.mocked(invoke).mockRejectedValueOnce(new Error("Live connection interrupted"));
  await expect(loadGroupDetails(request)).rejects.toThrow("Live connection interrupted");
  expect(invoke).toHaveBeenCalledExactlyOnceWith("subscribe_session", { request });
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
  expect(await renewSessionSubscriptions([request.subscription_id])).toEqual({ renewed_ids: [request.subscription_id], missing_ids: [], updates: [] });
  expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toEqual(["subscribe_session", "renew_session_subscriptions"]);
});
it("renews legacy leases with their revision and returns recovery changes", async () => {
  const legacy = { ...request, subscription_id: "legacy" };
  vi.mocked(invoke).mockRejectedValueOnce(new Error("Unknown viewer API route")).mockResolvedValueOnce({ ...update, subscription_id: "legacy" });
  await loadSessionUpdates(legacy);
  vi.mocked(invoke).mockRejectedValueOnce(new Error("Unknown viewer API route")).mockResolvedValueOnce({ ...update, subscription_id: "legacy", revision: "2" });
  const changes = await renewSessionSubscriptions(["legacy"]);
  expect(changes.updates[0].revision).toBe("2");
  expect(invoke).toHaveBeenLastCalledWith("load_session_updates", { request: { ...legacy, cursor: "1" } });
});

it("reports expired modern interests without fetching history during renewal", async () => {
  vi.mocked(invoke).mockResolvedValue({ renewed_ids: ["live"], missing_ids: ["expired"] });
  expect(await renewSessionSubscriptions(["live", "expired"])).toEqual({
    renewed_ids: ["live"], missing_ids: ["expired"], updates: [],
  });
  expect(invoke).toHaveBeenCalledExactlyOnceWith("renew_session_subscriptions", { ids: ["live", "expired"] });
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
    "subscribe_session", "load_session_updates",
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
