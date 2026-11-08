import { parseMachineAddress } from "./hubAddress";
import type { DiscoveryGroup, DiscoveryResult, DiscoveredMachine } from "./types";

const DIGEST = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const MAX_RESPONSE = 32 * 1024;

export function validateDiscoveryGroup(value: unknown): DiscoveryGroup {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid machine discovery group.");
  const group = value as DiscoveryGroup;
  if (typeof group.namespace !== "string" || typeof group.secret_digest !== "string" || !DIGEST.test(group.secret_digest)) {
    throw new Error("Invalid machine discovery group.");
  }
  parseMachineAddress(`${group.namespace}:machine`);
  return { namespace: group.namespace, secret_digest: group.secret_digest };
}

export function discoveryGroupKey(group: DiscoveryGroup): string {
  return `${group.namespace}/${group.secret_digest}`;
}

export function validateDiscoveryResult(value: unknown, expected: DiscoveryGroup): DiscoveryResult {
  const group = validateDiscoveryGroup(value);
  if (discoveryGroupKey(group) !== discoveryGroupKey(expected)) throw new Error("Hub returned a different discovery group.");
  const machines = (value as DiscoveryResult).machines;
  if (!Array.isArray(machines) || machines.length > 64) throw new Error("Hub returned an invalid machine discovery list.");
  const seen = new Set<string>();
  const validated = machines.map((machine: DiscoveredMachine) => {
    if (!machine || typeof machine !== "object" || typeof machine.host_id !== "string" || !UUID.test(machine.host_id)
      || typeof machine.name !== "string" || !machine.name.trim() || new TextEncoder().encode(machine.name).length > 128
      || /[\u0000-\u001f\u007f-\u009f]/.test(machine.name) || typeof machine.online !== "boolean" || seen.has(machine.host_id)) {
      throw new Error("Hub returned an invalid machine discovery list.");
    }
    seen.add(machine.host_id);
    return { host_id: machine.host_id, name: machine.name, online: machine.online };
  });
  return { ...group, machines: validated };
}

export async function discoverBrowserGroup(hub_url: string, reference: DiscoveryGroup, signal: AbortSignal): Promise<DiscoveryResult> {
  const group = validateDiscoveryGroup(reference);
  const controller = new AbortController();
  const abort = () => controller.abort();
  if (signal.aborted) abort();
  signal.addEventListener("abort", abort, { once: true });
  const timeout = setTimeout(abort, 15_000);
  try {
    const response = await fetch(`${hub_url}/hub/v1/discovery/${group.namespace}/${group.secret_digest}`, {
      signal: controller.signal, credentials: "omit", redirect: "error", cache: "no-store", headers: { Accept: "application/json" },
    });
    if (!response.ok) throw new Error(`Could not discover group machines (${response.status}).`);
    if (!response.headers.get("content-type")?.includes("application/json")) throw new Error("Hub returned an invalid machine discovery list.");
    const reader = response.body?.getReader();
    if (!reader) throw new Error("Hub returned an empty machine discovery response.");
    const chunks: Uint8Array[] = [];
    let length = 0;
    try {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        length += chunk.value.byteLength;
        if (length > MAX_RESPONSE) throw new Error("Hub machine discovery response is too large.");
        chunks.push(chunk.value);
      }
    } finally { await reader.cancel(); }
    if (signal.aborted) throw new Error("Machine disconnected");
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
    return validateDiscoveryResult(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)), group);
  } finally {
    clearTimeout(timeout);
    signal.removeEventListener("abort", abort);
  }
}
