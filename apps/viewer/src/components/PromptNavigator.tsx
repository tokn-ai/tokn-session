import { useCallback, useId, useRef, useState } from "react";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import type { EventSummary } from "../lib/types";
import { formatTimestamp } from "../lib/state";
import { PromptListIcon } from "./Icons";

interface PromptNavigatorProps {
  events: EventSummary[];
  has_older: boolean;
  loading_older: boolean;
  on_older: () => void;
  on_jump: (event_key: string) => void;
}

export function PromptNavigator({ events, has_older, loading_older, on_older, on_jump }: PromptNavigatorProps) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const close = useCallback(() => setOpen(false), []);
  const panel = useFloatingPanel(open, close, trigger);
  const id = useId();
  const prompts = events.filter((event) => event.type === "message" && event.role === "user" && !event.is_hidden);
  if (!prompts.length && !has_older && !open) return null;
  return <div className="prompt-nav">
    <button type="button" aria-label="Jump to prompt" title="Jump to prompt" aria-expanded={open} aria-controls={id}
      ref={trigger} className="icon-button" onClick={() => setOpen(!open)}><PromptListIcon /></button>
    {open && <div tabIndex={-1} ref={panel} id={id} className="prompt-nav__panel" role="region" aria-label="Prompts in this session">
      <h3>Jump to prompt</h3>
      <div className="prompt-nav__list">
        {has_older && <button type="button" className="text-button" onClick={on_older} disabled={loading_older}>
          {loading_older ? "Loading earlier prompts…" : "Load earlier history"}</button>}
        {!prompts.length && <p>No prompts in the loaded history.</p>}
        {prompts.map((prompt) => <button type="button" key={prompt.event_key} className="prompt-nav__row"
          onClick={() => { on_jump(prompt.event_key); close(); }}>
          <span>{prompt.summary.trim() || (prompt.images?.length ? "Image attachment" : prompt.title)}</span>
          {prompt.timestamp && <small>{formatTimestamp(prompt.timestamp, "time")}</small>}
        </button>)}
      </div>
    </div>}
  </div>;
}
