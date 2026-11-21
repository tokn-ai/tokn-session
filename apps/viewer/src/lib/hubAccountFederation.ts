import type { HubClient } from "./hub";
import { accountSession, secureAccountUrl, validApellIssuer, validateAccountIdentity, type AccountDevicePurpose, type AccountSession } from "./hubAccount";
import { parseMachineAddress } from "./hubAddress";

export type AccountProviderId = "apell" | "github";
export type AccountProvider = { id: "apell"; issuer: string } | { id: "github" };

export const ACCOUNT_PROVIDER_LABELS: Record<AccountProviderId, string> = { apell: "Apell Account", github: "GitHub" };

export interface AccountDeviceRequest {
  user_code: string;
  purpose: AccountDevicePurpose;
}

export interface ApellLinkConfirmation {
  username: string;
  confirmation: string;
}

type FederatedReturn = { status: "signed_in"; session: AccountSession } | { status: "linked" }
  | { status: "signup_required"; suggested_username: string } | { status: "link_required"; apell_link?: ApellLinkConfirmation };

function accountPath(provider: AccountProviderId) {
  return `account/browser/${provider}`;
}

export async function loadProviderConnection(client: HubClient, provider: AccountProviderId): Promise<{ login: string | null } | null> {
  const reply = await client.request<{ connection: Record<string, unknown> | null }>(accountPath(provider));
  if (reply?.connection === null) return null;
  const login = reply?.connection?.[`${provider}_login`];
  if (provider === "apell" && login === null) return { login };
  if (typeof login !== "string" || !login.trim() || login.length > (provider === "apell" ? 128 : 100)) {
    throw new Error(`Hub returned an invalid ${ACCOUNT_PROVIDER_LABELS[provider]} connection.`);
  }
  return { login };
}

export async function startFederatedAccount(client: HubClient, provider: AccountProvider, mode: "link" | "sign_in", device_request?: AccountDeviceRequest): Promise<string> {
  const reply = await client.request<{ authorization_url?: unknown }>(`${accountPath(provider.id)}/start`, "POST", {
    mode, ...(device_request && { user_code: device_request.user_code, device: device_request.purpose }),
  });
  const url = secureAccountUrl(reply?.authorization_url);
  if (url) {
    if (provider.id === "github") {
      if (url.origin === "https://github.com" && url.pathname === "/login/oauth/authorize") return url.href;
    } else if (validApellIssuer(provider.issuer)) {
      const issuer = new URL(provider.issuer);
      const base_path = `${issuer.pathname}/`;
      if (url.origin === issuer.origin && url.pathname.startsWith(base_path) && url.pathname.length > base_path.length
        && !url.pathname.slice(base_path.length).startsWith("/") && !/%(?:2f|5c)/i.test(url.pathname)) return url.href;
    }
  }
  throw new Error(`Hub returned an invalid ${ACCOUNT_PROVIDER_LABELS[provider.id]} sign-in address.`);
}

export async function completeFederatedAccount(client: HubClient, provider: AccountProviderId): Promise<FederatedReturn> {
  const reply = await client.request<{ status?: unknown; suggested_username?: unknown; username?: unknown; confirmation?: unknown }>(
    `${accountPath(provider)}/complete`, "POST", {},
  );
  switch (reply?.status) {
    case "signed_in": return { status: reply.status, session: accountSession(validateAccountIdentity(reply)) };
    case "linked": return { status: reply.status };
    case "link_required": {
      const { username, confirmation } = reply;
      if (provider === "github" || (username === undefined && confirmation === undefined)) return { status: reply.status };
      if (typeof username === "string" && typeof confirmation === "string" && /^[A-Za-z0-9_-]{43}$/.test(confirmation)) {
        parseMachineAddress(`${username}:machine`);
        return { status: reply.status, apell_link: { username, confirmation } };
      }
      break;
    }
    case "signup_required":
      if (typeof reply.suggested_username === "string" && reply.suggested_username.length <= 63) {
        return { status: reply.status, suggested_username: reply.suggested_username };
      }
  }
  throw new Error(`Hub returned an invalid ${ACCOUNT_PROVIDER_LABELS[provider]} sign-in response.`);
}

export async function confirmApellAccountLink(client: HubClient, confirmation: string): Promise<AccountSession> {
  const reply = await client.request<{ status?: unknown }>(`${accountPath("apell")}/confirm-link`, "POST", { confirmation });
  if (reply?.status !== "signed_in") throw new Error("Hub returned an invalid Apell Account sign-in response.");
  return accountSession(validateAccountIdentity(reply));
}

export async function signUpWithProvider(client: HubClient, provider: AccountProviderId, username: string): Promise<AccountSession> {
  const reply = await client.request<unknown>(`${accountPath(provider)}/signup`, "POST", { username });
  return accountSession(validateAccountIdentity(reply));
}
