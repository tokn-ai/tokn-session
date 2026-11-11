import { act, cleanup, fireEvent, renderHook, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { HubAccessService } from "./hubAccess";
import type { PresenceResult } from "./types";
import { PRESENCE_REFRESH_MS, useHubPresence } from "./useHubPresence";

const uuid = (index: number) => `550e8400-e29b-41d4-a716-${index.toString(16).padStart(12, "0")}`;
const first = uuid(1);
const second = uuid(2);
const reply = (ids: string[], online = true): PresenceResult => ({ machines: ids.map((host_id) => ({ host_id, online })) });
const service = (presence: HubAccessService["presence"]) => ({ hub_url: "https://hub.example", presence }) as HubAccessService;
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
let visible: DocumentVisibilityState;
beforeEach(() => {
  visible = "visible";
  vi.spyOn(document, "visibilityState", "get").mockImplementation(() => visible);
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); });

it("checks saved UUIDs without requiring an address or discovery group and keeps unchanged memberships stable", async () => {
  const presence = vi.fn<HubAccessService["presence"]>().mockImplementation(async (ids) => reply(ids, false));
  const current = service(presence);
  const { result, rerender } = renderHook(({ ids }) => useHubPresence(current, ids), { initialProps: { ids: [second, first, first] } });
  expect(result.current.get(first)?.state).toBe("checking");
  await waitFor(() => expect(result.current.get(first)?.state).toBe("offline"));
  expect(presence).toHaveBeenCalledWith([first, second], expect.any(AbortSignal));
  rerender({ ids: [first, second] });
  expect(presence).toHaveBeenCalledOnce();
  expect(result.current.size).toBe(2);
});

it("marks failed direct checks unavailable instead of keeping a stale online result or declaring offline", async () => {
  const presence = vi.fn<HubAccessService["presence"]>().mockImplementationOnce(async (ids) => reply(ids)).mockRejectedValueOnce(new Error("Hub unavailable"));
  const current = service(presence);
  const { result } = renderHook(() => useHubPresence(current, [first]));
  await waitFor(() => expect(result.current.get(first)?.state).toBe("online"));
  fireEvent.focus(window);
  await waitFor(() => expect(result.current.get(first)).toEqual({ state: "unavailable", error: "Hub unavailable" }));
});

it("batches 129 identities as 128 plus one and isolates a failed batch", async () => {
  const ids = Array.from({ length: 129 }, (_, index) => uuid(index));
  const presence = vi.fn<HubAccessService["presence"]>().mockRejectedValueOnce(new Error("First batch failed")).mockImplementationOnce(async (batch) => reply(batch, false));
  const current = service(presence);
  const { result } = renderHook(() => useHubPresence(current, ids));
  await waitFor(() => expect(result.current.get(ids[128])?.state).toBe("offline"));
  expect(presence.mock.calls.map(([batch]) => batch.length)).toEqual([128, 1]);
  expect(presence.mock.calls.flatMap(([batch]) => batch)).toEqual(ids);
  expect(result.current.get(ids[0])).toEqual({ state: "unavailable", error: "First batch failed" });
  expect(result.current.size).toBe(129);
});

it("polls visible galleries, refreshes on focus, and skips hidden galleries", async () => {
  vi.useFakeTimers();
  const presence = vi.fn<HubAccessService["presence"]>().mockImplementation(async (ids) => reply(ids));
  const current = service(presence);
  renderHook(() => useHubPresence(current, [first]));
  await act(async () => {});
  expect(presence).toHaveBeenCalledOnce();
  await act(async () => { await vi.advanceTimersByTimeAsync(PRESENCE_REFRESH_MS); });
  expect(presence).toHaveBeenCalledTimes(2);
  await act(async () => { fireEvent.focus(window); });
  expect(presence).toHaveBeenCalledTimes(3);
  visible = "hidden";
  fireEvent(document, new Event("visibilitychange"));
  await act(async () => { await vi.advanceTimersByTimeAsync(PRESENCE_REFRESH_MS * 2); fireEvent.focus(window); });
  expect(presence).toHaveBeenCalledTimes(3);
  visible = "visible";
  await act(async () => { fireEvent(document, new Event("visibilitychange")); });
  expect(presence).toHaveBeenCalledTimes(4);
});

it("does not overlap polls and aborts and restarts an in-flight check after hiding and showing", async () => {
  vi.useFakeTimers();
  const old = deferred<PresenceResult>();
  const fresh = deferred<PresenceResult>();
  const presence = vi.fn<HubAccessService["presence"]>().mockImplementationOnce(() => old.promise).mockImplementationOnce(() => fresh.promise);
  const current = service(presence);
  const { result } = renderHook(() => useHubPresence(current, [first]));
  await act(async () => {});
  const old_signal = presence.mock.calls[0][1];
  await act(async () => { await vi.advanceTimersByTimeAsync(PRESENCE_REFRESH_MS * 2); fireEvent.focus(window); });
  expect(presence).toHaveBeenCalledOnce();
  visible = "hidden";
  fireEvent(document, new Event("visibilitychange"));
  expect(old_signal.aborted).toBe(true);
  visible = "visible";
  fireEvent(document, new Event("visibilitychange"));
  expect(presence).toHaveBeenCalledOnce();
  await act(async () => { old.resolve(reply([first])); });
  expect(presence).toHaveBeenCalledTimes(2);
  expect(result.current.get(first)?.state).toBe("checking");
  await act(async () => { fresh.resolve(reply([first], false)); });
  expect(result.current.get(first)?.state).toBe("offline");
});

it("drops cached or late statuses when the Hub service changes", async () => {
  const old_reply = deferred<PresenceResult>();
  const fresh_reply = deferred<PresenceResult>();
  const old_presence = vi.fn<HubAccessService["presence"]>().mockImplementationOnce(async (ids) => reply(ids)).mockImplementationOnce(() => old_reply.promise);
  const old_service = service(old_presence);
  const next_service = service(vi.fn<HubAccessService["presence"]>().mockImplementation(() => fresh_reply.promise));
  const { result, rerender } = renderHook(({ current }) => useHubPresence(current, [first]), { initialProps: { current: old_service } });
  await waitFor(() => expect(result.current.get(first)?.state).toBe("online"));
  fireEvent.focus(window);
  expect(old_presence).toHaveBeenCalledTimes(2);
  const signal = old_presence.mock.calls[1][1];
  rerender({ current: next_service });
  expect(signal.aborted).toBe(true);
  expect(result.current.get(first)?.state).toBe("checking");
  await act(async () => { old_reply.resolve(reply([first])); });
  expect(result.current.get(first)?.state).toBe("checking");
  await act(async () => { fresh_reply.resolve(reply([first], false)); });
  expect(result.current.get(first)?.state).toBe("offline");
});

it("removes retired IDs, ignores their late result, and aborts on cleanup", async () => {
  const old_reply = deferred<PresenceResult>();
  const next_reply = deferred<PresenceResult>();
  const presence = vi.fn<HubAccessService["presence"]>().mockImplementationOnce(() => old_reply.promise).mockImplementationOnce(() => next_reply.promise);
  const current = service(presence);
  const { result, rerender, unmount } = renderHook(({ ids }) => useHubPresence(current, ids), { initialProps: { ids: [first] } });
  await waitFor(() => expect(presence).toHaveBeenCalledOnce());
  const retired_signal = presence.mock.calls[0][1];
  rerender({ ids: [second] });
  expect(retired_signal.aborted).toBe(true);
  expect(result.current.has(first)).toBe(false);
  await act(async () => { old_reply.resolve(reply([first])); });
  expect(result.current.has(first)).toBe(false);
  expect(result.current.get(second)?.state).toBe("checking");
  const active_signal = presence.mock.calls[1][1];
  unmount();
  expect(active_signal.aborted).toBe(true);
  await act(async () => { next_reply.resolve(reply([second])); });
});

it("does not request an empty gallery or start requests while initially hidden", async () => {
  const presence = vi.fn<HubAccessService["presence"]>().mockImplementation(async (ids) => reply(ids));
  const current = service(presence);
  visible = "hidden";
  const { result, rerender } = renderHook(({ ids }) => useHubPresence(current, ids), { initialProps: { ids: [] as string[] } });
  rerender({ ids: [first] });
  expect(result.current.get(first)?.state).toBe("checking");
  expect(presence).not.toHaveBeenCalled();
  visible = "visible";
  fireEvent(document, new Event("visibilitychange"));
  await waitFor(() => expect(result.current.get(first)?.state).toBe("online"));
  expect(presence).toHaveBeenCalledOnce();
});

it("starts one initial presence request under StrictMode effect replay", async () => {
  const presence = vi.fn<HubAccessService["presence"]>().mockImplementation(async (ids) => reply(ids));
  const current = service(presence);
  const { result } = renderHook(() => useHubPresence(current, [first]), { wrapper: StrictMode });
  await waitFor(() => expect(result.current.get(first)?.state).toBe("online"));
  expect(presence).toHaveBeenCalledOnce();
});
