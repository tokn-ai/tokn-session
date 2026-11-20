import { useEffect, useRef, useState } from "react";
import { LocalHostProvider } from "../components/LocalHostProvider";
import { NativeDevicePreferencesProvider } from "../components/NativeDevicePreferencesProvider";
import { HubAccess } from "../components/HubAccess";
import { NativeHubAccountEntry } from "../components/NativeHubAccountEntry";
import { hubErrorMessage } from "../lib/hub";
import { preferredHub, rememberHub, rememberMachine, useMachinePreferences } from "../lib/machinePreferences";
import { initializeLocalViewer } from "../lib/tauri";
import { selectMachine } from "../lib/transport";
import type { HubMachineSelection } from "../lib/types";
import { useNativeHubAccount } from "../lib/useNativeHubAccount";
import { ViewerPage } from "./ViewerPage";
import { SessionDraftsProvider } from "../components/SessionDraftsProvider";

type WorkspaceView = { kind: "machines"; hub_url: string; startup_host_id: string | null }
  | { kind: "opening_local" } | { kind: "local" };

export function DesktopWorkspace() {
  const [drafts_enabled, setDraftsEnabled] = useState(false);
  return <NativeDevicePreferencesProvider><LocalHostProvider>
    <SessionDraftsProvider enabled={drafts_enabled}>
      <DesktopWorkspaceContent on_local_ready={() => setDraftsEnabled(true)} />
    </SessionDraftsProvider>
  </LocalHostProvider></NativeDevicePreferencesProvider>;
}

function DesktopWorkspaceContent({ on_local_ready }: { on_local_ready: () => void }) {
  const preferences = useMachinePreferences();
  const preferences_ref = useRef(preferences);
  preferences_ref.current = preferences;
  const [view, setView] = useState<WorkspaceView>(() => preferences.selected_machine?.kind === "local"
    ? { kind: "opening_local" }
    : { kind: "machines", hub_url: preferredHub(preferences), startup_host_id: preferences.selected_machine?.kind === "hub" ? preferences.selected_machine.host_id : null });
  const [local_error, setLocalError] = useState<string>();
  const [hub_url, setHubUrl] = useState<string>();
  const account = useNativeHubAccount(view.kind === "machines" ? hub_url : undefined);

  function showMachines() {
    selectMachine();
    setHubUrl(undefined);
    setView({ kind: "machines", hub_url: preferredHub(preferences_ref.current), startup_host_id: null });
  }
  function openLocal() {
    selectMachine();
    setLocalError(undefined);
    setView({ kind: "opening_local" });
  }
  function rememberRemote(selection: HubMachineSelection) {
    setLocalError(undefined);
    rememberMachine(preferences_ref.current, selection);
  }

  useEffect(() => {
    if (view.kind !== "opening_local") return;
    let canceled = false;
    void initializeLocalViewer().then(() => {
      if (canceled) return;
      on_local_ready();
      rememberMachine(preferences_ref.current, { kind: "local" });
      setView({ kind: "local" });
    }).catch((error: unknown) => {
      if (canceled) return;
      setLocalError(hubErrorMessage(error));
      showMachines();
    });
    return () => { canceled = true; };
  }, [view.kind]);

  if (view.kind === "local") return <ViewerPage key="local" on_open_machines={showMachines} />;
  if (view.kind === "opening_local") return <main className="hub-home hub-access"><div className="hub-content">
    <header className="hub-header"><div><p className="hub-eyebrow">Tokn</p><h1>This machine</h1><p role="status">Opening this machine…</p></div></header>
    <p className="hub-muted">Preparing local sessions and live updates.</p>
    <button onClick={showMachines}>Cancel</button>
  </div></main>;
  return <HubAccess initial_hub_url={view.hub_url} startup_host_id={view.startup_host_id}
    known_hub_urls={preferences.hub_urls} on_local={openLocal} local_error={local_error}
    on_machine_open={rememberRemote} on_hub_ready={(url) => { setHubUrl(url); rememberHub(preferences_ref.current, url); }}
    on_hub_closed={() => setHubUrl(undefined)}
    account_control={account.supported && <NativeHubAccountEntry key={hub_url}
      session={account.session} signing_out={account.signing_out} sign_out_error={account.sign_out_error} on_sign_out={account.logout}
      challenge={account.challenge} starting={account.starting} sign_in_error={account.sign_in_error}
      on_start_sign_in={account.startSignIn} on_cancel_sign_in={account.cancelSignIn} on_open_approval={account.openApproval} />}
    account_roster={hub_url ? { hub_url, machines: account.machines, unavailable: account.unavailable,
      can_sign_in: account.supported, on_retry: account.retry } : undefined} />;
}
