import { beforeEach, expect, it, vi } from "vitest";
import { withMachineAddressing, type HubAccessBackend } from "./hubAccess";
import type { SavedHubHost } from "./types";
import { RemoteClient } from "./transport";

const host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43) };
const machine = { host_id: host.host_id, machine_address: "alice:macbook", name: "MacBook", online: true };
let hosts: SavedHubHost[];
let backend: HubAccessBackend;
beforeEach(() => {
  hosts = [];
  backend = {
    hub_url: "https://hub.example",
    status: vi.fn(async () => ({ hosts, selected_host_id: null, device_public_key: "D".repeat(43) })),
    pair: vi.fn(async () => { hosts.push(host); return host; }),
    authenticate: vi.fn(async (saved) => { hosts.push(saved); }),
    connect: vi.fn(), forget: vi.fn(), discover: vi.fn(), presence: vi.fn(),
    resolve: vi.fn(async () => machine),
    rememberMetadata: vi.fn(async (resolved) => ({ ...host, machine_address: resolved.machine_address, name: resolved.name })),
  };
});
const signal = () => new AbortController().signal;

it("pairs a readable address using the resolved UUID and host-confirmed key", async () => {
  const service = withMachineAddressing(backend);
  expect(await service.pairMachine(machine.machine_address, "123456", signal())).toEqual({ ...host, machine_address: machine.machine_address, name: machine.name });
  expect(backend.pair).toHaveBeenCalledWith(host.host_id, "123456", expect.any(AbortSignal), undefined, machine);
  expect(backend.rememberMetadata).toHaveBeenCalledWith(machine, expect.any(AbortSignal));
});

it("refuses first-device passkey login without a separately supplied pin", async () => {
  const service = withMachineAddressing(backend);
  await expect(service.loginMachine(machine.machine_address, signal())).rejects.toThrow("complete machine reference");
  expect(backend.authenticate).not.toHaveBeenCalled();
  await service.loginMachine(`${machine.machine_address}@${host.host_public_key}`, signal());
  expect(backend.authenticate).toHaveBeenCalledWith(host, false, expect.any(AbortSignal));
});

it("reuses remembered pins and rejects a directory remapping an address", async () => {
  hosts = [{ ...host, machine_address: machine.machine_address }];
  const service = withMachineAddressing(backend);
  await service.loginMachine(machine.machine_address, signal());
  expect(backend.authenticate).toHaveBeenCalledWith(hosts[0], false, expect.any(AbortSignal));
  vi.mocked(backend.resolve).mockResolvedValue({ ...machine, host_id: "550e8400-e29b-41d4-a716-446655440001" });
  await expect(service.pairMachine(machine.machine_address, "123456", signal())).rejects.toThrow("another machine");
  expect(backend.pair).not.toHaveBeenCalled();
});

it("refreshes a renamed machine address after authenticating its remembered pin", async () => {
  const saved = { ...host, machine_address: "alice:old-name" };
  hosts = [saved];
  const service = withMachineAddressing(backend);
  expect(await service.loginMachine(machine.machine_address, signal())).toEqual({ ...host, machine_address: machine.machine_address, name: machine.name });
  expect(backend.authenticate).toHaveBeenCalledWith(saved, false, expect.any(AbortSignal));
  expect(backend.rememberMetadata).toHaveBeenCalledWith(machine, expect.any(AbortSignal));
  expect(vi.mocked(backend.authenticate).mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(backend.rememberMetadata).mock.invocationCallOrder[0]);
  vi.mocked(backend.authenticate).mockRejectedValue(new Error("Machine key did not authenticate"));
  vi.mocked(backend.rememberMetadata).mockClear();
  await expect(service.loginMachine(machine.machine_address, signal())).rejects.toThrow("did not authenticate");
  expect(backend.rememberMetadata).not.toHaveBeenCalled();
});

it("rejects cancellation and explicit pin changes before authorization or metadata persistence", async () => {
  hosts = [host];
  const service = withMachineAddressing(backend);
  await expect(service.loginMachine(`${machine.machine_address}@${"I".repeat(43)}`, signal())).rejects.toThrow("remembered identity");
  const controller = new AbortController();
  vi.mocked(backend.resolve).mockImplementation(async () => { controller.abort(); return machine; });
  await expect(service.pairMachine(machine.machine_address, "123456", controller.signal)).rejects.toThrow("disconnected");
  expect(backend.pair).not.toHaveBeenCalled(); expect(backend.authenticate).not.toHaveBeenCalled(); expect(backend.rememberMetadata).not.toHaveBeenCalled();
});

it("refreshes a saved card's renamed address after its pinned connection opens", async () => {
  const group = { namespace: "alice", secret_digest: "G".repeat(43) };
  const saved = { ...host, machine_address: "alice:old-name", discovery_group: group };
  hosts = [saved];
  const client = new RemoteClient("http://localhost", "");
  const connect = vi.mocked(backend.connect).mockResolvedValue(client);
  vi.mocked(backend.discover).mockResolvedValue({ ...group, machines: [machine] });
  const service = withMachineAddressing(backend);
  expect(await service.connect(saved, signal())).toBe(client);
  expect(backend.rememberMetadata).toHaveBeenCalledWith(machine, expect.any(AbortSignal));
  expect(connect.mock.invocationCallOrder[0]).toBeLessThan(vi.mocked(backend.rememberMetadata).mock.invocationCallOrder[0]);
  vi.mocked(backend.discover).mockRejectedValue(new Error("Hub is unavailable"));
  vi.mocked(backend.rememberMetadata).mockClear();
  expect(await service.connect(saved, signal())).toBe(client);
  expect(backend.rememberMetadata).not.toHaveBeenCalled();
  client.close();
});
