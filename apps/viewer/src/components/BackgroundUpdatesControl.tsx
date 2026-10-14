import { useCallback, useId, useRef, useState } from "react";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import type { BackgroundUpdateLevel, BackgroundUpdatesScope, ConversationUpdateLevel } from "../lib/types";
import { ChevronIcon, CloseIcon, WarningIcon } from "./Icons";

interface BackgroundUpdatesControlProps {
  scope: BackgroundUpdatesScope;
  level: BackgroundUpdateLevel;
  conversation_level: ConversationUpdateLevel;
  conversation_available?: boolean;
  status: string | null;
  on_scope_change: (scope: BackgroundUpdatesScope) => void;
  on_level_change: (level: BackgroundUpdateLevel) => void;
  on_conversation_level_change: (level: ConversationUpdateLevel) => void;
}

const SCOPES: { scope: BackgroundUpdatesScope; label: string; description: string }[] = [
  { scope: "all", label: "All sessions", description: "Receive updates from every session." },
  { scope: "sidebar", label: "Sidebar sessions", description: "Receive updates for sessions loaded in the sidebar." },
];

const LEVELS: { level: BackgroundUpdateLevel; label: string; description: string }[] = [
  { level: "final", label: "Final replies", description: "Receive final assistant replies and attention updates." },
  { level: "messages", label: "All assistant messages", description: "Receive intermediate and final assistant messages, plus attention updates." },
];

const CONVERSATION_LEVELS: { level: ConversationUpdateLevel; label: string; description: string }[] = [
  { level: "final", label: "Final replies", description: "Receive user messages, final assistant replies, and attention updates." },
  { level: "messages", label: "Messages", description: "Include intermediate assistant messages." },
  { level: "steps", label: "Steps", description: "Include collapsed activity group summaries. Expand groups to load their details." },
  { level: "all", label: "All events", description: "Receive every live event, including tool calls and their details." },
];

/** Session scope, delivery filters, and connection details share one footer affordance. */
export function BackgroundUpdatesControl({
  scope,
  level,
  conversation_level,
  conversation_available = false,
  status,
  on_scope_change,
  on_level_change,
  on_conversation_level_change,
}: BackgroundUpdatesControlProps) {
  const [is_open, setOpen] = useState(false);
  const panel_id = useId();
  const trigger_ref = useRef<HTMLButtonElement>(null);
  const close = useCallback(() => setOpen(false), []);
  const panel_ref = useFloatingPanel(is_open, close, trigger_ref);
  const scopeLabel = scope === "all" ? "All sessions" : "Sidebar sessions";
  const levelLabel = level === "final" ? "Final replies" : "All assistant messages";
  const isConnecting = status?.startsWith("Connecting") === true;
  const phaseLabel = isConnecting ? "Connecting" : status ? "Unavailable" : "Live";

  return (
    <div className="background-updates">
      <button
        aria-controls={panel_id}
        aria-expanded={is_open}
        aria-haspopup="dialog"
        aria-label={`Live updates. ${scopeLabel}. ${levelLabel}. ${phaseLabel}. Change settings`}
        className="status-bar__updates"
        data-tone={isConnecting ? "active" : status ? "warning" : "neutral"}
        onClick={() => setOpen((open) => !open)}
        ref={trigger_ref}
        title={`Live updates · ${scopeLabel} · ${levelLabel} · ${phaseLabel}`}
        type="button"
      >
        {isConnecting ? <span aria-hidden="true" className="status-bar__spinner" />
          : status ? <WarningIcon aria-hidden="true" /> : null}
        <span>{level === "final" ? "Final" : "Messages"} · {scope === "all" ? "All" : "Sidebar"}</span>
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
          <h2 id={`${panel_id}-title`}>Live updates</h2>
          <button aria-label="Close live updates" className="icon-button" onClick={close} type="button"><CloseIcon /></button>
        </header>
        <fieldset className="background-updates-panel__options">
          <legend className="background-updates-panel__legend">Sessions</legend>
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
        <fieldset className="background-updates-panel__options">
          <legend className="background-updates-panel__legend" id={`${panel_id}-background-level-title`}>Background delivery</legend>
          <select
            aria-describedby={`${panel_id}-background-level-description`}
            aria-labelledby={`${panel_id}-background-level-title`}
            className="background-updates-panel__select"
            onChange={(event) => on_level_change(event.currentTarget.value as BackgroundUpdateLevel)}
            value={level}
          >
            {LEVELS.map((choice) => <option key={choice.level} value={choice.level}>{choice.label}</option>)}
          </select>
          <p className="background-updates-panel__level-description" id={`${panel_id}-background-level-description`}>
            {LEVELS.find((choice) => choice.level === level)?.description}
          </p>
        </fieldset>
        <fieldset className="background-updates-panel__options" disabled={!conversation_available}>
          <legend className="background-updates-panel__legend" id={`${panel_id}-conversation-level-title`}>Open conversation</legend>
          <select
            aria-describedby={`${panel_id}-conversation-level-description`}
            aria-labelledby={`${panel_id}-conversation-level-title`}
            className="background-updates-panel__select"
            onChange={(event) => on_conversation_level_change(event.currentTarget.value as ConversationUpdateLevel)}
            value={conversation_level}
          >
            {CONVERSATION_LEVELS.map((choice) => <option key={choice.level} value={choice.level}>{choice.label}</option>)}
          </select>
          <p className="background-updates-panel__level-description" id={`${panel_id}-conversation-level-description`}>
            {conversation_available
              ? CONVERSATION_LEVELS.find((choice) => choice.level === conversation_level)?.description
              : "Open a session to choose its live delivery level."}
          </p>
        </fieldset>
        <p aria-live="polite" className="background-updates-panel__status" data-tone={status && !isConnecting ? "warning" : "neutral"}>
          {status ?? "Receiving live background updates."}
        </p>
      </div> : null}
    </div>
  );
}
