import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getSessionModelStatus, listSessionModels, selectSessionModel } from "./tauri";
import { useSessionModel } from "./useSessionModel";
import type { ModelIdentity, SessionModelState, SessionModelStatus } from "./types";
import type { SessionMutationLock } from "./sessionModels";

const transport = vi.hoisted(() => ({ scope: "local", invoke: vi.fn(), closed: new Set<() => void>() }));
vi.mock("./transport", () => ({
  invoke: vi.fn(), listen: vi.fn(), isDesktop: () => true,
  viewerStorageScope: () => transport.scope,
  captureTransport: () => ({ invoke: transport.invoke, on_close: (handler: () => void) => {
    transport.closed.add(handler); return () => transport.closed.delete(handler);
  } }),
}));
vi.mock("./tauri", () => ({ getSessionModelStatus: vi.fn(), listSessionModels: vi.fn(), selectSessionModel: vi.fn() }));

const first: ModelIdentity = { model_provider: "anthropic", model_id: "first", display_name: "First model" };
const second: ModelIdentity = { model_provider: "anthropic", model_id: "second", display_name: "Second model" };
const state = (model = first, revision = "r1"): SessionModelState => ({ model, source: "live", revision });
const status = (overrides: Partial<SessionModelStatus> = {}): SessionModelStatus => ({ state: state(),
  can_select: true, can_list: true, reason: null, runtime_generation: "runtime-1", updates_pi_default: true, ...overrides });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
const lock = (): SessionMutationLock => ({ send_busy: false, model_busy: false });
beforeEach(() => {
  vi.resetAllMocks(); transport.scope = "local"; transport.closed.clear();
  vi.mocked(getSessionModelStatus).mockResolvedValue(status());
  vi.mocked(listSessionModels).mockResolvedValue({ status: status(), models: [first, second].map((model) => ({ model, supports_images: true })) });
  vi.mocked(selectSessionModel).mockImplementation(async (request) => ({ request_id: request.request_id, status: "applied", message: "Changed", state: state(second, "r2") }));
});
afterEach(cleanup);

it("reads selected-session status, lazily loads verified choices, and acknowledges an exact revision/runtime selection", async () => {
  const guard = lock();
  const { result } = renderHook(() => useSessionModel("one", undefined, guard));
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(listSessionModels).not.toHaveBeenCalled();
  await act(() => result.current.loadChoices());
  await act(() => result.current.select(second));
  expect(selectSessionModel).toHaveBeenCalledExactlyOnceWith({ session_key: "one", request_id: expect.any(String),
    model_provider: "anthropic", model_id: "second", expected_revision: "r1", runtime_generation: "runtime-1" }, transport.invoke);
  expect(result.current.state).toEqual(state(second, "r2"));
  expect(result.current.input_revision).toBe(1);
  expect(guard.model_busy).toBe(false);
});

it("rejects unlisted choices and admits model changes only while idle with no concurrent Send", async () => {
  const guard = lock();
  const { result, rerender } = renderHook(({ running }) => useSessionModel("one", undefined, guard, running), { initialProps: { running: false } });
  await waitFor(() => expect(result.current.checking).toBe(false));
  await act(() => result.current.select(second));
  expect(selectSessionModel).not.toHaveBeenCalled();
  await act(() => result.current.loadChoices());
  guard.send_busy = true;
  await act(() => result.current.select(second));
  guard.send_busy = false;
  rerender({ running: true });
  await waitFor(() => expect(result.current.checking).toBe(false));
  await act(() => result.current.loadChoices());
  await act(() => result.current.select(second));
  expect(selectSessionModel).not.toHaveBeenCalled();
});

it("holds the synchronous model lock through delivery and prevents duplicate same-frame changes", async () => {
  const guard = lock();
  const pending = deferred<Awaited<ReturnType<typeof selectSessionModel>>>();
  vi.mocked(selectSessionModel).mockReturnValue(pending.promise);
  const { result } = renderHook(() => useSessionModel("one", undefined, guard));
  await waitFor(() => expect(result.current.checking).toBe(false));
  await act(() => result.current.loadChoices());
  let sending!: Promise<void>;
  act(() => { sending = result.current.select(second); void result.current.select(second); });
  expect(guard.model_busy).toBe(true);
  expect(selectSessionModel).toHaveBeenCalledTimes(1);
  const request = vi.mocked(selectSessionModel).mock.calls[0][0];
  await act(async () => { pending.resolve({ request_id: request.request_id, status: "applied", state: state(second), message: "" }); await sending; });
  expect(guard.model_busy).toBe(false);
});

it("preserves the current model on definite failure and blocks uncertain retries until a status read", async () => {
  const guard = lock();
  vi.mocked(selectSessionModel).mockImplementation(async (request) => ({ request_id: request.request_id, status: "not_applied", state: null, message: "Runtime changed" }));
  const { result } = renderHook(() => useSessionModel("one", undefined, guard));
  await waitFor(() => expect(result.current.checking).toBe(false));
  await act(() => result.current.loadChoices());
  await act(() => result.current.select(second));
  expect(result.current.state?.model).toEqual(first);
  expect(result.current.error).toBe("Runtime changed");
  expect(result.current.input_revision).toBe(0);
  vi.mocked(selectSessionModel).mockRejectedValue(new Error("Connection lost"));
  await act(() => result.current.loadChoices());
  await act(() => result.current.select(second));
  expect(result.current.uncertain).toBe(true);
  expect(guard.model_uncertain).toBe(true);
  await act(() => result.current.select(second));
  expect(selectSessionModel).toHaveBeenCalledTimes(2);
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ state: state(second, "r2") }));
  await act(() => result.current.refresh());
  expect(result.current.uncertain).toBe(false);
  expect(guard.model_uncertain).toBe(false);
  expect(result.current.state?.model).toEqual(second);
  expect(result.current.input_revision).toBe(1);
});

it("refreshes input capabilities once for an external live model change, without repeated status/running bumps", async () => {
  const guard = lock();
  const { result, rerender } = renderHook(({ fallback, running }) => useSessionModel("one", fallback, guard, running), {
    initialProps: { fallback: { ...state(), source: "recorded" as const }, running: false },
  });
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.input_revision).toBe(0);
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ state: state(second, "r2") }));
  rerender({ fallback: { ...state(second, "recorded-r2"), source: "recorded" }, running: false });
  await waitFor(() => expect(result.current.state?.model).toEqual(second));
  expect(result.current.input_revision).toBe(1);
  await act(() => result.current.refresh());
  rerender({ fallback: { ...state(second, "recorded-r2"), source: "recorded" }, running: true });
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.input_revision).toBe(1);
});

it("refreshes input capabilities if a lazy catalog discovers a changed live model", async () => {
  const guard = lock();
  const { result } = renderHook(() => useSessionModel("one", undefined, guard));
  await waitFor(() => expect(result.current.checking).toBe(false));
  vi.mocked(listSessionModels).mockResolvedValue({ status: status({ state: state(second, "r2") }),
    models: [first, second].map((model) => ({ model, supports_images: true })) });
  await act(() => result.current.loadChoices());
  expect(result.current.input_revision).toBe(1);
  await act(() => result.current.loadChoices());
  expect(result.current.input_revision).toBe(1);
});

it("does not replace a live selected model with an older recorded page or a failed refresh", async () => {
  const guard = lock();
  const { result, rerender } = renderHook(({ fallback }) => useSessionModel("one", fallback, guard), {
    initialProps: { fallback: state() },
  });
  await waitFor(() => expect(result.current.checking).toBe(false));
  await act(() => result.current.loadChoices());
  await act(() => result.current.select(second));
  vi.mocked(getSessionModelStatus).mockRejectedValue(new Error("Older host"));
  rerender({ fallback: { ...state(first, "older"), source: "recorded" } });
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.state?.model).toEqual(second);
  expect(result.current.can_open).toBe(false);
});

it("uses recorded fallback on old hosts without loading a catalog", async () => {
  vi.mocked(getSessionModelStatus).mockRejectedValue(new Error("Unknown viewer API route"));
  const recorded: SessionModelState = { ...state(), source: "recorded" };
  const { result } = renderHook(() => useSessionModel("one", recorded, lock()));
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.state).toEqual(recorded);
  expect(result.current.can_open).toBe(false);
  expect(listSessionModels).not.toHaveBeenCalled();
});

it.each(["recorded", "catalog"] as const)("preserves %s model context when live status cannot resolve a model", async (source) => {
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ state: { model: null, source: "unknown", revision: "unknown" },
    can_select: false, can_list: false, reason: "This provider is read-only." }));
  const fallback: SessionModelState = { ...state(), source };
  const { result } = renderHook(() => useSessionModel("one", fallback, lock()));
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.state).toEqual(fallback);
  expect(result.current.can_open).toBe(false);
  expect(result.current.reason).toBe("This provider is read-only.");
});

it("ignores an old session catalog and an old-machine selection response", async () => {
  const catalog = deferred<Awaited<ReturnType<typeof listSessionModels>>>();
  vi.mocked(listSessionModels).mockReturnValueOnce(catalog.promise);
  let guard = lock();
  const { result, rerender } = renderHook(({ session }) => useSessionModel(session, undefined, guard), { initialProps: { session: "one" } });
  await waitFor(() => expect(result.current.checking).toBe(false));
  let loading!: Promise<void>;
  act(() => { loading = result.current.loadChoices(); });
  rerender({ session: "two" });
  await waitFor(() => expect(result.current.checking).toBe(false));
  await act(async () => { catalog.resolve({ status: status({ state: state(second) }), models: [] }); await loading; });
  expect(result.current.state?.model).toEqual(first);
  expect(result.current.choices).toBeNull();
  await act(() => result.current.loadChoices());
  const pending = deferred<Awaited<ReturnType<typeof selectSessionModel>>>();
  vi.mocked(selectSessionModel).mockReturnValueOnce(pending.promise);
  let selecting!: Promise<void>;
  act(() => { selecting = result.current.select(second); });
  transport.scope = "remote";
  guard = lock();
  rerender({ session: "two" });
  await waitFor(() => expect(result.current.checking).toBe(false));
  await act(async () => { pending.resolve({ request_id: vi.mocked(selectSessionModel).mock.calls[0][0].request_id, status: "applied", message: "", state: state(second) }); await selecting; });
  expect(result.current.state?.model).toEqual(first);
  expect(result.current.input_revision).toBe(0);
});

it("retires a pending status response when the captured machine disconnects", async () => {
  const pending = deferred<SessionModelStatus>();
  vi.mocked(getSessionModelStatus).mockReturnValueOnce(pending.promise);
  const { result } = renderHook(() => useSessionModel("one", undefined, lock()));
  act(() => { for (const close of transport.closed) close(); });
  await act(async () => pending.resolve(status()));
  expect(result.current.status).toBeNull();
  expect(result.current.error).toMatch(/disconnected/);
});
