import { act, cleanup, fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getSessionControl, setSessionControl, cancelSessionTurn } from "./tauri";
import { useSessionControl } from "./useSessionControl";
import { SessionControlSelector } from "../components/SessionControlSelector";
import type { SessionControlStatus, SessionSummary } from "./types";

const machine = vi.hoisted(() => ({ scope: "machine-one", invoke: vi.fn(), closed: new Set<() => void>() }));
vi.mock("./transport", () => ({ viewerStorageScope: () => machine.scope,
  captureTransport: () => ({ invoke: machine.invoke, on_close: (callback: () => void) => {
    machine.closed.add(callback); return () => machine.closed.delete(callback);
  } }),
}));
vi.mock("./tauri", () => ({ getSessionControl: vi.fn(), setSessionControl: vi.fn(), cancelSessionTurn: vi.fn() }));
const session: SessionSummary = {
  session_key: "one", session_id: "one", parent_session_id: null, is_subagent: false, provider: "codex",
  title: "Session", preview: null, project: null, cwd: null, updated_at_ms: null, timestamp: null,
  agent_path: null, agent_nickname: null, agent_role: null, child_count: 0, message_count: null,
  event_count: null, history_status: null, has_unread: false,
};
const status = (overrides: Partial<SessionControlStatus> = {}): SessionControlStatus => ({
  session_key: "one", provider: "codex", transport: "rpc", can_select: true, can_cancel: true, available: true,
  message: "", runtime_id: "runtime", instance_id: "instance", ...overrides,
});
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function Controls({ selected = session }: { selected?: SessionSummary }) {
  const control = useSessionControl(selected);
  return <SessionControlSelector control={control} running={selected.is_running} />;
}
beforeEach(() => {
  vi.resetAllMocks(); machine.scope = "machine-one"; machine.closed.clear();
  vi.mocked(getSessionControl).mockResolvedValue(status());
  vi.mocked(setSessionControl).mockImplementation(async (request) => status({ transport: request.transport }));
});
afterEach(cleanup);

it("shows the host-selected RPC route and keeps missing configuration explicit", async () => {
  vi.mocked(getSessionControl).mockResolvedValue(status({ available: false, message: "Configure an RPC profile." }));
  render(<Controls />);
  await waitFor(() => expect(screen.getByRole("combobox", { name: "Codex connection" })).toBeEnabled());
  expect(screen.getByRole("combobox")).toHaveValue("rpc");
  expect(screen.getByText("Configure an RPC profile.")).toBeVisible();
  expect(setSessionControl).not.toHaveBeenCalled();
  expect(getSessionControl).toHaveBeenCalledWith({ session_key: "one" }, machine.invoke);
});

it("switches only through the captured host command and blocks stale control state while saving", async () => {
  const saving = deferred<SessionControlStatus>();
  vi.mocked(setSessionControl).mockReturnValueOnce(saving.promise);
  render(<Controls />);
  const picker = await screen.findByRole("combobox", { name: "Codex connection" });
  await waitFor(() => expect(picker).toBeEnabled());
  fireEvent.change(picker, { target: { value: "desktop" } });
  expect(picker).toBeDisabled();
  expect(screen.getByText("Changing control…")).toBeVisible();
  expect(setSessionControl).toHaveBeenCalledExactlyOnceWith({ session_key: "one", transport: "desktop", request_id: expect.any(String) }, machine.invoke);
  await act(async () => saving.resolve(status({ transport: "desktop", runtime_id: null, instance_id: null })));
  expect(picker).toBeEnabled();
  expect(picker).toHaveValue("desktop");
});

it("leaves a failed switch unconfirmed without selecting another route or retrying the mutation", async () => {
  vi.mocked(setSessionControl).mockRejectedValueOnce(new Error("RPC ownership changed"));
  render(<Controls />);
  const picker = await screen.findByRole("combobox");
  await waitFor(() => expect(picker).toBeEnabled());
  fireEvent.change(picker, { target: { value: "desktop" } });
  await screen.findByText(/Could not confirm session control/);
  expect(picker).toBeDisabled();
  expect(setSessionControl).toHaveBeenCalledTimes(1);
  fireEvent.click(screen.getByRole("button", { name: "Retry control" }));
  await waitFor(() => expect(picker).toBeEnabled());
  expect(picker).toHaveValue("rpc");
  expect(setSessionControl).toHaveBeenCalledTimes(1);
});

it("hides transport selection for read-only clients and non-Codex sessions", async () => {
  vi.mocked(getSessionControl).mockResolvedValue(status({ can_select: false, can_cancel: false, available: false, message: "Viewing only" }));
  const view = render(<Controls />);
  await waitFor(() => expect(screen.queryByLabelText("Session control")).not.toBeInTheDocument());
  view.rerender(<Controls selected={{ ...session, provider: "pi" }} />);
  expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
  expect(getSessionControl).toHaveBeenCalledTimes(1);
});

it("ignores delayed status and switch replies after selecting another session", async () => {
  const older = deferred<SessionControlStatus>();
  vi.mocked(getSessionControl).mockReturnValueOnce(older.promise)
    .mockResolvedValue(status({ session_key: "two", transport: "desktop" }));
  const next = { ...session, session_key: "two" };
  const { result, rerender } = renderHook(({ selected }) => useSessionControl(selected), { initialProps: { selected: session } });
  rerender({ selected: next });
  await waitFor(() => expect(result.current.status?.transport).toBe("desktop"));
  await act(async () => older.resolve(status()));
  expect(result.current.status?.session_key).toBe("two");
  const saving = deferred<SessionControlStatus>();
  vi.mocked(setSessionControl).mockReturnValueOnce(saving.promise);
  act(() => { void result.current.choose("rpc"); });
  vi.mocked(getSessionControl).mockResolvedValueOnce(status());
  rerender({ selected: session });
  await waitFor(() => expect(result.current.status?.session_key).toBe("one"));
  await act(async () => saving.resolve(status({ session_key: "two" })));
  expect(result.current.status?.session_key).toBe("one");
});

it("clears live routing proof on disconnect and exposes Stop only for active RPC sessions", async () => {
  vi.mocked(cancelSessionTurn).mockImplementation(async (request) => ({ request_id: request.request_id, status: "accepted", message: "Stop requested" }));
  render(<Controls selected={{ ...session, is_running: true }} />);
  const stop = await screen.findByRole("button", { name: "Stop" });
  fireEvent.click(stop);
  await screen.findByText("Stop requested");
  expect(cancelSessionTurn).toHaveBeenCalledWith({ session_key: "one", request_id: expect.any(String) }, machine.invoke);
  act(() => { for (const callback of machine.closed) callback(); });
  expect(screen.queryByRole("button", { name: "Stop" })).not.toBeInTheDocument();
  expect(screen.getByRole("combobox")).toBeDisabled();
  expect(screen.getByText(/Machine disconnected/)).toBeVisible();
});

it("offers native DSH cancellation without a Codex transport selector", async () => {
  vi.mocked(getSessionControl).mockResolvedValue(status({ provider: "dsh", can_select: false, can_cancel: true }));
  render(<Controls selected={{ ...session, provider: "dsh", is_running: true }} />);
  expect(await screen.findByRole("button", { name: "Stop" })).toBeEnabled();
  expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
});

it("keeps cancellation available while input is awaiting acknowledgement", async () => {
  const { result } = renderHook(() => useSessionControl(session));
  await waitFor(() => expect(result.current.status).not.toBeNull());
  render(<SessionControlSelector control={result.current} input_busy running />);
  expect(screen.getByRole("combobox")).toBeDisabled();
  expect(screen.getByRole("button", { name: "Stop" })).toBeEnabled();
});

it("refreshes cancellation after activation and explicit input revisions without polling", async () => {
  vi.mocked(getSessionControl).mockResolvedValueOnce(status({ can_cancel: false })).mockResolvedValue(status());
  const { result, rerender } = renderHook(({ running, revision }) => useSessionControl({ ...session, is_running: running }, revision), {
    initialProps: { running: false, revision: 0 },
  });
  await waitFor(() => expect(result.current.status?.can_cancel).toBe(false));
  expect(getSessionControl).toHaveBeenCalledTimes(1);
  rerender({ running: true, revision: 0 });
  await waitFor(() => expect(result.current.status?.can_cancel).toBe(true));
  expect(getSessionControl).toHaveBeenCalledTimes(2);
  rerender({ running: true, revision: 1 });
  await waitFor(() => expect(getSessionControl).toHaveBeenCalledTimes(3));
  await waitFor(() => expect(result.current.checking).toBe(false));
});

it("keeps draft routing stable for ordinary checks and cancellation but retires a replaced runtime", async () => {
  vi.mocked(cancelSessionTurn).mockImplementation(async (request) => ({ request_id: request.request_id, status: "accepted", message: "Stop requested" }));
  const { result, rerender } = renderHook(({ running }) => useSessionControl({ ...session, is_running: running }), {
    initialProps: { running: false },
  });
  await waitFor(() => expect(result.current.checking).toBe(false));
  const before = result.current.revision;
  rerender({ running: true });
  await waitFor(() => expect(result.current.checking).toBe(false));
  expect(result.current.revision).toBe(before);
  await act(() => result.current.cancel());
  expect(result.current.revision).toBe(before);
  vi.mocked(getSessionControl).mockResolvedValue(status({ instance_id: "replacement" }));
  await act(() => result.current.refresh());
  expect(result.current.revision).not.toBe(before);
});
