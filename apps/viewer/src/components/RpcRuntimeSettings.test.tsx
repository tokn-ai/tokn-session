import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { RpcRuntimeSettings } from "./RpcRuntimeSettings";
import { captureTransport, type CommandInvoker } from "../lib/transport";
import type { RpcRuntimeInfo, RpcRuntimeProfile, RpcRuntimeProfilesSnapshot } from "../lib/types";
vi.mock("../lib/transport", async (original) => ({ ...await original<typeof import("../lib/transport")>(), captureTransport: vi.fn(), isDesktop: () => false }));
const profile: RpcRuntimeProfile = { profile_id: "primary", name: "Remote Codex", provider: "codex", mode: "attached", executable: null,
  arguments: [], working_directory: null, endpoint: "ws://127.0.0.1:9876", environment_keys: [], credential_kind: "bearer", is_default: true };
const runtime: RpcRuntimeInfo = { runtime_id: "runtime-a", instance_id: "instance-a", profile_id: "primary", profile_revision: 3,
  name: "Remote Codex", provider: "codex", ownership: "attached", status: "connected", session_count: 0, last_error: null,
  capabilities: { operations: { models: { supported: true, reason: null } }, atomic_model_on_send: false } };
const props = { active: true, connected: true, machine_key: "host-a", machine_label: "Host A", heading_id: "rpc-title" };
function bind(send: ReturnType<typeof vi.fn>) {
  vi.mocked(captureTransport).mockReturnValue({ invoke: send as CommandInvoker, listen: vi.fn().mockResolvedValue(() => {}), release: vi.fn(), on_close: () => () => {} });
}
function fixture(profiles: RpcRuntimeProfile[] = [profile]) {
  let snapshot: RpcRuntimeProfilesSnapshot = { version: 1, revision: 3, profiles };
  let runtimes: RpcRuntimeInfo[] = [];
  return vi.fn(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "get_rpc_runtime_profiles") return snapshot;
    if (command === "list_rpc_runtimes") return { version: 1, runtimes };
    if (command === "save_rpc_runtime_profile") {
      const request = payload!.request as { profile: RpcRuntimeProfile };
      snapshot = { ...snapshot, revision: snapshot.revision + 1, profiles: [{ ...profile, ...request.profile, profile_id: request.profile.profile_id ?? "created" }] };
      return snapshot;
    }
    if (command === "delete_rpc_runtime_profile") { snapshot = { ...snapshot, revision: snapshot.revision + 1, profiles: [] }; return snapshot; }
    if (command === "validate_rpc_runtime_profile") return { valid: true, issues: [], executable_exists: null, working_directory_exists: null };
    if (command === "start_rpc_runtime") { runtimes = [runtime]; return runtime; }
    if (command === "close_rpc_runtime") { runtimes = []; return { runtime_id: runtime.runtime_id, instance_id: runtime.instance_id, closed: true }; }
    throw new Error(`Unexpected command: ${command}`);
  });
}
beforeEach(() => vi.clearAllMocks());
afterEach(cleanup);

it("loads the selected remote machine and preserves write-only credentials on edit", async () => {
  const send = fixture(); bind(send); render(<RpcRuntimeSettings {...props} />);
  await screen.findByRole("button", { name: "Edit Remote Codex" });
  expect(screen.getByText("Host A")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Edit Remote Codex" }));
  expect(screen.queryByLabelText("Bearer token")).not.toBeInTheDocument();
  expect(screen.getByLabelText("Authentication")).toHaveValue("keep");
  fireEvent.change(screen.getByLabelText("Profile name"), { target: { value: "Renamed" } });
  fireEvent.click(screen.getByRole("button", { name: "Validate on host" }));
  await screen.findByText("Profile is valid on this machine.");
  fireEvent.click(screen.getByRole("button", { name: "Save profile" }));
  await screen.findByRole("button", { name: "Edit Renamed" });
  expect(send).toHaveBeenCalledWith("save_rpc_runtime_profile", { request: { expected_revision: 3, profile: {
    profile_id: "primary", name: "Renamed", provider: "codex", mode: "attached", executable: null, arguments: [], working_directory: null,
    endpoint: "ws://127.0.0.1:9876", environment: {}, credentials: null, is_default: true,
  } } });
});

it("creates and deletes a profile with typed argument/environment parsing", async () => {
  const send = fixture([]); bind(send); render(<RpcRuntimeSettings {...props} />);
  await screen.findByText("No agent runtime profiles yet.");
  fireEvent.click(screen.getByRole("button", { name: "Add profile" }));
  fireEvent.change(screen.getByLabelText("Profile name"), { target: { value: "Local agent" } });
  fireEvent.change(screen.getByLabelText("Executable"), { target: { value: "/usr/local/bin/codex" } });
  fireEvent.change(screen.getByLabelText("Working directory"), { target: { value: "/project" } });
  fireEvent.change(screen.getByLabelText("Arguments"), { target: { value: '["--config", "value"]' } });
  fireEvent.change(screen.getByLabelText("Environment variables"), { target: { value: "replace" } });
  fireEvent.change(screen.getByLabelText("Replacement environment"), { target: { value: '{"MODEL_URL":"http://localhost:8000"}' } });
  fireEvent.click(screen.getByRole("button", { name: "Save profile" }));
  await screen.findByRole("button", { name: "Edit Local agent" });
  expect(send).toHaveBeenCalledWith("save_rpc_runtime_profile", { request: { expected_revision: 3, profile: expect.objectContaining({
    arguments: ["--config", "value"], environment: { MODEL_URL: "http://localhost:8000" }, credentials: { type: "none" }, profile_id: null,
  }) } });
  fireEvent.click(screen.getByRole("button", { name: "Delete Local agent" }));
  fireEvent.click(screen.getByRole("button", { name: "Delete profile" }));
  await screen.findByText("No agent runtime profiles yet.");
  expect(send).toHaveBeenCalledWith("delete_rpc_runtime_profile", { request: { expected_revision: 4, profile_id: "created" } });
});

it("connects and detaches an exact external instance without offering process stop", async () => {
  const send = fixture(); bind(send); render(<RpcRuntimeSettings {...props} />);
  await screen.findByRole("button", { name: "Connect Remote Codex" });
  fireEvent.click(screen.getByRole("button", { name: "Connect Remote Codex" }));
  await screen.findByRole("button", { name: "Detach Remote Codex" });
  expect(send).toHaveBeenCalledWith("start_rpc_runtime", { request: { request_id: expect.any(String), profile_id: "primary", expected_revision: 3 } });
  expect(screen.queryByRole("button", { name: "Stop Remote Codex" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Detach Remote Codex" }));
  await screen.findByRole("button", { name: "Connect Remote Codex" });
  expect(send).toHaveBeenCalledWith("close_rpc_runtime", { request: { request_id: expect.any(String), runtime_id: "runtime-a", expected_instance_id: "instance-a" } });
});

it("retains start request identity on a delivery failure so retry cannot launch twice", async () => {
  const base = fixture();
  const send = vi.fn((command: string, payload?: Record<string, unknown>) => command === "start_rpc_runtime" ? Promise.reject(new Error("Connection interrupted")) : base(command, payload));
  bind(send); render(<RpcRuntimeSettings {...props} />);
  await screen.findByRole("button", { name: "Connect Remote Codex" });
  fireEvent.click(screen.getByRole("button", { name: "Connect Remote Codex" }));
  await screen.findByText("Connection interrupted");
  fireEvent.click(screen.getByRole("button", { name: "Connect Remote Codex" }));
  await waitFor(() => expect(send.mock.calls.filter(([command]) => command === "start_rpc_runtime")).toHaveLength(2));
  const calls = send.mock.calls.filter(([command]) => command === "start_rpc_runtime");
  expect(calls[0][1]).toEqual(calls[1][1]);
});

it("offers a fresh attempt only after explicit refresh verifies no runtime at the same revision", async () => {
  const base = fixture();
  const send = vi.fn((command: string, payload?: Record<string, unknown>) => command === "start_rpc_runtime" ? Promise.reject(new Error("Endpoint unavailable")) : base(command, payload));
  bind(send); render(<RpcRuntimeSettings {...props} />);
  await screen.findByRole("button", { name: "Connect Remote Codex" });
  fireEvent.click(screen.getByRole("button", { name: "Connect Remote Codex" }));
  await screen.findByText("Endpoint unavailable");
  expect(screen.queryByRole("button", { name: "New connection attempt for Remote Codex" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  const fresh = await screen.findByRole("button", { name: "New connection attempt for Remote Codex" });
  fireEvent.click(fresh);
  await waitFor(() => expect(send.mock.calls.filter(([command]) => command === "start_rpc_runtime")).toHaveLength(2));
  const calls = send.mock.calls.filter(([command]) => command === "start_rpc_runtime");
  expect(calls[0][1]).not.toEqual(calls[1][1]);
  await screen.findByText("Endpoint unavailable");
  expect(screen.queryByRole("button", { name: "New connection attempt for Remote Codex" })).not.toBeInTheDocument();
});

it.each(["connected", "cleanup_failed"] as const)("does not offer a fresh attempt when refreshed inventory contains a %s runtime", async (status) => {
  const base = fixture();
  let found = false;
  const send = vi.fn((command: string, payload?: Record<string, unknown>) => {
    if (command === "start_rpc_runtime") return Promise.reject(new Error("Connection interrupted"));
    if (command === "list_rpc_runtimes" && found) return Promise.resolve({ version: 1, runtimes: [{ ...runtime, status, last_error: status === "cleanup_failed" ? "Cleanup was not confirmed; restart host." : null }] });
    return base(command, payload);
  });
  bind(send); render(<RpcRuntimeSettings {...props} />);
  await screen.findByRole("button", { name: "Connect Remote Codex" });
  fireEvent.click(screen.getByRole("button", { name: "Connect Remote Codex" }));
  await screen.findByText("Connection interrupted");
  found = true; fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  const detach = await screen.findByRole("button", { name: "Detach Remote Codex" });
  expect(screen.queryByRole("button", { name: "New connection attempt for Remote Codex" })).not.toBeInTheDocument();
  if (status === "cleanup_failed") expect(detach).toBeDisabled();
});

it("does not authorize a fresh attempt from a different saved profile revision", async () => {
  let revision = 3;
  const send = vi.fn(async (command: string) => {
    if (command === "get_rpc_runtime_profiles") return { version: 1, revision, profiles: [profile] };
    if (command === "list_rpc_runtimes") return { version: 1, runtimes: [] };
    throw new Error("Endpoint unavailable");
  });
  bind(send); render(<RpcRuntimeSettings {...props} />);
  await screen.findByRole("button", { name: "Connect Remote Codex" });
  fireEvent.click(screen.getByRole("button", { name: "Connect Remote Codex" }));
  await screen.findByText("Endpoint unavailable");
  revision = 4; fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  await waitFor(() => expect(screen.queryByRole("status")).not.toBeInTheDocument());
  expect(screen.queryByRole("button", { name: "New connection attempt for Remote Codex" })).not.toBeInTheDocument();
});

it("discards old-machine drafts and ignores delayed saves after switching machines", async () => {
  let resolve!: (value: RpcRuntimeProfilesSnapshot) => void;
  const base = fixture();
  const original = vi.fn((command: string, payload?: Record<string, unknown>) => command === "save_rpc_runtime_profile"
    ? new Promise<RpcRuntimeProfilesSnapshot>((done) => { resolve = done; }) : base(command, payload));
  bind(original);
  const view = render(<RpcRuntimeSettings {...props} />);
  await screen.findByRole("button", { name: "Edit Remote Codex" });
  fireEvent.click(screen.getByRole("button", { name: "Edit Remote Codex" }));
  fireEvent.change(screen.getByLabelText("Profile name"), { target: { value: "Old draft" } });
  fireEvent.click(screen.getByRole("button", { name: "Save profile" }));
  const next = fixture([]); bind(next);
  view.rerender(<RpcRuntimeSettings {...props} machine_key="host-b" machine_label="Host B" />);
  await screen.findByText("No agent runtime profiles yet.");
  fireEvent.click(screen.getByRole("button", { name: "Add profile" }));
  expect(screen.getByLabelText("Profile name")).toHaveValue("");
  await act(async () => resolve({ version: 1, revision: 4, profiles: [{ ...profile, name: "Old draft" }] }));
  expect(screen.getByLabelText("Profile name")).toHaveValue("");
  expect(screen.queryByText("Old draft")).not.toBeInTheDocument();
  expect(next.mock.calls.some(([command]) => command === "save_rpc_runtime_profile")).toBe(false);
});

it("does not read or send while hidden or disconnected, and explains permission failures", async () => {
  const send = vi.fn().mockRejectedValue(new Error("Remote runtime management is disabled by this host.")); bind(send);
  const view = render(<RpcRuntimeSettings {...props} active={false} />);
  expect(send).not.toHaveBeenCalled();
  view.rerender(<RpcRuntimeSettings {...props} connected={false} />);
  expect(send).not.toHaveBeenCalled();
  expect(screen.getByText("Connect to a machine to manage its agent runtimes.")).toBeInTheDocument();
  view.rerender(<RpcRuntimeSettings {...props} />);
  await screen.findByRole("alert");
  expect(within(screen.getByRole("alert")).getByText("Remote runtime management is disabled by this host.")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Add profile" })).toBeDisabled();
});
