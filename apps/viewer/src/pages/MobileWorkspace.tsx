import { useRef } from "react";
import { HubAccess } from "../components/HubAccess";
import { NativeDevicePreferencesProvider } from "../components/NativeDevicePreferencesProvider";
import { preferredHub, rememberHub, rememberMachine, useMachinePreferences } from "../lib/machinePreferences";

export function MobileWorkspace() {
  return <NativeDevicePreferencesProvider><RemoteMachines /></NativeDevicePreferencesProvider>;
}

function RemoteMachines() {
  const preferences = useMachinePreferences();
  const current = useRef(preferences);
  current.current = preferences;
  const initial = useRef({
    hub_url: preferredHub(preferences),
    host_id: preferences.selected_machine?.kind === "hub" ? preferences.selected_machine.host_id : null,
  });
  return <HubAccess initial_hub_url={initial.current.hub_url} startup_host_id={initial.current.host_id}
    known_hub_urls={preferences.hub_urls}
    on_machine_open={(selection) => rememberMachine(current.current, selection)}
    on_hub_ready={(hub_url) => rememberHub(current.current, hub_url)} />;
}
