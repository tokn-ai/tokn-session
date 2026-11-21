import { useCallback, useEffect, useRef, useState } from "react";
import { HubError, hubErrorMessage, type HubClient } from "./hub";
import {
  accountSession, browserAccountClient, hubAccountAvailability, loadAccountMachines, logoutAccount,
  refreshBrowserAccount, type AccountSession, type HubAccountAvailability,
} from "./hubAccount";
import type { ResolvedMachine } from "./types";
import { useAccountKeys } from "./useAccountKeys";

const ROSTER_FRESH_MS = 30_000;
const ACCOUNT_CHANGED = "Your Hub account changed in another tab. Review the active account before continuing.";
const SESSION_EXPIRED = "Your Hub session expired. Sign in again.";

interface Roster {
  machines: ResolvedMachine[];
  unavailable?: boolean;
}

export function useBrowserHubAccount(load_roster: boolean, showing_profile: boolean, showing_gallery: boolean) {
  const [session, setSession] = useState<AccountSession>();
  const [availability, setAvailability] = useState<HubAccountAvailability>();
  const [check_error, setCheckError] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const [signing_out, setSigningOut] = useState(false);
  const [sign_out_error, setSignOutError] = useState<string>();
  const [roster, setRoster] = useState<Roster>({ machines: [] });
  const [boundary, setBoundary] = useState(0);
  const [attempt, setAttempt] = useState(0);
  const [roster_revision, setRosterRevision] = useState(0);
  const roster_checked_at = useRef<number>(undefined);
  const active = useRef<AccountSession>(undefined);
  const generation = useRef(0);
  const logout_client = useRef<HubClient>(undefined);
  const retry = useCallback(() => setAttempt((value) => value + 1), []);

  const replace = useCallback((next?: AccountSession) => {
    generation.current++;
    if (active.current) {
      active.current.client.close();
      setBoundary((value) => value + 1);
    }
    roster_checked_at.current = undefined;
    active.current = next;
    setSession(next);
    setRoster({ machines: [] });
    setSignOutError(undefined);
  }, []);

  const handleSessionError = useCallback((error: unknown) => {
    if (!(error instanceof HubError)) return false;
    if (error.status === 409 && error.details?.account_session_changed === true) {
      replace();
      setNotice(ACCOUNT_CHANGED);
      setAvailability(undefined);
      retry();
      return true;
    }
    if (error.status === 401) {
      replace();
      setNotice(SESSION_EXPIRED);
      return true;
    }
    return false;
  }, [replace, retry]);

  useEffect(() => {
    const client = browserAccountClient();
    let available: HubAccountAvailability | undefined;
    let refreshing = false;

    async function refresh() {
      if (refreshing || client.signal.aborted || logout_client.current) return;
      refreshing = true;
      const revision = generation.current;
      const stale = () => client.signal.aborted || revision !== generation.current;
      try {
        available ??= await hubAccountAvailability(client);
        if (stale()) return;
        if (available !== "unsupported") {
          const identity = await refreshBrowserAccount(client);
          if (stale()) return;
          const previous = active.current;
          if (identity && identity.user_id !== previous?.user_id) {
            replace(accountSession(identity));
            if (previous) setNotice(ACCOUNT_CHANGED);
            else setNotice((message) => message === ACCOUNT_CHANGED ? message : undefined);
          } else if (!identity && previous) {
            replace();
            setNotice(SESSION_EXPIRED);
          } else if (identity) {
            setRosterRevision((value) => value + 1);
          } else {
            setRoster({ machines: [] });
          }
        } else if (active.current) {
          replace();
        }
        setCheckError(undefined);
        setAvailability(available);
      } catch (error) {
        if (stale()) return;
        if (available) setAvailability(available);
        setCheckError(hubErrorMessage(error));
        setRoster((previous) => ({ ...previous, unavailable: true }));
      } finally {
        refreshing = false;
      }
    }
    const hide = () => {
      client.close();
      logout_client.current?.close();
      replace();
    };
    const restore = (event: PageTransitionEvent) => { if (event.persisted) window.location.reload(); };
    window.addEventListener("focus", refresh);
    window.addEventListener("pagehide", hide);
    window.addEventListener("pageshow", restore);
    void refresh();
    return () => {
      window.removeEventListener("focus", refresh);
      window.removeEventListener("pagehide", hide);
      window.removeEventListener("pageshow", restore);
      client.close();
    };
  }, [attempt, replace]);

  useEffect(() => () => {
    generation.current++;
    active.current?.client.close();
    active.current = undefined;
    logout_client.current?.close();
  }, []);

  useEffect(() => {
    const checked = roster_checked_at.current;
    if (showing_gallery && checked !== undefined && Date.now() - checked >= ROSTER_FRESH_MS) {
      setRosterRevision((value) => value + 1);
    }
  }, [showing_gallery]);

  useEffect(() => {
    if (!load_roster || !session || signing_out) return;
    roster_checked_at.current = undefined;
    const revision = generation.current;
    let current = true;
    const stale = () => !current || session.client.signal.aborted || revision !== generation.current;
    void loadAccountMachines(session.client, session.username).then((machines) => {
      if (stale()) return;
      roster_checked_at.current = Date.now();
      setRoster({ machines });
    }).catch((error: unknown) => {
      if (stale() || handleSessionError(error)) return;
      roster_checked_at.current = Date.now();
      setRoster((previous) => ({ ...previous, unavailable: true }));
    });
    return () => { current = false; };
  }, [load_roster, session, signing_out, roster_revision, handleSessionError]);

  async function logout() {
    if (!session || active.current !== session || logout_client.current) return;
    const client = session.client;
    logout_client.current = client;
    generation.current++;
    setSigningOut(true);
    setSignOutError(undefined);
    try {
      await logoutAccount(client);
      if (!client.signal.aborted) { replace(); setNotice(undefined); }
    } catch (error) {
      if (!client.signal.aborted && !handleSessionError(error)) setSignOutError(hubErrorMessage(error));
    } finally {
      logout_client.current = undefined;
      setSigningOut(false);
    }
  }

  const signedIn = useCallback((next: AccountSession) => { replace(next); setNotice(undefined); }, [replace]);

  const onFailure = useCallback((error: unknown, report: (message: string) => void) => {
    if (session && session === active.current && !session.client.signal.aborted && !handleSessionError(error)) {
      report(hubErrorMessage(error));
    }
  }, [session, handleSessionError]);

  const key_list = useAccountKeys(session?.client, showing_profile, onFailure);
  return {
    session, key_list, supported: availability !== undefined && availability !== "unsupported", availability, check_error, notice,
    signing_out, sign_out_error, logout, boundary, roster, retry, signedIn, onFailure,
  };
}

export type BrowserHubAccount = ReturnType<typeof useBrowserHubAccount>;
