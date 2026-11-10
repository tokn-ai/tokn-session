import { HubClient, HubError } from "./hub";
import { parseMachineAddress, validateResolvedMachine } from "./hubAddress";
import type { ResolvedMachine } from "./types";

const ACCOUNT = "account";
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
export const MACHINE_READ = "machines:read";
export const MACHINE_REGISTER = "machines:register";
export type AccountKeyScope = typeof MACHINE_READ | typeof MACHINE_REGISTER;

export interface AccountKey {
  key_id: string;
  name: string;
  scopes: AccountKeyScope[];
  expires_at: number;
}

export interface AccountSession {
  username: string;
  client: HubClient;
}

export async function hubAccountsEnabled(client: HubClient): Promise<boolean> {
  try {
    const response = await client.request<{ enabled: boolean }>(`${ACCOUNT}/capabilities`);
    return response?.enabled === true;
  } catch (error) {
    if (error instanceof HubError && error.status === 404) return false;
    throw error;
  }
}

export function requestAccountCode(client: HubClient, email: string): Promise<void> {
  return client.request(`${ACCOUNT}/email/start`, "POST", { email });
}

export async function verifyAccountCode(client: HubClient, email: string, code: string, username?: string): Promise<AccountSession | undefined> {
  try {
    const reply = await client.request<{ user_id: string; username: string; token: string }>(`${ACCOUNT}/email/verify`, "POST", { email, code, username });
    if (typeof reply?.username !== "string" || !UUID.test(reply.user_id) || typeof reply.token !== "string" || !/^tkna_[A-Za-z0-9_-]{43}$/.test(reply.token)) {
      throw new Error("Hub returned an invalid account session.");
    }
    parseMachineAddress(`${reply.username}:machine`);
    return { username: reply.username, client: new HubClient(reply.token, client.endpoint) };
  } catch (error) {
    if (error instanceof HubError && error.status === 409 && error.details?.username_required === true) return;
    throw error;
  }
}

export function approveAccountDevice(client: HubClient, user_code: string): Promise<void> {
  return client.request(`${ACCOUNT}/device/approve`, "POST", { user_code });
}

export async function loadAccountMachines(client: HubClient, username: string): Promise<ResolvedMachine[]> {
  const { machines } = await client.request<{ machines: ResolvedMachine[] }>(`${ACCOUNT}/machines`);
  if (!Array.isArray(machines)) throw new Error("Hub returned an invalid machine list.");
  return machines.map((machine) => {
    const address = machine?.machine_address;
    if (parseMachineAddress(address).username !== username) throw new Error("Hub returned a machine from another account.");
    return validateResolvedMachine(machine, address);
  });
}

export async function loadAccountKeys(client: HubClient): Promise<AccountKey[]> {
  const { keys } = await client.request<{ keys: AccountKey[] }>(`${ACCOUNT}/keys`);
  if (!Array.isArray(keys) || !keys.every((key) =>
    typeof key?.key_id === "string" && typeof key.name === "string" && Number.isSafeInteger(key.expires_at)
    && Array.isArray(key.scopes) && key.scopes.every((scope) => scope === MACHINE_READ || scope === MACHINE_REGISTER))) {
    throw new Error("Hub returned an invalid API key list.");
  }
  return keys;
}

export function createAccountKey(client: HubClient, name: string, scopes: AccountKeyScope[]): Promise<AccountKey & { token: string }> {
  return client.request(`${ACCOUNT}/keys`, "POST", { name, scopes });
}

export function revokeAccountKey(client: HubClient, key_id: string): Promise<void> {
  return client.request(`${ACCOUNT}/keys/${encodeURIComponent(key_id)}`, "DELETE");
}

export function logoutAccount(client: HubClient): Promise<void> {
  return client.request(`${ACCOUNT}/logout`, "POST", {});
}
