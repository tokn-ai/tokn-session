import { useId, useRef, useState } from "react";
import { modelLabel } from "../lib/sessionModels";
import { formatTimestamp, providerLabel, sessionDisplayTitle, subagentDetail } from "../lib/state";
import type { SessionModelState, SessionSummary } from "../lib/types";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import { ChevronIcon } from "./Icons";

interface SessionDetailsProps {
  session: SessionSummary;
  model?: SessionModelState | null;
  count_label: string;
  hidden_label: string | null;
  show_lifecycle: boolean;
  on_lifecycle_change: (show: boolean) => void;
}

export function SessionDetails({ session, model, count_label, hidden_label, show_lifecycle, on_lifecycle_change }: SessionDetailsProps) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const panel = useFloatingPanel(open, () => setOpen(false), trigger);
  const id = useId();
  const title = sessionDisplayTitle(session);
  const child = session.is_subagent && subagentDetail(session);
  const updated = session.updated_at_ms != null ? new Date(session.updated_at_ms) : null;
  const timestamp = updated && Number.isFinite(updated.getTime()) ? updated.toISOString() : session.timestamp;
  return <div className="session-details">
    <h2 aria-labelledby={`${id}-title`}>
      <button ref={trigger} type="button" className="session-details__trigger" title={title}
        aria-label={`Session details for ${title}`} aria-expanded={open} aria-controls={id} onClick={() => setOpen(!open)}>
        <span className="session-details__title" id={`${id}-title`}>{title}</span><ChevronIcon />
        <span className="session-header__provider" data-provider={session.provider}>{providerLabel(session.provider)}</span>
      </button>
    </h2>
    <div hidden={!open} ref={panel} id={id} role="region" aria-label="Session details" tabIndex={-1} className="session-details__panel">
      <dl>
        <dt>Agent</dt><dd className="session-details__provider" data-provider={session.provider}>{providerLabel(session.provider)}</dd>
        {session.project && <><dt>Project</dt><dd>{session.project}</dd></>}
        {session.cwd && <><dt>Directory</dt><dd>{session.cwd}</dd></>}
        {session.is_subagent && <><dt>Subagent</dt><dd>{child || "Yes"}</dd></>}
        {model?.model && <><dt>Model</dt><dd title={model.model.model_id}>{modelLabel(model.model)}
          {model.source === "recorded" ? " · Recorded" : model.source === "catalog" ? " · Model list" : ""}</dd></>}
        <dt>Session</dt><dd>{session.session_id}</dd>
        {timestamp && <><dt>Updated</dt><dd><time dateTime={timestamp}>{formatTimestamp(timestamp)}</time></dd></>}
        <dt>History</dt><dd>{count_label}</dd>
      </dl>
      <label className="session-details__filter">
        <input type="checkbox" aria-describedby={`${id}-events-description`} checked={show_lifecycle}
          onChange={event => on_lifecycle_change(event.currentTarget.checked)} />
        Show technical events
      </label>
      <p id={`${id}-events-description`}>Includes token usage and routine lifecycle events.</p>
      {hidden_label && <p>{hidden_label}</p>}
    </div>
  </div>;
}
