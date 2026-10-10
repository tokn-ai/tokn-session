import type { ResolvedMachine } from "./types";

const SLUG = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const KEY = /^[A-Za-z0-9_-]{43}$/;

export interface MachineTarget {
  host_id?: string;
  machine_address?: string;
  host_public_key?: string;
}

export function parseMachineAddress(value: string): { username: string; machine_name: string } {
  if (typeof value !== "string") throw new Error("Enter a machine address as username:host.");
  const parts = value.split(":");
  if (parts.length !== 2 || !parts.every((part) => SLUG.test(part))) {
    throw new Error("Enter username:host using lowercase letters, numbers, and internal hyphens (1–63 characters each).");
  }
  return { username: parts[0], machine_name: parts[1] };
}

export function parseMachineTarget(value: string): MachineTarget {
  const parts = value.trim().split("@");
  if (parts.length > 2 || (parts.length === 2 && !KEY.test(parts[1]))) throw new Error("Invalid machine reference. Use username:host or a UUID, optionally followed by @ and the verified machine key.");
  const selector = parts[0];
  const identity = UUID.test(selector) ? { host_id: selector } : (parseMachineAddress(selector), { machine_address: selector });
  return { ...identity, ...(parts.length === 2 ? { host_public_key: parts[1] } : {}) };
}

export function validateResolvedMachine(value: unknown, machine_address: string): ResolvedMachine {
  parseMachineAddress(machine_address);
  if (!value || typeof value !== "object") throw new Error("Hub returned an invalid machine address.");
  const result = value as ResolvedMachine;
  if (result.machine_address !== machine_address || !UUID.test(result.host_id)
    || typeof result.online !== "boolean" || typeof result.name !== "string"
    || !result.name.trim() || new TextEncoder().encode(result.name).length > 128 || /[\u0000-\u001f\u007f-\u009f]/.test(result.name)) {
    throw new Error("Hub returned an invalid machine address.");
  }
  // Ignore unknown fields, including any public key sent by a directory.
  return { host_id: result.host_id, machine_address, name: result.name, online: result.online };
}

export async function resolveBrowserMachine(hub_url: string, machine_address: string, signal: AbortSignal): Promise<ResolvedMachine> {
  const { username, machine_name } = parseMachineAddress(machine_address);
  const controller = new AbortController();
  const abort = () => controller.abort();
  if (signal.aborted) abort();
  signal.addEventListener("abort", abort, { once: true });
  const timeout = setTimeout(abort, 15_000);
  try {
    const response = await fetch(`${hub_url}/hub/v1/resolve/${username}/${machine_name}`, {
      signal: controller.signal, credentials: "omit", redirect: "error", headers: { Accept: "application/json" },
    });
    if (response.status === 404) throw new Error("This machine address is not registered. Check its username and host name.");
    if (!response.ok) throw new Error(`Could not resolve the machine address (${response.status}).`);
    if (!response.headers.get("content-type")?.includes("application/json")) throw new Error("Hub returned an invalid machine address.");
    const reader = response.body?.getReader();
    if (!reader) throw new Error("Hub returned an empty machine address.");
    const chunks: Uint8Array[] = [];
    let length = 0;
    try {
      for (;;) {
        const next = await reader.read();
        if (next.done) break;
        length += next.value.byteLength;
        if (length > 4096) throw new Error("Hub machine address response is too large.");
        chunks.push(next.value);
      }
    } finally { await reader.cancel(); }
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
    return validateResolvedMachine(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)), machine_address);
  } finally {
    clearTimeout(timeout); signal.removeEventListener("abort", abort);
  }
}
