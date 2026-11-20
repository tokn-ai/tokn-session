import type { HubClient } from "./hub";
import { accountSession, validateAccountIdentity, type AccountDevicePurpose, type AccountSession } from "./hubAccount";

const GITHUB_ACCOUNT = "account/browser/github";

export interface AccountDeviceRequest {
  user_code: string;
  purpose: AccountDevicePurpose;
}

type GitHubReturn = { status: "signed_in"; session: AccountSession } | { status: "linked" }
  | { status: "signup_required"; suggested_username: string } | { status: "link_required" };

export async function loadGitHubConnection(client: HubClient): Promise<string | null> {
  const reply = await client.request<{ connection: { github_login?: unknown } | null }>(GITHUB_ACCOUNT);
  if (reply?.connection === null) return null;
  const login = reply?.connection?.github_login;
  if (typeof login !== "string" || !login.trim() || login.length > 100) throw new Error("Hub returned an invalid GitHub connection.");
  return login;
}

export async function startGitHubAccount(client: HubClient, mode: "link" | "sign_in", device_request?: AccountDeviceRequest): Promise<string> {
  const reply = await client.request<{ authorization_url?: unknown }>(`${GITHUB_ACCOUNT}/start`, "POST", {
    mode, ...(device_request && { user_code: device_request.user_code, device: device_request.purpose }),
  });
  let url: URL | undefined;
  if (typeof reply?.authorization_url === "string") {
    try { url = new URL(reply.authorization_url); } catch { /* invalid response */ }
  }
  if (!url || url.origin !== "https://github.com" || url.pathname !== "/login/oauth/authorize" || url.username || url.password || url.hash) {
    throw new Error("Hub returned an invalid GitHub sign-in address.");
  }
  return url.href;
}

export async function completeGitHubAccount(client: HubClient): Promise<GitHubReturn> {
  const reply = await client.request<{ status?: unknown; suggested_username?: unknown }>(`${GITHUB_ACCOUNT}/complete`, "POST", {});
  switch (reply?.status) {
    case "signed_in": return { status: reply.status, session: accountSession(validateAccountIdentity(reply)) };
    case "linked":
    case "link_required": return { status: reply.status };
    case "signup_required":
      if (typeof reply.suggested_username === "string" && reply.suggested_username.length <= 63) {
        return { status: reply.status, suggested_username: reply.suggested_username };
      }
  }
  throw new Error("Hub returned an invalid GitHub sign-in response.");
}

export async function signUpWithGitHub(client: HubClient, username: string): Promise<AccountSession> {
  const reply = await client.request<unknown>(`${GITHUB_ACCOUNT}/signup`, "POST", { username });
  return accountSession(validateAccountIdentity(reply));
}
