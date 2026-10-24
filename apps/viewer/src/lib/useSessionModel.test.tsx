import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getSessionModelStatus, listSessionModels } from "./tauri";
import { useSessionModel } from "./useSessionModel";
import type { ModelIdentity, SessionModelState, SessionModelStatus } from "./types";

const transport = vi.hoisted(() => ({ scope: "local", invoke: vi.fn(), closed: new Set<() => void>() }));
vi.mock("./transport", () => ({ invoke: vi.fn(), listen: vi.fn(), isDesktop: () => true,
  viewerStorageScope: () => transport.scope, captureTransport: () => ({ invoke: transport.invoke,
    on_close: (handler: () => void) => { transport.closed.add(handler); return () => transport.closed.delete(handler); } }),
}));
vi.mock("./tauri", () => ({ getSessionModelStatus: vi.fn(), listSessionModels: vi.fn() }));
const first: ModelIdentity = { model_provider: "anthropic", model_id: "first", display_name: "First model" };
const second: ModelIdentity = { model_provider: "anthropic", model_id: "second", display_name: "Second model" };
const state = (model = first, revision = "r1"): SessionModelState => ({ model, source: "live", revision });
const status = (overrides: Partial<SessionModelStatus> = {}): SessionModelStatus => ({ state: state(),
  can_select: true, can_list: true, reason: null, runtime_generation: "runtime-1", updates_pi_default: true,
  supports_model_input: true, ...overrides });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
beforeEach(() => {
  vi.resetAllMocks(); transport.scope = "local"; transport.closed.clear();
  vi.mocked(getSessionModelStatus).mockResolvedValue(status());
  vi.mocked(listSessionModels).mockResolvedValue({ status: status(), models: [first, second].map((model) => ({ model, supports_images: true })) });
});
afterEach(cleanup);

it("loads verified choices lazily and permits draft choices while the runtime is busy", async () => {
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ can_select: false, reason: "Wait for Pi to finish" }));
  const { result } = renderHook(() => useSessionModel("one", undefined, true));
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.can_open).toBe(true);
  expect(result.current.reason).toBeNull();
  expect(listSessionModels).not.toHaveBeenCalled();
  await act(() => result.current.loadChoices());
  expect(result.current.choiceAllowed(second)).toBe(true);
  expect(result.current.state?.model).toEqual(first);
});

it("keeps older hosts read-only even when they expose the old immediate model catalog", async () => {
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ supports_model_input: undefined }));
  const { result } = renderHook(() => useSessionModel("one", undefined));
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.can_open).toBe(false);
  expect(result.current.reason).toMatch(/Update this host/);
  await act(() => result.current.loadChoices());
  expect(listSessionModels).not.toHaveBeenCalled();
});

it("keeps an explicit native capability limitation instead of suggesting an upgrade", async () => {
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ supports_model_input: false,
    reason: "This RPC cannot atomically apply a model on Send." }));
  const { result } = renderHook(() => useSessionModel("one", undefined));
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.reason).toBe("This RPC cannot atomically apply a model on Send.");
});

it.each(["recorded", "catalog"] as const)("preserves %s context when model controls are unavailable", async (source) => {
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ state: { model: null, source: "unknown", revision: "unknown" },
    can_select: false, can_list: false, supports_model_input: false, reason: "This session is read-only." }));
  const fallback: SessionModelState = { ...state(), source };
  const { result } = renderHook(() => useSessionModel("one", fallback));
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.state).toEqual(fallback);
  expect(result.current.can_open).toBe(false);
});

it("refreshes input capabilities for a changed live model without repeating on identical running/status updates", async () => {
  const { result, rerender } = renderHook(({ running }) => useSessionModel("one", undefined, running), { initialProps: { running: false } });
  await waitFor(() => expect(result.current.checking).toBe(false));
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ state: state(second, "r2") }));
  await act(() => result.current.refresh());
  expect(result.current.input_revision).toBe(1);
  rerender({ running: true });
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.input_revision).toBe(1);
});

it("accepts confirmed message results without allowing a pending older status read to replace them", async () => {
  const pending = deferred<SessionModelStatus>();
  vi.mocked(getSessionModelStatus).mockReturnValueOnce(pending.promise);
  const { result } = renderHook(() => useSessionModel("one", undefined));
  act(() => { result.current.acceptState(state(second, "r2"), true); });
  await act(async () => pending.resolve(status()));
  expect(result.current.state?.model).toEqual(second);
  expect(result.current.input_revision).toBe(1);
});

it("ignores an old session or machine catalog and rejects stale response callbacks", async () => {
  const pending = deferred<Awaited<ReturnType<typeof listSessionModels>>>();
  vi.mocked(listSessionModels).mockReturnValueOnce(pending.promise);
  const { result, rerender } = renderHook(({ session }) => useSessionModel(session, undefined), { initialProps: { session: "one" } });
  await waitFor(() => expect(result.current.checking).toBe(false));
  const old_accept = result.current.acceptState;
  let loading!: Promise<void>;
  act(() => { loading = result.current.loadChoices(); });
  transport.scope = "remote";
  rerender({ session: "two" });
  await waitFor(() => expect(result.current.checking).toBe(false));
  await act(async () => { pending.resolve({ status: status({ state: state(second) }), models: [] }); await loading; });
  act(() => { old_accept(state(second, "old"), true); });
  expect(result.current.state?.model).toEqual(first);
  expect(result.current.choices).toBeNull();
  expect(result.current.input_revision).toBe(0);
});

it("retires status responses when the captured machine disconnects", async () => {
  const pending = deferred<SessionModelStatus>();
  vi.mocked(getSessionModelStatus).mockReturnValueOnce(pending.promise);
  const { result } = renderHook(() => useSessionModel("one", undefined));
  act(() => { for (const close of transport.closed) close(); });
  await act(async () => pending.resolve(status()));
  expect(result.current.status).toBeNull();
  expect(result.current.error).toMatch(/disconnected/);
});

it("rejects a receipt captured before leaving and returning to the same session", async () => {
  const { result, rerender } = renderHook(({ session }) => useSessionModel(session, undefined), { initialProps: { session: "one" } });
  await waitFor(() => expect(result.current.checking).toBe(false));
  const old_accept = result.current.acceptState;
  rerender({ session: "two" });
  await waitFor(() => expect(result.current.checking).toBe(false));
  rerender({ session: "one" });
  await waitFor(() => expect(result.current.checking).toBe(false));
  act(() => { expect(old_accept(state(second, "old"), true)).toBe(false); });
  expect(result.current.state?.model).toEqual(first);
  expect(result.current.input_revision).toBe(0);
});

it("rejects old-runtime receipts but accepts an applied revision on the current generation", async () => {
  const { result } = renderHook(() => useSessionModel("one", undefined));
  await waitFor(() => expect(result.current.checking).toBe(false));
  const old_accept = result.current.acceptState;
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ runtime_generation: "runtime-2", state: state(first, "fresh-runtime") }));
  await act(() => result.current.refresh());
  const before = result.current.input_revision;
  act(() => { expect(old_accept(state(second, "old"), true)).toBe(false); });
  expect(result.current.state?.model).toEqual(first);
  expect(result.current.input_revision).toBe(before);
  act(() => { expect(result.current.acceptState(state(second, "applied"), true)).toBe(true); });
  expect(result.current.state?.model).toEqual(second);
  expect(result.current.input_revision).toBe(before + 1);
});

it("retires delayed model catalogs and receipts when the host control route changes", async () => {
  const catalog = deferred<Awaited<ReturnType<typeof listSessionModels>>>();
  vi.mocked(listSessionModels).mockReturnValueOnce(catalog.promise);
  const { result, rerender } = renderHook(({ route }) => useSessionModel("one", undefined, false, 0, route), {
    initialProps: { route: "rpc" },
  });
  await waitFor(() => expect(result.current.checking).toBe(false));
  const accept_rpc = result.current.acceptState;
  let loading!: Promise<void>;
  act(() => { loading = result.current.loadChoices(); });
  vi.mocked(getSessionModelStatus).mockResolvedValue(status({ runtime_generation: "desktop-owner", state: state(second, "desktop") }));
  rerender({ route: "desktop" });
  await waitFor(() => expect(result.current.status?.runtime_generation).toBe("desktop-owner"));
  await act(async () => {
    catalog.resolve({ status: status(), models: [{ model: first, supports_images: true }] });
    await loading;
  });
  expect(result.current.choices).toBeNull();
  expect(result.current.state?.model).toEqual(second);
  act(() => { expect(accept_rpc(state(first, "late-rpc"))).toBe(false); });
  expect(result.current.state?.revision).toBe("desktop");
});
