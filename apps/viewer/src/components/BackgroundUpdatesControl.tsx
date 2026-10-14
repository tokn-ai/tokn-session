import { useCallback, useId, useRef, useState } from "react";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import type { BackgroundUpdatesScope } from "../lib/types";
import { ChevronIcon, CloseIcon, WarningIcon } from "./Icons";

interface BackgroundUpdatesControlProps {
  scope: BackgroundUpdatesScope;
  status: string | null;
  on_scope_change: (scope: BackgroundUpdatesScope) => void;
}

const SCOPES: { scope: BackgroundUpdatesScope; label: string; description: string }[] = [
  { scope: "all", label: "All sessions", description: "Receive updates from every session." },
  { scope: "sidebar", label: "Sidebar sessions", description: "Receive updates for sessions loaded in the sidebar." },
];

/** Scope and connection details share one small footer affordance. */
export function BackgroundUpdatesControl({ scope, status, on_scope_change }: BackgroundUpdatesControlProps) {
  const [is_open, setOpen] = useState(false);
  const panel_id = useId();
  const trigger_ref = useRef<HTMLButtonElement>(null);
  const close = useCallback(() => setOpen(false), []);
  const panel_ref = useFloatingPanel(is_open, close, trigger_ref);
  const scopeLabel = scope === "all" ? "All sessions" : "Sidebar sessions";
  const isConnecting = status?.startsWith("Connecting") === true;
  const phaseLabel = isConnecting ? "Connecting" : status ? "Unavailable" : "Live";

  return (
    <div className="background-updates">
      <button
        aria-controls={panel_id}
        aria-expanded={is_open}
        aria-haspopup="dialog"
        aria-label={`Background updates. ${scopeLabel}. ${phaseLabel}. Change scope`}
        className="status-bar__updates"
        data-tone={isConnecting ? "active" : status ? "warning" : "neutral"}
        onClick={() => setOpen((open) => !open)}
        ref={trigger_ref}
        title={`Background updates · ${scopeLabel} · ${phaseLabel}`}
        type="button"
      >
        {isConnecting ? <span aria-hidden="true" className="status-bar__spinner" />
          : status ? <WarningIcon aria-hidden="true" /> : null}
        <span>Updates · {scope === "all" ? "All" : "Sidebar"}</span>
        <ChevronIcon aria-hidden="true" />
      </button>

      {is_open ? <div
        aria-labelledby={`${panel_id}-title`}
        aria-modal="false"
        className="background-updates-panel"
        id={panel_id}
        ref={panel_ref}
        role="dialog"
        tabIndex={-1}
      >
        <header className="background-updates-panel__header">
          <h2 id={`${panel_id}-title`}>Background updates</h2>
          <button aria-label="Close background updates" className="icon-button" onClick={close} type="button"><CloseIcon /></button>
        </header>
        <fieldset className="background-updates-panel__options">
          <legend className="sr-only">Background update scope</legend>
          {SCOPES.map((choice) => <label className="background-updates-panel__option" data-selected={scope === choice.scope} key={choice.scope}>
            <input
              aria-describedby={`${panel_id}-${choice.scope}-description`}
              aria-labelledby={`${panel_id}-${choice.scope}-label`}
              checked={scope === choice.scope}
              name={`${panel_id}-scope`}
              onChange={() => on_scope_change(choice.scope)}
              type="radio"
              value={choice.scope}
            />
            <span>
              <strong className="background-updates-panel__label" id={`${panel_id}-${choice.scope}-label`}>{choice.label}</strong>
              <span className="background-updates-panel__description" id={`${panel_id}-${choice.scope}-description`}>{choice.description}</span>
            </span>
          </label>)}
        </fieldset>
        <p aria-live="polite" className="background-updates-panel__status" data-tone={status && !isConnecting ? "warning" : "neutral"}>
          {status ?? "Receiving live background updates."}
        </p>
      </div> : null}
    </div>
  );
}
