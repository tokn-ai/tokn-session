import { expect, it, vi } from "vitest";
import { NativePreferenceStore } from "./nativePreferenceStore";
import type { NativePreferenceUpdate } from "./types";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((accept, decline) => { resolve = accept; reject = decline; });
  return { promise, resolve, reject };
}
const settle = async () => { await Promise.resolve(); await Promise.resolve(); };
const update = (preferences: number, revision: number, write_id = "another-window"): NativePreferenceUpdate<number> => ({ preferences, revision, write_id });
type Writer = (value: number, write_id: string) => Promise<NativePreferenceUpdate<number>>;

it("updates display immediately while serializing and coalescing queued writes", async () => {
  const first = deferred<NativePreferenceUpdate<number>>();
  const second = deferred<NativePreferenceUpdate<number>>();
  const write = vi.fn<Writer>().mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
  const changed = vi.fn();
  const store = new NativePreferenceStore<number>((left, right) => left === right, changed);
  store.initialize(0, write, 0);
  store.update(1); store.update(2); store.update(3);
  expect(store.read()).toBe(3);
  expect(write.mock.calls.map(([value]) => value)).toEqual([1]);
  expect(store.getStatus()).toEqual({ saving: true, error: null });
  first.resolve(update(1, 1, write.mock.calls[0][1])); await settle();
  expect(store.read()).toBe(3);
  expect(write.mock.calls.map(([value]) => value)).toEqual([1, 3]);
  second.resolve(update(3, 2, write.mock.calls[1][1])); await settle();
  expect(store.read()).toBe(3);
  expect(store.getStatus()).toEqual({ saving: false, error: null });
  store.update(3);
  expect(write).toHaveBeenCalledTimes(2);
  expect(changed).toHaveBeenCalledTimes(4);
  expect(write.mock.calls[0][1]).not.toEqual(write.mock.calls[1][1]);
});

it("retains unsaved edits after failure and exposes a retry without touching display", async () => {
  const write = vi.fn<Writer>().mockRejectedValueOnce(new Error("Disk full"))
    .mockImplementationOnce(async (value, write_id) => update(value, 2, write_id));
  const store = new NativePreferenceStore<number>((left, right) => left === right, vi.fn());
  const status_changed = vi.fn(); store.subscribe(status_changed);
  store.initialize(0, write, 0);
  store.update(1); await settle();
  expect(store.read()).toBe(1);
  expect(store.getStatus()).toEqual({ saving: false, error: "Disk full" });
  store.receive(update(0, 1));
  expect(store.read()).toBe(1);
  store.retry();
  expect(store.getStatus()).toEqual({ saving: true, error: null });
  await settle();
  expect(store.read()).toBe(1);
  expect(store.getStatus()).toEqual({ saving: false, error: null });
  expect(write.mock.calls.map(([value]) => value)).toEqual([1, 1]);
  expect(status_changed).toHaveBeenCalledTimes(4);
});

it("continues with the latest queued edit after an older write fails", async () => {
  const first = deferred<NativePreferenceUpdate<number>>();
  const write = vi.fn<Writer>().mockReturnValueOnce(first.promise)
    .mockImplementationOnce(async (value, write_id) => update(value, 1, write_id));
  const store = new NativePreferenceStore<number>((left, right) => left === right, vi.fn());
  store.initialize(0, write, 0);
  store.update(1); store.update(2);
  first.reject(new Error("Temporary failure")); await settle();
  expect(write.mock.calls.map(([value]) => value)).toEqual([1, 2]);
  expect(store.read()).toBe(2);
  expect(store.getStatus()).toEqual({ saving: false, error: null });
});

it("keeps newer drafts over delayed local results and events, then ignores old native revisions", async () => {
  const first = deferred<NativePreferenceUpdate<number>>();
  const second = deferred<NativePreferenceUpdate<number>>();
  const write = vi.fn<Writer>().mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
  const store = new NativePreferenceStore<number>((left, right) => left === right, vi.fn());
  store.initialize(0, write, 0);
  store.receive(update(4, 1));
  expect(store.read()).toBe(4);
  store.update(1); store.update(2);
  first.resolve(update(1, 2, write.mock.calls[0][1])); await settle();
  store.receive(update(1, 2, write.mock.calls[0][1]));
  expect(store.read()).toBe(2);
  second.resolve(update(2, 3, write.mock.calls[1][1])); await settle();
  store.receive(update(4, 1));
  expect(store.read()).toBe(2);
  store.receive(update(5, 4));
  expect(store.read()).toBe(5);
});

it("a newer external commit outranks an older result even when it repeats the old local value", async () => {
  const pending = deferred<NativePreferenceUpdate<number>>();
  const write = vi.fn<Writer>().mockReturnValue(pending.promise);
  const store = new NativePreferenceStore<number>((left, right) => left === right, vi.fn());
  store.initialize(7, write, 0);
  store.update(1);
  store.receive(update(1, 1, write.mock.calls[0][1]));
  store.receive(update(7, 2));
  expect(store.read()).toBe(1);
  pending.resolve(update(1, 1, write.mock.calls[0][1])); await settle();
  expect(store.read()).toBe(7);
  expect(store.getStatus()).toEqual({ saving: false, error: null });
});

it("does not mistake another window's same-value commit for an echo of a failed local write", async () => {
  const write = vi.fn<Writer>().mockRejectedValueOnce(new Error("Disk full"))
    .mockImplementationOnce(async (value, write_id) => update(value, 1, write_id));
  const store = new NativePreferenceStore<number>((left, right) => left === right, vi.fn());
  store.initialize(10, write, 0);
  store.update(11); await settle();
  store.update(12); await settle();
  expect(store.read()).toBe(12);
  store.receive(update(11, 2));
  expect(store.read()).toBe(11);
});

it("a replaced initialization is unaffected by a previous writer's late result", async () => {
  const stale = deferred<NativePreferenceUpdate<number>>();
  const old_write = vi.fn<Writer>().mockReturnValue(stale.promise);
  const store = new NativePreferenceStore<number>((left, right) => left === right, vi.fn());
  store.initialize(0, old_write, 0); store.update(1);
  store.initialize(8, async (value, write_id) => update(value, 6, write_id), 5);
  stale.resolve(update(1, 1, old_write.mock.calls[0][1])); await settle();
  expect(store.read()).toBe(8);
  expect(store.getStatus()).toEqual({ saving: false, error: null });
  store.update(9); await settle();
  expect(store.read()).toBe(9);
});

it("does not acknowledge a result carrying another request's write identity", async () => {
  const store = new NativePreferenceStore<number>((left, right) => left === right, vi.fn());
  store.initialize(0, async () => update(1, 1, "wrong-identity"), 0);
  store.update(1); await settle();
  expect(store.read()).toBe(1);
  expect(store.getStatus().error).toContain("different write identity");
});

it("flushes edits triggered while publishing the idle status", async () => {
  let revision = 0;
  const write = vi.fn<Writer>(async (value, write_id) => update(value, ++revision, write_id));
  const store = new NativePreferenceStore<number>((left, right) => left === right, vi.fn());
  store.initialize(0, write, 0);
  store.subscribe(() => { if (!store.getStatus().saving && store.read() === 1) store.update(2); });
  store.update(1); await settle(); await settle();
  expect(write.mock.calls.map(([value]) => value)).toEqual([1, 2]);
  expect(store.read()).toBe(2);
  expect(store.getStatus()).toEqual({ saving: false, error: null });
});
