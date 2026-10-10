import { beforeEach, expect, it, vi } from "vitest";
import { invoke } from "./transport";
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
