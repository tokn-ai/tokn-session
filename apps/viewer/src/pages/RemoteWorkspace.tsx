import { useRef, useState } from "react";
import { NativeHubAccountEntry } from "../components/NativeHubAccountEntry";
import { HubAccess } from "../components/HubAccess";
import { NativeDevicePreferencesProvider } from "../components/NativeDevicePreferencesProvider";
import { preferredHub, rememberHub, rememberMachine, useMachinePreferences } from "../lib/machinePreferences";
import { useNativeHubAccount } from "../lib/useNativeHubAccount";

export function RemoteWorkspace() {
  return <NativeDevicePreferencesProvider><RemoteMachines /></NativeDevicePreferencesProvider>;
}

function RemoteMachines() {
  const preferences = useMachinePreferences();
  const current = useRef(preferences);
  current.current = preferences;
  const [hub_url, setHubUrl] = useState<string>();
  const account = useNativeHubAccount(hub_url);
  const initial = useRef({
    hub_url: preferredHub(preferences),
    host_id: preferences.selected_machine?.kind === "hub" ? preferences.selected_machine.host_id : null,
  });
  return <HubAccess initial_hub_url={initial.current.hub_url} startup_host_id={initial.current.host_id}
    known_hub_urls={preferences.hub_urls}
    on_machine_open={(selection) => rememberMachine(current.current, selection)}
    on_hub_ready={(url) => { setHubUrl(url); rememberHub(current.current, url); }}
    on_hub_closed={() => setHubUrl(undefined)}
    account_control={account.supported && <NativeHubAccountEntry key={hub_url}
      session={account.session} signing_out={account.signing_out} sign_out_error={account.sign_out_error} on_sign_out={account.logout}
      challenge={account.challenge} starting={account.starting} sign_in_error={account.sign_in_error}
      on_start_sign_in={account.startSignIn} on_cancel_sign_in={account.cancelSignIn} />}
    account_roster={hub_url ? { hub_url, machines: account.machines, unavailable: account.unavailable,
      can_sign_in: account.supported, on_retry: account.retry } : undefined} />;
}
