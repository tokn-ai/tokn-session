import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { AgentInstallations } from "./AgentInstallations";
import { captureTransport, type CommandInvoker, type EventSubscriber } from "../lib/transport";
import type { AgentInstallation, RpcProvider, RuntimeInstallInventory, RuntimeInstallJob } from "../lib/types";

vi.mock("../lib/transport", async (original) => ({ ...await original<typeof import("../lib/transport")>(), captureTransport: vi.fn() }));
const props = { active: true, connected: true, machine_label: "Selected host" };
function agent(provider: RpcProvider, extra: Partial<AgentInstallation> = {}): AgentInstallation {
  return { provider, version: null, generation: null, requires_node: provider === "pi" || provider === "dsh", available: true,
    unavailable_reason: null, in_use: false, job: null, ...extra };
}
function job(provider: RpcProvider, request_id: string, extra: Partial<RuntimeInstallJob> = {}): RuntimeInstallJob {
  return { job_id: "job-a", request_id, provider, state: "resolving", downloaded_bytes: 0, total_bytes: null, error: null, ...extra };
}
function bind(send: ReturnType<typeof vi.fn>) {
  const handlers = new Map<string, Set<(event: { payload: unknown }) => void>>();
  const listen: EventSubscriber = async (name, handler) => {
    const values = handlers.get(name) ?? new Set();
    values.add(handler as (event: { payload: unknown }) => void); handlers.set(name, values);
    return () => { values.delete(handler as (event: { payload: unknown }) => void); };
  };
  vi.mocked(captureTransport).mockReturnValue({ invoke: send as CommandInvoker, listen, release: vi.fn(), on_close: () => () => {} });
  return (event = "agent-installations-changed") => act(() => { for (const handler of handlers.get(event) ?? []) handler({ payload: {} }); });
}
beforeEach(() => vi.clearAllMocks());
afterEach(cleanup);

it("downloads only after a click, shows host prerequisites, and follows job progress and cancellation", async () => {
  let snapshot: RuntimeInstallInventory = { agents: [agent("codex"), agent("dsh", { available: false, unavailable_reason: "Install Node 24 or newer and npm on this host." })] };
  const send = vi.fn(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "get_agent_installations") return snapshot;
    if (command === "start_agent_installation") {
      const request = payload!.request as { request_id: string; provider: RpcProvider };
      const current = job(request.provider, request.request_id);
      snapshot = { agents: [agent("codex", { job: current, in_use: true }), snapshot.agents[1]] }; return current;
    }
    if (command === "cancel_agent_installation") {
      const current = { ...snapshot.agents[0].job!, state: "cancelled" as const };
      snapshot = { agents: [agent("codex", { job: current }), snapshot.agents[1]] }; return current;
    }
    throw new Error(`Unexpected command: ${command}`);
  });
  const emit = bind(send); render(<AgentInstallations {...props} />);
  await screen.findByRole("button", { name: "Download Codex" });
  expect(send.mock.calls.every(([command]) => command === "get_agent_installations")).toBe(true);
  expect(screen.getByRole("button", { name: "Download DSH" })).toBeDisabled();
  expect(screen.getByText("Install Node 24 or newer and npm on this host.")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Download Codex" }));
  await screen.findByRole("button", { name: "Cancel Codex download" });
  expect(screen.queryByText(/Stop this agent’s active runtimes/)).not.toBeInTheDocument();
  expect(send).toHaveBeenCalledWith("start_agent_installation", { request: { request_id: expect.any(String), provider: "codex" } });
  snapshot = { agents: [agent("codex", { job: { ...snapshot.agents[0].job!, state: "downloading", downloaded_bytes: 512 * 1024, total_bytes: 1024 * 1024 } }), snapshot.agents[1]] };
  emit();
  await waitFor(() => expect(screen.getByRole("progressbar", { name: "Codex download progress" })).toHaveValue(50));
  expect(screen.getByText("512 KB of 1.0 MB")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Cancel Codex download" }));
  await screen.findByText("Download cancelled.");
  expect(send).toHaveBeenCalledWith("cancel_agent_installation", { request: { job_id: "job-a" } });
  expect(screen.getByRole("button", { name: "Download Codex" })).toBeEnabled();
});

it("blocks changes while in use, refreshes after a runtime stops, and pins removal to the confirmed installation", async () => {
  let snapshot: RuntimeInstallInventory = { agents: [agent("pi", { version: "1.2.0", generation: "generation-a", in_use: true })] };
  const send = vi.fn(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "get_agent_installations") return snapshot;
    if (command === "remove_agent_installation") {
      const request = payload!.request as { generation: string };
      if (request.generation !== snapshot.agents[0].generation) throw new Error("The installation changed. Refresh and try again.");
      snapshot = { agents: [agent("pi")] }; return snapshot;
    }
    throw new Error(`Unexpected command: ${command}`);
  });
  const emit = bind(send); render(<AgentInstallations {...props} />);
  expect(await screen.findByRole("button", { name: "Update Pi" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Remove Pi" })).toBeDisabled();
  snapshot = { agents: [{ ...snapshot.agents[0], in_use: false }] }; emit("managed-session-changed");
  await waitFor(() => expect(screen.getByRole("button", { name: "Remove Pi" })).toBeEnabled());
  fireEvent.click(screen.getByRole("button", { name: "Remove Pi" }));
  snapshot = { agents: [{ ...snapshot.agents[0], version: "1.3.0", generation: "generation-b" }] }; emit();
  await screen.findByText("v1.3.0");
  fireEvent.click(screen.getByRole("button", { name: "Remove downloaded Pi" }));
  await screen.findByText("The installation changed. Refresh and try again.");
  expect(send).toHaveBeenCalledWith("remove_agent_installation", { request: { provider: "pi", generation: "generation-a" } });
  expect(screen.getByText("v1.3.0")).toBeInTheDocument();
  expect(send.mock.calls.some(([command]) => command.includes("stop") || command.includes("close"))).toBe(false);
});

it("reuses an uncertain download request and gives a completed update a fresh request", async () => {
  let snapshot: RuntimeInstallInventory = { agents: [agent("open_code", { version: "1.0.0", generation: "generation-a" })] };
  let starts = 0;
  const send = vi.fn(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "get_agent_installations") return snapshot;
    if (command === "start_agent_installation") {
      starts += 1;
      if (starts === 1) throw new Error("Download request connection interrupted.");
      const request = payload!.request as { provider: RpcProvider; request_id: string };
      const current = job(request.provider, request.request_id, { state: "completed" });
      snapshot = { agents: [agent("open_code", { version: "1.1.0", generation: "generation-b", job: current })] }; return current;
    }
    throw new Error(`Unexpected command: ${command}`);
  });
  bind(send); render(<AgentInstallations {...props} />);
  fireEvent.click(await screen.findByRole("button", { name: "Update OpenCode" }));
  await screen.findByText("Download request connection interrupted.");
  fireEvent.click(screen.getByRole("button", { name: "Update OpenCode" }));
  await screen.findByText("v1.1.0");
  await waitFor(() => expect(screen.getByRole("button", { name: "Update OpenCode" })).toBeEnabled());
  fireEvent.click(screen.getByRole("button", { name: "Update OpenCode" }));
  await waitFor(() => expect(send.mock.calls.filter(([command]) => command === "start_agent_installation")).toHaveLength(3));
  const calls = send.mock.calls.filter(([command]) => command === "start_agent_installation");
  expect(calls[0][1]).toEqual(calls[1][1]);
  expect(calls[1][1]).not.toEqual(calls[2][1]);
});

it("keeps delayed downloads on their original machine and ignores their result after switching", async () => {
  let resolve!: (value: RuntimeInstallJob) => void;
  const original = vi.fn(async (command: string) => {
    if (command === "get_agent_installations") return { agents: [agent("pi")] };
    if (command === "start_agent_installation") return new Promise<RuntimeInstallJob>((done) => { resolve = done; });
    throw new Error(`Unexpected command: ${command}`);
  });
  bind(original); const view = render(<AgentInstallations key="host-a" {...props} machine_label="Host A" />);
  fireEvent.click(await screen.findByRole("button", { name: "Download Pi" }));
  const next = vi.fn(async (_command: string) => ({ agents: [agent("pi", { version: "2.0.0", generation: "next" })] }));
  bind(next); view.rerender(<AgentInstallations key="host-b" {...props} machine_label="Host B" />);
  await screen.findByText("v2.0.0");
  await act(async () => resolve(job("pi", "old-request", { state: "completed" })));
  expect(screen.getByText("v2.0.0")).toBeInTheDocument();
  expect(next.mock.calls.every(([command]) => command === "get_agent_installations")).toBe(true);
});

it("does not query hidden or disconnected settings and disables stale controls when hidden", async () => {
  const send = vi.fn(async (_command: string) => ({ agents: [agent("codex")] }));
  bind(send); const view = render(<AgentInstallations {...props} active={false} />);
  expect(send).not.toHaveBeenCalled();
  view.rerender(<AgentInstallations {...props} connected={false} />);
  expect(send).not.toHaveBeenCalled();
  view.rerender(<AgentInstallations {...props} />);
  await screen.findByRole("button", { name: "Download Codex" });
  view.rerender(<AgentInstallations {...props} active={false} />);
  expect(screen.getByRole("button", { name: "Download Codex" })).toBeDisabled();
  expect(send.mock.calls.every(([command]) => command === "get_agent_installations")).toBe(true);
});
