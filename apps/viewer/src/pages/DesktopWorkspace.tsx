import { useEffect, useRef, useState } from "react";
import { HubAccess } from "../components/HubAccess";
import { hubErrorMessage } from "../lib/hub";
import { preferredHub, readMachinePreferences, rememberHub, rememberMachine } from "../lib/machinePreferences";
import { initializeLocalViewer } from "../lib/tauri";
import { selectMachine } from "../lib/transport";
import type { HubMachineSelection, MachinePreferences } from "../lib/types";
import { ViewerPage } from "./ViewerPage";

type WorkspaceView = { kind: "machines"; hub_url: string; startup_host_id: string | null }
  | { kind: "opening_local" } | { kind: "local" };

export function DesktopWorkspace() {
  const [preferences, setPreferences] = useState(readMachinePreferences);
  const preferences_ref = useRef(preferences);
  const [view, setView] = useState<WorkspaceView>(() => preferences.selected_machine?.kind === "local"
    ? { kind: "opening_local" }
    : { kind: "machines", hub_url: preferredHub(preferences), startup_host_id: preferences.selected_machine?.kind === "hub" ? preferences.selected_machine.host_id : null });
  const [local_error, setLocalError] = useState<string>();

  function updatePreferences(next: MachinePreferences) {
    preferences_ref.current = next;
    setPreferences(next);
  }
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
    updatePreferences(rememberMachine(preferences_ref.current, selection));
  }

  useEffect(() => {
    if (view.kind !== "opening_local") return;
    let canceled = false;
    void initializeLocalViewer().then(() => {
      if (canceled) return;
      updatePreferences(rememberMachine(preferences_ref.current, { kind: "local" }));
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
    on_machine_open={rememberRemote} on_hub_ready={(hub_url) => updatePreferences(rememberHub(preferences_ref.current, hub_url))} />;
}
