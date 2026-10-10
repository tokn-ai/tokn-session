import { beforeEach, expect, it, vi } from "vitest";
import { withMachineAddressing, type HubAccessBackend } from "./hubAccess";
import type { SavedHubHost } from "./types";

const host = { host_id: "550e8400-e29b-41d4-a716-446655440000", host_public_key: "H".repeat(43) };
const machine = { host_id: host.host_id, machine_address: "clouds:macbook", name: "MacBook", online: true };
let hosts: SavedHubHost[];
let backend: HubAccessBackend;
beforeEach(() => {
  hosts = [];
  backend = {
    hub_url: "https://hub.example",
    status: vi.fn(async () => ({ hosts, selected_host_id: null, device_public_key: "D".repeat(43) })),
    pair: vi.fn(async () => { hosts.push(host); return host; }),
    authenticate: vi.fn(async (saved) => { hosts.push(saved); }),
    connect: vi.fn(), forget: vi.fn(),
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

it("rejects cancellation and explicit pin changes before authorization or metadata persistence", async () => {
  hosts = [host];
  const service = withMachineAddressing(backend);
  await expect(service.loginMachine(`${machine.machine_address}@${"I".repeat(43)}`, signal())).rejects.toThrow("remembered identity");
  const controller = new AbortController();
  vi.mocked(backend.resolve).mockImplementation(async () => { controller.abort(); return machine; });
  await expect(service.pairMachine(machine.machine_address, "123456", controller.signal)).rejects.toThrow("disconnected");
  expect(backend.pair).not.toHaveBeenCalled(); expect(backend.authenticate).not.toHaveBeenCalled(); expect(backend.rememberMetadata).not.toHaveBeenCalled();
});
