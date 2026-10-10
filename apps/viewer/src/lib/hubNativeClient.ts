import { type SavedHubHost } from "./hubDeviceStore";
import type { ConnectionState, UnlistenFn, ViewerClient } from "./transport";
import type { PasskeyProvider } from "./hubEncryptedClient";
import type { ResolvedMachine } from "./types";

export interface NativeHubStatus {
  hub_url: string;
  device_public_key: string;
  hosts: SavedHubHost[];
  selected_host_id: string | null;
}
interface Descriptor extends SavedHubHost { connection_id: string; endpoint: string; }
async function command<T>(name: string, request: Record<string, unknown>): Promise<T> {
  return (await import("@tauri-apps/api/core")).invoke<T>(name, request);
}
export function nativeHubStatus(hub_url: string): Promise<NativeHubStatus> { return command("hub_client_status", { hub_url }); }
export function nativeHubPair(hub_url: string, host_id: string, code: string, expected_host_public_key?: string, machine_address?: string): Promise<SavedHubHost> { return command("hub_client_pair", { hub_url, host_id, code, expected_host_public_key, ...(machine_address ? { machine_address } : {}) }); }
export function nativeHubForget(hub_url: string, host_id: string): Promise<void> { return command("hub_client_forget", { hub_url, host_id }); }
export function nativeHubResolve(hub_url: string, machine_address: string): Promise<ResolvedMachine> {
  return command("hub_client_resolve", { hub_url, machine_address });
}
export function nativeHubRememberMetadata(hub_url: string, machine: ResolvedMachine): Promise<SavedHubHost> {
  return command("hub_client_remember_metadata", { hub_url, host_id: machine.host_id, machine_address: machine.machine_address, name: machine.name });
}
export function nativePasskeys(hub_url: string, auth_id?: string): PasskeyProvider {
  return {
    create: (options) => command("hub_client_passkey_credential", { hub_url, auth_id, options: { publicKey: options }, register: true }),
    get: (options) => command("hub_client_passkey_credential", { hub_url, auth_id, options: { publicKey: options }, register: false }),
  };
}
export async function nativeHubAuthenticate(hub_url: string, host: SavedHubHost, register: boolean, signal: AbortSignal, provider?: PasskeyProvider): Promise<SavedHubHost> {
  const start = await command<{ auth_id: string; options: { publicKey: Parameters<PasskeyProvider["create"]>[0] } }>("hub_client_auth_start", { hub_url, host_id: host.host_id, host_public_key: host.host_public_key, register });
  const cancel = () => { void command("hub_client_auth_cancel", { auth_id: start.auth_id }).catch(() => {}); };
  signal.addEventListener("abort", cancel, { once: true });
  try {
    if (signal.aborted) throw new Error("Machine disconnected");
    const passkeys = provider ?? nativePasskeys(hub_url, start.auth_id);
    const credential = register ? await passkeys.create(start.options.publicKey, signal) : await passkeys.get(start.options.publicKey, signal);
    if (signal.aborted) throw new Error("Machine disconnected");
    return await command("hub_client_auth_finish", { auth_id: start.auth_id, credential });
  } finally { signal.removeEventListener("abort", cancel); cancel(); }
}

/** Native Rust owns sockets and private keys; the UI receives only JSON/events. */
export class NativeHubClient implements ViewerClient {
  readonly endpoint: string;
  private closed = false;
  private listeners = new Map<string, Set<(event: { payload: unknown }) => void>>();
  private close_handlers = new Set<() => void>();
  private stops: UnlistenFn[] = [];
  private started?: Promise<void>;
  private on_state: (state: ConnectionState) => void = () => {};
  private constructor(private descriptor: Descriptor) { this.endpoint = descriptor.endpoint; }
  static async connect(hub_url: string, host: SavedHubHost, signal?: AbortSignal): Promise<NativeHubClient> {
    if (signal?.aborted) throw new Error("Machine disconnected");
    const descriptor = await command<Descriptor>("hub_client_open", { hub_url, host_id: host.host_id, host_public_key: host.host_public_key });
    const client = new NativeHubClient(descriptor);
    if (signal?.aborted) { client.close(); throw new Error("Machine disconnected"); }
    return client;
  }
  async invoke<T>(name: string, payload: unknown = {}): Promise<T> {
    if (this.closed) throw new Error("Machine disconnected");
    const result = await command<T>("hub_client_request", { connection_id: this.descriptor.connection_id, command: name, payload });
    if (this.closed) throw new Error("Machine disconnected");
    return result;
  }
  setStateListener(handler: (state: ConnectionState) => void): void { this.on_state = handler; }
  private async start(): Promise<void> {
    try {
      const { listen } = await import("@tauri-apps/api/event");
      const stop_events = await listen<{ connection_id: string; event: string; payload: unknown }>("hub-client-event", ({ payload }) => {
        if (this.closed || payload.connection_id !== this.descriptor.connection_id) return;
        for (const handler of this.listeners.get(payload.event) ?? []) handler({ payload: payload.payload });
      });
      this.stops.push(stop_events);
      const stop_state = await listen<{ connection_id: string; state: ConnectionState }>("hub-client-state", ({ payload }) => {
        if (!this.closed && payload.connection_id === this.descriptor.connection_id) this.on_state(payload.state);
      });
      this.stops.push(stop_state);
      if (this.closed) throw new Error("Machine disconnected");
      await command("hub_client_listen", { connection_id: this.descriptor.connection_id });
      if (this.closed) throw new Error("Machine disconnected");
    } catch (error) {
      this.stops.splice(0).forEach((stop) => stop());
      this.close();
      throw error;
    }
  }
  async listen<T>(name: string, handler: (event: { payload: T }) => void): Promise<UnlistenFn> {
    if (this.closed) throw new Error("Machine disconnected");
    const handlers = this.listeners.get(name) ?? new Set<(event: { payload: unknown }) => void>();
    const observer = handler as (event: { payload: unknown }) => void;
    handlers.add(observer); this.listeners.set(name, handlers);
    this.started ??= this.start();
    try { await this.started; } catch (error) { handlers.delete(observer); throw error; }
    return () => handlers.delete(observer);
  }
  onClose(handler: () => void): UnlistenFn { if (this.closed) return () => {}; this.close_handlers.add(handler); return () => this.close_handlers.delete(handler); }
  async release(name: string, payload?: Record<string, unknown>): Promise<void> { if (!this.closed) await this.invoke(name, payload); }
  close(): void {
    if (this.closed) return;
    for (const handler of this.close_handlers) handler();
    this.closed = true; this.close_handlers.clear(); this.listeners.clear();
    this.stops.splice(0).forEach((stop) => stop());
    void command("hub_client_close", { connection_id: this.descriptor.connection_id }).catch(() => {});
  }
}
