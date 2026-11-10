import { afterEach, expect, it, vi } from "vitest";
import { discoverBrowserGroup, validateDiscoveryGroup, validateDiscoveryResult } from "./hubDiscovery";

const group = { namespace: "personal", secret_digest: "A".repeat(43) };
const machine = { host_id: "550e8400-e29b-41d4-a716-446655440000", name: "Laptop", online: true };
afterEach(() => vi.unstubAllGlobals());

it("validates canonical group references and partitions equal names by digest", () => {
  expect(validateDiscoveryGroup(group)).toEqual(group);
  for (const value of [null, [], { ...group, namespace: "Personal" }, { ...group, namespace: "a/b" }, { ...group, secret_digest: "B".repeat(43) }, { ...group, secret_digest: "123456" }]) {
    expect(() => validateDiscoveryGroup(value)).toThrow();
  }
  expect(() => validateDiscoveryResult({ ...group, secret_digest: `${"B".repeat(42)}A`, machines: [] }, group)).toThrow("different discovery group");
});

it("keeps directory candidates separate from trusted machine pins and rejects duplicate or invalid rows", () => {
  expect(validateDiscoveryResult({ ...group, machines: [{ ...machine, host_public_key: "untrusted", discovery_group: group }] }, group)).toEqual({ ...group, machines: [machine] });
  for (const machines of [[machine, machine], [{ ...machine, online: "true" }], [{ ...machine, host_id: "bad" }], [{ ...machine, name: "a\n" }], Array(65).fill(machine)]) {
    expect(() => validateDiscoveryResult({ ...group, machines }, group)).toThrow("invalid machine discovery list");
  }
});

it("retains a negotiated canonical address without accepting an invalid address", () => {
  const registered = { ...machine, machine_address: "personal:laptop" };
  expect(validateDiscoveryResult({ ...group, machines: [registered] }, group).machines).toEqual([registered]);
  expect(() => validateDiscoveryResult({ ...group, machines: [{ ...machine, machine_address: "personal:Laptop" }] }, group)).toThrow();
  expect(() => validateDiscoveryResult({ ...group, machines: [{ ...machine, machine_address: "other:laptop" }] }, group)).toThrow("outside the discovery namespace");
});

it("fetches the exact group without credentials, redirects or cached discovery and bounds the body", async () => {
  const fetcher = vi.fn().mockResolvedValue(new Response(JSON.stringify({ ...group, machines: [machine] }), { headers: { "Content-Type": "application/json" } }));
  vi.stubGlobal("fetch", fetcher);
  expect(await discoverBrowserGroup("https://hub.example", group, new AbortController().signal)).toEqual({ ...group, machines: [machine] });
  expect(fetcher).toHaveBeenCalledWith(`https://hub.example/hub/v1/discovery/personal/${group.secret_digest}`, expect.objectContaining({
    credentials: "omit", redirect: "error", cache: "no-store",
    headers: { Accept: "application/json", "X-Tokn-Discovery-Addresses": "1" },
  }));
  fetcher.mockResolvedValue(new Response(" ".repeat(32769), { headers: { "Content-Type": "application/json" } }));
  await expect(discoverBrowserGroup("https://hub.example", group, new AbortController().signal)).rejects.toThrow("too large");
});

it("discards a response when its discovery request was canceled", async () => {
  const controller = new AbortController();
  const fetcher = vi.fn().mockImplementation(async () => {
    controller.abort();
    return new Response(JSON.stringify({ ...group, machines: [] }), { headers: { "Content-Type": "application/json" } });
  });
  vi.stubGlobal("fetch", fetcher);
  await expect(discoverBrowserGroup("https://hub.example", group, controller.signal)).rejects.toThrow("disconnected");
});
