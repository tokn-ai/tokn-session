import { useState } from "react";
import { HubAccess } from "../components/HubAccess";
import { HubAccount } from "../components/HubAccount";
import { HubAccountMenu } from "../components/HubAccountMenu";
import { HubConnection } from "../components/HubConnection";
import { HubNavigation, HubPageLink, useHubNavigation } from "../components/HubNavigation";
import { SettingsEntry } from "../components/SettingsEntry";
import type { HubStatus } from "../lib/hub";
import { useBrowserHubAccount, type BrowserHubAccount } from "../lib/useBrowserHubAccount";

// A direct link to Profile or administration doesn't need a machine service.
// Once opened, the workspace owns the connection for the document's lifetime.
function useVisited(visible: boolean) {
  const [visited, setVisited] = useState(visible);
  if (visible && !visited) setVisited(true);
  return visited || visible;
}

export function BrowserHub({ status, bootstrap_token }: { status: HubStatus; bootstrap_token?: string }) {
  const route = useHubNavigation();
  const admin = route.page === "admin";
  const [configured, setConfigured] = useState(status.configured);
  const visited_workspace = useVisited(!admin);
  const [setup_token, setSetupToken] = useState(admin ? bootstrap_token : undefined);
  if (!admin && setup_token) setSetupToken(undefined);
  return <HubNavigation.Provider value={route.navigate}>
    {visited_workspace && <BrowserWorkspace route={route} />}
    {admin && <HubConnection initial_status={{ ...status, configured }} bootstrap_token={setup_token}
      on_configured={() => { setConfigured(true); setSetupToken(undefined); }} />}
  </HubNavigation.Provider>;
}

function BrowserWorkspace({ route }: { route: ReturnType<typeof useHubNavigation> }) {
  const gallery = route.page === "machines";
  const profile = route.page === "profile";
  const visited_gallery = useVisited(gallery);
  const account = useBrowserHubAccount(visited_gallery, profile, gallery && !route.host_id);
  return <>
    <MachineWorkspace key={account.boundary} route={route} account={account} />
    {profile && <HubAccount key={route.search} account={account} />}
  </>;
}

// After sign-out on Profile, dispose the old machine immediately and wait
// until the user returns to Machines to open a new device service.
function MachineWorkspace({ route, account }: { route: ReturnType<typeof useHubNavigation>; account: BrowserHubAccount }) {
  const visible = route.page === "machines";
  const visited = useVisited(visible);
  if (!visited) return null;
  const { session } = account;
  return <HubAccess visible={visible} browser_view={{ host_id: route.host_id, on_change: route.showMachine }}
    account_control={account.supported && (session
      ? <HubAccountMenu key={session.user_id} username={session.username} signing_out={account.signing_out}
        sign_out_error={account.sign_out_error} on_sign_out={account.logout} />
      : <><SettingsEntry /><HubPageLink className="hub-sign-in" href="/account">Sign in</HubPageLink></>)}
    account_roster={{ hub_url: window.location.origin, ...account.roster, on_retry: account.retry }} />;
}
