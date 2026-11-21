import { useEffect, useMemo, useState } from "react";
import { hubErrorMessage } from "./hub";
import { validateAccountIdentity, validateAccountMachines, type AccountIdentity } from "./hubAccount";
import { createUuid } from "./id";
import { invokeHost } from "./platform";
import type { ResolvedMachine } from "./types";

interface LoginChallenge {
  verification_url: string;
  user_code: string;
  expires_at: number;
  interval_ms: number;
}

interface AccountState {
  supported?: boolean;
  session?: AccountIdentity;
  machines: ResolvedMachine[];
  unavailable?: boolean;
  challenge?: LoginChallenge;
  starting?: boolean;
  sign_in_error?: string;
  signing_out?: boolean;
  sign_out_error?: string;
}

interface LoginAttempt {
  attempt_id: string;
  timer?: ReturnType<typeof setTimeout>;
}

interface HubAccountScope {
  hub_url: string;
  active: boolean;
  read?: object;
  login?: LoginAttempt;
  signing_out?: boolean;
}

const EMPTY_ACCOUNT: AccountState = { machines: [] };

function pollInterval(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 1_000 || value > 60_000) {
    throw new Error("Hub returned invalid sign-in settings.");
  }
  return value;
}

function validateChallenge(challenge: LoginChallenge, hub_url: string): LoginChallenge {
  const { user_code, expires_at, interval_ms } = challenge;
  const url = new URL(challenge.verification_url);
  if (url.origin !== hub_url || url.pathname !== "/account" || url.hash || url.username || url.password
    || !/^[A-Z2-9]{4}-[A-Z2-9]{4}$/.test(user_code)
    || url.searchParams.get("user_code") !== user_code || url.searchParams.get("device") !== "viewer"
    || [...url.searchParams].length !== 2 || !Number.isSafeInteger(expires_at)
    || expires_at * 1000 <= Date.now()) {
    throw new Error("Hub returned an invalid sign-in request.");
  }
  return { user_code, expires_at, verification_url: url.href, interval_ms: pollInterval(interval_ms) };
}

function revokeAttempt(scope: HubAccountScope) {
  const attempt = scope.login;
  if (!attempt) return;
  scope.login = undefined;
  clearTimeout(attempt.timer);
  void invokeHost("hub_account_login_cancel", { hub_url: scope.hub_url, attempt_id: attempt.attempt_id }).catch(() => {});
}

/** The native host owns approval and credentials. React only holds public account data for the connected Hub. */
export function useNativeHubAccount(hub_url?: string) {
  const scope = useMemo<HubAccountScope | undefined>(() => hub_url ? { hub_url, active: false } : undefined, [hub_url]);
  const [snapshot, setSnapshot] = useState<{ scope: HubAccountScope; state: AccountState }>();
  const state = snapshot?.scope === scope ? snapshot?.state ?? EMPTY_ACCOUNT : EMPTY_ACCOUNT;

  function publish(owner: HubAccountScope, update: (previous: AccountState) => AccountState) {
    if (owner.active) setSnapshot((previous) => owner.active
      ? { scope: owner, state: update(previous?.scope === owner ? previous.state : EMPTY_ACCOUNT) } : previous);
  }

  async function refresh(owner: HubAccountScope) {
    if (!owner.active || owner.read || owner.login || owner.signing_out) return;
    const read = {};
    owner.read = read;
    const current = () => owner.active && owner.read === read;
    const params = { hub_url: owner.hub_url };
    try {
      const status = await invokeHost<{ supported: boolean; account?: unknown }>("hub_account_status", params);
      if (!current()) return;
      if (typeof status?.supported !== "boolean") throw new Error("Hub returned invalid account settings.");
      const identity = status.supported && status.account != null ? validateAccountIdentity(status.account) : undefined;
      publish(owner, (previous) => ({
        ...previous, supported: status.supported, session: identity, unavailable: undefined,
        machines: identity && identity.user_id === previous.session?.user_id ? previous.machines : [],
      }));
      if (!identity) return;
      const result = await invokeHost<{ account?: unknown; machines: unknown }>("hub_account_machines", params);
      if (!current()) return;
      const session = result.account != null ? validateAccountIdentity(result.account) : undefined;
      const machines = session ? validateAccountMachines(result.machines, session.username) : [];
      publish(owner, (previous) => ({ ...previous, session, machines, unavailable: undefined }));
    } catch {
      if (current()) publish(owner, (previous) => ({ ...previous, unavailable: true }));
    } finally {
      if (owner.read === read) owner.read = undefined;
    }
  }

  useEffect(() => {
    if (!scope) return;
    scope.active = true;
    const sync = () => { if (document.visibilityState !== "hidden") void refresh(scope); };
    window.addEventListener("focus", sync);
    document.addEventListener("visibilitychange", sync);
    void refresh(scope);
    return () => {
      scope.active = false;
      scope.read = undefined;
      revokeAttempt(scope);
      window.removeEventListener("focus", sync);
      document.removeEventListener("visibilitychange", sync);
    };
  }, [scope]);

  async function startSignIn() {
    if (!scope?.active || !state.supported || state.session || scope.login || scope.signing_out) return;
    const owner = scope;
    const attempt: LoginAttempt = { attempt_id: createUuid() };
    owner.read = undefined;
    owner.login = attempt;
    const params = { hub_url: owner.hub_url, attempt_id: attempt.attempt_id };
    const current = () => owner.active && owner.login === attempt;
    const fail = (error: unknown) => {
      if (!current()) return;
      revokeAttempt(owner);
      publish(owner, (previous) => ({ ...previous, starting: false, challenge: undefined, sign_in_error: hubErrorMessage(error) }));
    };
    publish(owner, (previous) => ({ ...previous, starting: true, challenge: undefined, sign_in_error: undefined }));
    try {
      const challenge = validateChallenge(await invokeHost<LoginChallenge>("hub_account_login_start", params), owner.hub_url);
      if (!current()) return;
      publish(owner, (previous) => ({ ...previous, starting: false, challenge }));

      async function poll() {
        if (!current()) return;
        if (challenge.expires_at * 1000 <= Date.now()) {
          fail(new Error("The sign-in request expired. Start again."));
          return;
        }
        try {
          const result = await invokeHost<{ account?: unknown; interval_ms: number }>("hub_account_login_poll", params);
          if (!current()) return;
          if (result.account == null) {
            attempt.timer = setTimeout(() => { void poll(); }, pollInterval(result.interval_ms));
            return;
          }
          const session = validateAccountIdentity(result.account);
          owner.login = undefined;
          publish(owner, (previous) => ({ ...previous, session, challenge: undefined, machines: [], unavailable: undefined }));
          void refresh(owner);
        } catch (error) { fail(error); }
      }
      attempt.timer = setTimeout(() => { void poll(); }, challenge.interval_ms);
    } catch (error) { fail(error); }
  }

  function cancelSignIn() {
    if (!scope?.active || !scope.login) return;
    revokeAttempt(scope);
    publish(scope, (previous) => ({ ...previous, starting: false, challenge: undefined, sign_in_error: undefined }));
  }

  async function openApproval() {
    if (!scope?.active || !scope.login || !state.challenge) return;
    await invokeHost("hub_account_login_open", { hub_url: scope.hub_url, attempt_id: scope.login.attempt_id });
  }

  async function logout() {
    if (!scope?.active || !state.session || scope.signing_out) return;
    scope.read = undefined;
    scope.signing_out = true;
    publish(scope, (previous) => ({ ...previous, signing_out: true, sign_out_error: undefined }));
    try {
      await invokeHost("hub_account_logout", { hub_url: scope.hub_url });
      publish(scope, (previous) => ({ ...previous, session: undefined, machines: [], unavailable: undefined }));
    } catch (error) {
      publish(scope, (previous) => ({ ...previous, sign_out_error: hubErrorMessage(error) }));
    } finally {
      scope.signing_out = false;
      publish(scope, (previous) => ({ ...previous, signing_out: false }));
    }
  }

  return { ...state, startSignIn, cancelSignIn, openApproval, logout, retry: () => { if (scope) void refresh(scope); } };
}
