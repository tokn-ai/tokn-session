import { useState } from "react";
import { isHosting, useLocalHost } from "../lib/localHost";

/** Service management is deliberately restricted to the floating dialog. */
export function BackgroundHostingControls({ on_action }: { on_action: () => void }) {
  const host = useLocalHost()!;
  const [confirm_removal, setConfirmRemoval] = useState(false);
  const services = host.status?.background_services;
  const hosting = isHosting(host.status);
  const external = host.status?.phase === "external";
  const background = host.status?.phase === "background";
  if (!external && !background && !services?.api.registered && !services?.connector.registered) return null;
  return <section className="host-background" aria-label="Background hosting services">
    <h3>Background hosting</h3>
    {services && <dl className="host-summary">
      {(["connector", "api"] as const).map((kind) => {
        const service = services[kind];
        return <div key={kind}><dt>{kind === "api" ? "Session API" : "Hub connector"}</dt><dd>
          {service.running ? "Running" : service.loaded ? "Loaded; waiting" : service.registered ? "Stopped; registered" : "Not installed"}
          {service.error && <p role="alert">{service.error}</p>}
        </dd></div>;
      })}
    </dl>}
    {(external || background) && <button type="button" disabled={host.busy || !host.status?.external_stop_supported} onClick={() => { on_action(); void host.stop_external(); }}>{host.busy ? "Updating background hosting…" : "Stop external hosting"}</button>}
    {host.status?.background_services && <button type="button" disabled={host.busy || hosting || !host.status.external_remove_supported} onClick={() => setConfirmRemoval(true)}>Remove background hosting</button>}
    {confirm_removal && <div>
      <p>Remove both service registrations so background hosting will no longer start at login? Machine identity and paired devices will be kept.</p>
      <button type="button" disabled={host.busy || hosting || !host.status?.external_remove_supported} onClick={() => { setConfirmRemoval(false); on_action(); void host.remove_external(); }}>Remove registrations</button>
      <button type="button" disabled={host.busy} onClick={() => setConfirmRemoval(false)}>Cancel removal</button>
    </div>}
    <p className="host-hint">{host.status?.external_stop_supported
      ? "Stops the Hub connector, then the session API, for this login session. Remove background hosting also removes their login startup registrations. Saved identity and paired devices are preserved."
      : external || background ? "These background services cannot be controlled safely by the app. Manage them outside the app before using app hosting."
      : "Background services are stopped. Remove their registrations to prevent startup at the next login."}</p>
  </section>;
}
