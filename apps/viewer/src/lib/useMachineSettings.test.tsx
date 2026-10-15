import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { CommandInvoker, EventSubscriber } from "./transport";
import type { MachineSettingsSnapshot, RelayStatus } from "./types";
import { useMachineSettings } from "./useMachineSettings";

afterEach(cleanup);

function status(endpoint = "tcp://127.0.0.1:5557"): RelayStatus {
  return { settings: { mode: "automatic", endpoint, include_native: false }, active_endpoint: null,
    phase: "live", native: false, error: null };
}

function snapshot(): MachineSettingsSnapshot {
  return { version: 1, reader: { status: status(), editable: true, edit_reason: null },
    sources: [], sources_editable: false, sources_edit_reason: "Detected from host environment",
    hosting: { available: true, editable: true, reason: null } };
}

function connection(response = Promise.resolve(snapshot())) {
  let event!: (event: { payload: RelayStatus }) => void;
  const stop = vi.fn();
  const invoke = vi.fn(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "get_machine_settings") return response;
    if (command === "configure_relay") return { ...status(), settings: payload?.settings };
    throw new Error(`Unexpected command ${command}`);
  });
  const listen = vi.fn(async (_name: string, handler: typeof event) => { event = handler; return stop; });
  return { transport: { invoke: invoke as CommandInvoker, listen: listen as EventSubscriber }, invoke, stop,
    emit: (payload: RelayStatus) => event({ payload }) };
}

describe("machine settings ownership", () => {
  it("does not contact a machine before a connection is selected", async () => {
    const host = connection();
    const { result } = renderHook(() => useMachineSettings(false, host.transport, false));
    await act(async () => {});
    expect(result.current.loading).toBe(false);
    expect(host.invoke).not.toHaveBeenCalled();
    expect(result.current.settings).toBeNull();
  });
  it("reads once and applies local changes through the captured transport", async () => {
    const host = connection();
    const { result, unmount } = renderHook(() => useMachineSettings(true, host.transport));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(host.invoke).toHaveBeenCalledExactlyOnceWith("get_machine_settings");
    const draft = { mode: "external" as const, endpoint: " tcp://127.0.0.1:7777 ", include_native: false };
    await act(async () => { await result.current.applyReader(draft); });
    expect(host.invoke).toHaveBeenLastCalledWith("configure_relay", { settings: { ...draft, endpoint: draft.endpoint.trim() } });
    expect(result.current.settings?.reader.status.settings.endpoint).toBe("tcp://127.0.0.1:7777");
    unmount();
    expect(host.stop).toHaveBeenCalledOnce();
  });

  it("keeps a remote machine read-only even if its response claims local edit capability", async () => {
    const host = connection();
    const { result } = renderHook(() => useMachineSettings(false, host.transport));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.settings?.reader.editable).toBe(false);
    expect(result.current.settings?.hosting.editable).toBe(false);
    await expect(result.current.applyReader(status().settings)).rejects.toThrow("read-only");
    expect(host.invoke).toHaveBeenCalledOnce();
  });

  it("preserves a newer status event when a delayed snapshot arrives", async () => {
    let resolve!: (value: MachineSettingsSnapshot) => void;
    const host = connection(new Promise((accept) => { resolve = accept; }));
    const { result } = renderHook(() => useMachineSettings(true, host.transport));
    await waitFor(() => expect(host.invoke).toHaveBeenCalledOnce());
    act(() => host.emit(status("tcp://127.0.0.1:8888")));
    await act(async () => { resolve(snapshot()); });
    expect(result.current.settings?.reader.status.settings.endpoint).toBe("tcp://127.0.0.1:8888");
  });

  it("does not let an old machine snapshot overwrite a successor", async () => {
    let resolve!: (value: MachineSettingsSnapshot) => void;
    const old = connection(new Promise((accept) => { resolve = accept; }));
    const next = connection(Promise.resolve({ ...snapshot(), reader: { ...snapshot().reader, status: status("next") } }));
    const { result, rerender } = renderHook(({ transport }) => useMachineSettings(true, transport), { initialProps: { transport: old.transport } });
    rerender({ transport: next.transport });
    await waitFor(() => expect(result.current.settings?.reader.status.settings.endpoint).toBe("next"));
    await act(async () => { resolve(snapshot()); });
    expect(result.current.settings?.reader.status.settings.endpoint).toBe("next");
    expect(old.stop).toHaveBeenCalledOnce();
  });

  it("registers status events before reading the machine baseline", async () => {
    let ready!: () => void;
    const host = connection();
    const registering = new Promise<void>((resolve) => { ready = resolve; });
    const listen = vi.fn(async () => { await registering; return () => {}; });
    const transport = { ...host.transport, listen: listen as EventSubscriber };
    renderHook(() => useMachineSettings(true, transport));
    expect(host.invoke).not.toHaveBeenCalled();
    await act(async () => { ready(); });
    expect(host.invoke).toHaveBeenCalledOnce();
  });

  it("rejects a late Apply result after switching machine lifetimes", async () => {
    let applied!: (status: RelayStatus) => void;
    const old = connection();
    old.invoke.mockImplementation(async (command) => command === "get_machine_settings"
      ? snapshot() : new Promise<RelayStatus>((resolve) => { applied = resolve; }));
    const next = connection(Promise.resolve({ ...snapshot(), reader: { ...snapshot().reader, status: status("next") } }));
    const { result, rerender } = renderHook(({ transport }) => useMachineSettings(true, transport), { initialProps: { transport: old.transport } });
    await waitFor(() => expect(result.current.loading).toBe(false));
    const pending = result.current.applyReader(status("old").settings);
    const rejected = expect(pending).rejects.toThrow("changed while applying");
    rerender({ transport: next.transport });
    await waitFor(() => expect(result.current.settings?.reader.status.settings.endpoint).toBe("next"));
    await act(async () => { applied(status("old")); await rejected; });
    expect(result.current.settings?.reader.status.settings.endpoint).toBe("next");
  });

  it("reports unsupported hosts without requesting history or attempting writes", async () => {
    const host = connection();
    host.invoke.mockRejectedValue(new Error("Unknown viewer API route: get_machine_settings"));
    const { result } = renderHook(() => useMachineSettings(false, host.transport));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.error).toContain("does not expose machine settings");
    expect(host.invoke).toHaveBeenCalledOnce();
  });
});
