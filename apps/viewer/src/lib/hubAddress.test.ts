import { afterEach, expect, it, vi } from "vitest";
import { parseMachineAddress, parseMachineTarget, resolveBrowserMachine, validateResolvedMachine } from "./hubAddress";

const host_id = "550e8400-e29b-41d4-a716-446655440000";
const machine = { host_id, machine_address: "clouds:macbook", name: "MacBook", online: false };
afterEach(() => vi.unstubAllGlobals());

it("accepts canonical readable addresses and independently pinned references", () => {
  expect(parseMachineAddress("clouds:macbook")).toEqual({ username: "clouds", machine_name: "macbook" });
  expect(parseMachineTarget(`clouds:macbook@${"H".repeat(43)}`)).toEqual({ machine_address: "clouds:macbook", host_public_key: "H".repeat(43) });
  expect(parseMachineTarget(host_id)).toEqual({ host_id });
  expect(parseMachineAddress(`${"a".repeat(63)}:1`)).toBeDefined();
  for (const address of ["alice", "alice:", ":host", "Alice:host", "alice:host:extra", "alice:../host", "alice:host_1", "alice:-host", `alice:${"a".repeat(64)}`, "alice:机器"]) {
    expect(() => parseMachineAddress(address)).toThrow();
  }
  expect(() => parseMachineTarget("clouds:macbook@bad")).toThrow();
  expect(() => parseMachineTarget(`clouds:macbook@${"H".repeat(43)}@extra`)).toThrow();
});

it("does not treat a directory's public key as a machine pin", () => {
  expect(validateResolvedMachine({ ...machine, host_public_key: "attacker", public_key: "registration-key" }, machine.machine_address)).toEqual(machine);
  expect(() => validateResolvedMachine({ ...machine, machine_address: "other:host" }, machine.machine_address)).toThrow("invalid");
  expect(() => validateResolvedMachine({ ...machine, host_id: "invalid" }, machine.machine_address)).toThrow("invalid");
});

it("resolves exact addresses while refusing redirects and oversized bodies", async () => {
  const fetcher = vi.fn().mockResolvedValue(new Response(JSON.stringify(machine), { headers: { "Content-Type": "application/json" } }));
  vi.stubGlobal("fetch", fetcher);
  expect(await resolveBrowserMachine("https://hub.example", machine.machine_address, new AbortController().signal)).toEqual(machine);
  expect(fetcher).toHaveBeenCalledWith("https://hub.example/hub/v1/resolve/clouds/macbook", expect.objectContaining({ credentials: "omit", redirect: "error" }));
  fetcher.mockResolvedValue(new Response(" ".repeat(4097), { headers: { "Content-Type": "application/json" } }));
  await expect(resolveBrowserMachine("https://hub.example", machine.machine_address, new AbortController().signal)).rejects.toThrow("too large");
});
