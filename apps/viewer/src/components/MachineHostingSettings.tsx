import { useLocalHost, hostLabels } from "../lib/localHost";
import { HostSetup } from "./HostSetup";

/** Only mounted for the selected local desktop machine. */
export function MachineHostingSettings() {
  const host = useLocalHost();
  if (!host) return <p>Hosting management is available in this computer’s desktop app.</p>;
  if (!host.status) return <div>
    <p role={host.error ? "alert" : "status"}>{host.error ?? "Loading hosting settings…"}</p>
    {host.error && <button type="button" onClick={host.retry}>Retry hosting status</button>}
  </div>;
  const { status } = host;
  return <div className="settings-hosting">
    <p className="settings-hosting__status" role="status">{hostLabels[status.phase]}</p>
    <dl className="settings-hosting__summary">
      <div><dt>Computer</dt><dd>{status.name || "This computer"}</dd></div>
      <div><dt>Hub</dt><dd>{status.hub_url || "Not configured"}</dd></div>
      <div><dt>Remote access</dt><dd>{status.allow_control ? "Viewing and agent input" : "Viewing only"}</dd></div>
    </dl>
    <p>Open hosting setup to change the Hub, computer name, or remote-input permission.</p>
    <HostSetup />
    {(host.error || status.error) && <p role="alert">{host.error ?? status.error}</p>}
  </div>;
}
