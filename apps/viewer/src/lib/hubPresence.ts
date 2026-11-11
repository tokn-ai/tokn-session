import type { PresenceRequest, PresenceResult } from "./types";

export const MAX_PRESENCE_HOSTS = 128;
const MAX_RESPONSE = 24 * 1024;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

export function validatePresenceIds(host_ids: string[]): void {
  if (!Array.isArray(host_ids) || host_ids.length > MAX_PRESENCE_HOSTS
    || host_ids.some((id) => typeof id !== "string" || !UUID.test(id)) || new Set(host_ids).size !== host_ids.length) {
    throw new Error("Invalid machine presence request.");
  }
}

export function validatePresenceResult(value: unknown, host_ids: string[]): PresenceResult {
  validatePresenceIds(host_ids);
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Hub returned invalid machine presence.");
  const machines = (value as PresenceResult).machines;
  if (!Array.isArray(machines) || machines.length !== host_ids.length) throw new Error("Hub returned incomplete machine presence.");
  const remaining = new Set(host_ids);
  const validated = machines.map((machine) => {
    if (!machine || typeof machine !== "object" || typeof machine.host_id !== "string"
      || typeof machine.online !== "boolean" || !remaining.delete(machine.host_id)) {
      throw new Error("Hub returned invalid machine presence.");
    }
    const { connector_version } = machine;
    if (connector_version !== undefined && (
      !machine.online || typeof connector_version !== "string" || connector_version.length > 64
      || !/^[0-9]/.test(connector_version) || /[^A-Za-z0-9.+-]/.test(connector_version)
    )) throw new Error("Hub returned an invalid connector version.");
    return {
      host_id: machine.host_id,
      online: machine.online,
      ...(connector_version === undefined ? {} : { connector_version }),
    };
  });
  return { machines: validated };
}

export async function loadBrowserPresence(hub_url: string, host_ids: string[], signal: AbortSignal): Promise<PresenceResult> {
  validatePresenceIds(host_ids);
  if (signal.aborted) throw new Error("Machine disconnected");
  if (host_ids.length === 0) return { machines: [] };
  const controller = new AbortController();
  const abort = () => controller.abort();
  signal.addEventListener("abort", abort, { once: true });
  const timeout = setTimeout(abort, 10_000);
  try {
    const request: PresenceRequest = { host_ids };
    const response = await fetch(`${hub_url}/hub/v1/presence`, {
      method: "POST", body: JSON.stringify(request),
      headers: { Accept: "application/json", "Content-Type": "application/json", "X-Tokn-Presence-Versions": "1" },
      signal: controller.signal, credentials: "omit", redirect: "error", cache: "no-store",
    });
    if (!response.ok) throw new Error(`Could not check machine presence (${response.status}).`);
    if (response.headers.get("content-type")?.split(";")[0].trim() !== "application/json") throw new Error("Hub returned invalid machine presence.");
    const reader = response.body?.getReader();
    if (!reader) throw new Error("Hub returned an empty presence response.");
    const chunks: Uint8Array[] = [];
    let length = 0;
    try {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        length += chunk.value.byteLength;
        if (length > MAX_RESPONSE) throw new Error("Hub presence response is too large.");
        chunks.push(chunk.value);
      }
    } finally { await reader.cancel(); }
    if (signal.aborted) throw new Error("Machine disconnected");
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
    return validatePresenceResult(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)), host_ids);
  } finally { clearTimeout(timeout); signal.removeEventListener("abort", abort); }
}
