import { useEffect, useRef, useState } from "react";
import { LocalHostProvider } from "../components/LocalHostProvider";
import { NativeDevicePreferencesProvider } from "../components/NativeDevicePreferencesProvider";
import { HubAccess } from "../components/HubAccess";
import { hubErrorMessage } from "../lib/hub";
import { preferredHub, rememberHub, rememberMachine, useMachinePreferences } from "../lib/machinePreferences";
import { initializeLocalViewer } from "../lib/tauri";
import { selectMachine } from "../lib/transport";
import type { HubMachineSelection } from "../lib/types";
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

  function showMachines() {
    selectMachine();
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
    on_machine_open={rememberRemote} on_hub_ready={(hub_url) => { rememberHub(preferences_ref.current, hub_url); }} />;
}
