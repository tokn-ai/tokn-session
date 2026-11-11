import { StrictMode } from "react";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import type { HubAccessService } from "./hubAccess";
import { discoveryGroupKey } from "./hubDiscovery";
import type { DiscoveryGroup, DiscoveryResult, SavedHubHost } from "./types";
import { useHubDiscovery } from "./useHubDiscovery";

afterEach(cleanup);
const first_group = { namespace: "personal", secret_digest: "A".repeat(43) };
const second_group = { namespace: "personal", secret_digest: `${"B".repeat(42)}A` };
const machine = { host_id: "550e8400-e29b-41d4-a716-446655440001", name: "Laptop", online: true };
const saved = (discovery_group?: DiscoveryGroup, host_id = "550e8400-e29b-41d4-a716-446655440000"): SavedHubHost => ({ host_id, host_public_key: "H".repeat(43), discovery_group });
const service = (discover: HubAccessService["discover"]) => ({ hub_url: "https://hub.example", discover }) as HubAccessService;
const resultFor = (group: DiscoveryGroup, machines = [machine]): DiscoveryResult => ({ ...group, machines });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

it("never requests unknown groups", async () => {
  const discover = vi.fn<HubAccessService["discover"]>();
  const current = service(discover);
  const { result, rerender } = renderHook(({ hosts }) => useHubDiscovery(current, hosts), { initialProps: { hosts: [] as SavedHubHost[] } });
  rerender({ hosts: [saved()] });
  act(() => result.current.refresh_group(discoveryGroupKey(first_group)));
  await act(async () => {});
  expect(result.current.groups).toEqual([]);
  expect(discover).not.toHaveBeenCalled();
});

it("loads each distinct group once and retains its state across refreshed host metadata", async () => {
  const discover = vi.fn<HubAccessService["discover"]>().mockImplementation(async (group) => resultFor(group));
  const current = service(discover);
  const { result, rerender } = renderHook(({ hosts }) => useHubDiscovery(current, hosts), { initialProps: { hosts: [saved(first_group)] } });
  await waitFor(() => expect(result.current.groups[0].loading).toBe(false));
  const first_loaded = result.current.groups[0];
  rerender({ hosts: [{ ...saved(first_group), name: "Renamed" }, saved(first_group, machine.host_id)] });
  expect(result.current.groups[0]).toBe(first_loaded);
  expect(discover).toHaveBeenCalledOnce();
  rerender({ hosts: [saved(first_group), saved(second_group, machine.host_id)] });
  await waitFor(() => expect(result.current.groups.every((group) => !group.loading)).toBe(true));
  expect(result.current.groups.find((group) => group.group_key === discoveryGroupKey(first_group))).toBe(first_loaded);
  expect(discover).toHaveBeenCalledTimes(2);
});

it("isolates failures and retries only the requested group", async () => {
  let first_calls = 0;
  const discover = vi.fn<HubAccessService["discover"]>().mockImplementation(async (group) => {
    if (group.secret_digest === first_group.secret_digest && first_calls++ === 0) throw new Error("Group unavailable");
    return resultFor(group);
  });
  const current = service(discover);
  const { result } = renderHook(() => useHubDiscovery(current, [saved(first_group), saved(second_group, machine.host_id)]));
  await waitFor(() => expect(result.current.groups.every((group) => !group.loading)).toBe(true));
  expect(result.current.groups[0].error).toBe("Group unavailable");
  const other = result.current.groups[1];
  act(() => result.current.refresh_group(discoveryGroupKey(first_group)));
  await waitFor(() => expect(result.current.groups[0].loading).toBe(false));
  expect(result.current.groups[0].machines).toEqual([machine]);
  expect(result.current.groups[0].error).toBeUndefined();
  expect(result.current.groups[1]).toBe(other);
  expect(discover.mock.calls.filter(([group]) => group.secret_digest === second_group.secret_digest)).toHaveLength(1);
});

it("hides cached machines immediately when switching service and ignores its late response", async () => {
  const old_request = deferred<DiscoveryResult>();
  const next_request = deferred<DiscoveryResult>();
  const old_discover = vi.fn<HubAccessService["discover"]>().mockResolvedValueOnce(resultFor(first_group)).mockImplementationOnce(() => old_request.promise);
  const old_service = service(old_discover);
  const next_service = service(vi.fn<HubAccessService["discover"]>().mockImplementation(() => next_request.promise));
  const { result, rerender } = renderHook(({ current }) => useHubDiscovery(current, [saved(first_group)]), { initialProps: { current: old_service } });
  await waitFor(() => expect(result.current.groups[0].machines).toHaveLength(1));
  act(() => result.current.refresh_group(discoveryGroupKey(first_group)));
  await waitFor(() => expect(old_discover).toHaveBeenCalledTimes(2));
  const stale_signal = old_discover.mock.calls[1][1];
  rerender({ current: next_service });
  expect(result.current.groups[0].machines).toEqual([]);
  expect(stale_signal.aborted).toBe(true);
  await act(async () => { old_request.resolve(resultFor(first_group)); });
  expect(result.current.groups[0].machines).toEqual([]);
  await act(async () => { next_request.resolve(resultFor(first_group, [])); });
  expect(result.current.groups[0]).toMatchObject({ loading: false, machines: [] });
});

it("removes a retired group without refetching retained groups or accepting a late result", async () => {
  const pending = deferred<DiscoveryResult>();
  const discover = vi.fn<HubAccessService["discover"]>().mockImplementation(async (group) => group.secret_digest === first_group.secret_digest ? pending.promise : resultFor(group));
  const current = service(discover);
  const { result, rerender } = renderHook(({ hosts }) => useHubDiscovery(current, hosts), { initialProps: { hosts: [saved(first_group), saved(second_group, machine.host_id)] } });
  await waitFor(() => expect(discover).toHaveBeenCalledTimes(2));
  const stale_signal = discover.mock.calls.find(([group]) => group.secret_digest === first_group.secret_digest)![1];
  rerender({ hosts: [saved(second_group, machine.host_id)] });
  expect(result.current.groups.map((group) => group.group_key)).toEqual([discoveryGroupKey(second_group)]);
  expect(stale_signal.aborted).toBe(true);
  await act(async () => { pending.resolve(resultFor(first_group)); });
  act(() => result.current.refresh_group(discoveryGroupKey(first_group)));
  expect(discover).toHaveBeenCalledTimes(2);
});

it("aborts requests and disables retained refresh callbacks after unmount", async () => {
  const pending = deferred<DiscoveryResult>();
  const discover = vi.fn<HubAccessService["discover"]>().mockImplementation(() => pending.promise);
  const current = service(discover);
  const { result, unmount } = renderHook(() => useHubDiscovery(current, [saved(first_group)]));
  await waitFor(() => expect(discover).toHaveBeenCalledOnce());
  const refresh = result.current.refresh_group;
  unmount();
  expect(discover.mock.calls[0][1].aborted).toBe(true);
  refresh(discoveryGroupKey(first_group));
  await act(async () => { pending.resolve(resultFor(first_group)); });
  expect(discover).toHaveBeenCalledOnce();
});

it("survives StrictMode effect replay without leaving an abandoned group loading", async () => {
  const discover = vi.fn<HubAccessService["discover"]>().mockImplementation(async (group) => resultFor(group));
  const current = service(discover);
  const { result } = renderHook(() => useHubDiscovery(current, [saved(first_group)]), { wrapper: StrictMode });
  await waitFor(() => expect(result.current.groups[0]).toMatchObject({ loading: false, machines: [machine] }));
  expect(discover).toHaveBeenCalledOnce();
});

it("rejects a response for another digest instead of exposing its candidate machines", async () => {
  const discover = vi.fn<HubAccessService["discover"]>().mockResolvedValue(resultFor(second_group));
  const current = service(discover);
  const { result } = renderHook(() => useHubDiscovery(current, [saved(first_group)]));
  await waitFor(() => expect(result.current.groups[0].error).toBe("Hub returned a different discovery group."));
  expect(result.current.groups[0].machines).toEqual([]);
});
