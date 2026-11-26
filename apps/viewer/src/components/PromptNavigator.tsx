import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type RefObject } from "react";
import { readingEventKey } from "../lib/readingPosition";
import { formatTimestamp } from "../lib/state";
import type { EventSummary } from "../lib/types";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import { PromptListIcon, SearchIcon } from "./Icons";

interface PromptNavigatorProps {
  events: EventSummary[];
  has_older: boolean;
  loading_older: boolean;
  timeline_ref: RefObject<HTMLDivElement | null>;
  on_older: () => void;
  on_jump: (event_key: string) => void;
}

export function isVisiblePrompt(event: EventSummary) {
  return event.type === "message" && event.role === "user" && !event.is_hidden;
}

function showRow(list: HTMLElement, row: HTMLElement) {
  const bounds = list.getBoundingClientRect();
  const target = row.getBoundingClientRect();
  if (target.top < bounds.top) list.scrollTop += target.top - bounds.top;
  else if (target.bottom > bounds.bottom) list.scrollTop += target.bottom - bounds.bottom;
}

function currentPrompt(timeline: HTMLElement | null) {
  if (!timeline) return null;
  const viewport = timeline.getBoundingClientRect();
  if (!viewport.height) return null;
  const prompts = timeline.querySelectorAll<HTMLElement>("[data-reading-prompt]");
  const line = viewport.top + timeline.clientTop + 1;
  let start = 0;
  let end = prompts.length;
  while (start < end) {
    const middle = Math.floor((start + end) / 2);
    if (prompts[middle].getBoundingClientRect().top <= line) start = middle + 1;
    else end = middle;
  }
  if (start) return prompts[start - 1].dataset.scrollKey!;
  const first = prompts[0];
  return first && first.getBoundingClientRect().top < viewport.bottom ? first.dataset.scrollKey! : null;
}

export function PromptNavigator({ events, has_older, loading_older, timeline_ref, on_older, on_jump }: PromptNavigatorProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [current, setCurrent] = useState<string | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const close = useCallback(() => setOpen(false), []);
  const panel = useFloatingPanel(open, close, trigger);
  const id = useId();

  useLayoutEffect(() => {
    const row = list.current?.querySelector<HTMLElement>('[aria-current="location"]');
    if (list.current && row) showRow(list.current, row);
  }, [open]);
  useEffect(() => {
    const timeline = timeline_ref.current;
    if (!open || !timeline) return;
    const update = () => setCurrent(currentPrompt(timeline));
    update();
    timeline.addEventListener("scroll", update, { passive: true });
    return () => timeline.removeEventListener("scroll", update);
  }, [open, events, timeline_ref]);
  useEffect(() => {
    // A touch user can open the index without bringing up the on-screen keyboard.
    if (open && !window.matchMedia?.("(pointer: coarse)").matches) search.current?.focus({ preventScroll: true });
  }, [open]);

  if (!open && !has_older && !events.some(isVisiblePrompt)) return null;
  const term = query.trim().toLocaleLowerCase();
  const matches = open ? events.filter(isVisiblePrompt).map((prompt) => ({
    prompt,
    label: prompt.summary.trim() || (prompt.images?.length ? "Image attachment" : prompt.title),
  })).filter(({ label }) => label.toLocaleLowerCase().includes(term)) : [];

  function navigateResults(event: KeyboardEvent) {
    if (!list.current || event.nativeEvent.isComposing) return;
    const rows = [...list.current.querySelectorAll<HTMLButtonElement>(".prompt-nav__row")];
    const index = rows.findIndex((row) => row === event.target);
    if (event.target !== search.current && index === -1) return;
    if (event.key === "Enter" && event.target === search.current && rows.length) {
      event.preventDefault();
      rows[0].click();
    } else if (event.key === "ArrowDown" || (event.key === "ArrowUp" && index >= 0)) {
      event.preventDefault();
      if (event.key === "ArrowUp" && index === 0) {
        search.current?.focus({ preventScroll: true });
        return;
      }
      const next = rows[event.key === "ArrowUp" ? index - 1 : Math.min(index + 1, rows.length - 1)];
      if (next) {
        next.focus({ preventScroll: true });
        showRow(list.current, next);
      }
    }
  }

  return <div className="prompt-nav">
    <button type="button" aria-label="Jump to prompt" title="Jump to prompt" aria-expanded={open} aria-controls={open ? id : undefined}
      ref={trigger} className="icon-button" onClick={() => {
        if (!open) { setQuery(""); setCurrent(currentPrompt(timeline_ref.current)); }
        setOpen(!open);
      }}><PromptListIcon /></button>
    {open && <div tabIndex={-1} ref={panel} id={id} className="prompt-nav__panel" role="region" aria-label="Prompts in this session"
      onKeyDown={navigateResults}>
      <h3>Session prompts</h3>
      <label className="prompt-nav__search">
        <SearchIcon />
        <input ref={search} type="search" value={query} aria-label="Search loaded prompts" placeholder="Search loaded prompts"
          onChange={(event) => { setQuery(event.target.value); if (list.current) list.current.scrollTop = 0; }} />
      </label>
      <div className="prompt-nav__list" ref={list}>
        {!matches.length && <p role="status">{term ? "No matching prompts in the loaded history." : "No prompts in the loaded history."}</p>}
        {matches.map(({ prompt, label }) => {
          const active = readingEventKey(prompt) === current;
          return <button type="button" key={readingEventKey(prompt)} className="prompt-nav__row"
            aria-current={active ? "location" : undefined}
            onClick={() => { on_jump(prompt.event_key); close(); }}>
            <span>{label}</span>
            {active ? <small>Reading</small> : prompt.timestamp && <small>{formatTimestamp(prompt.timestamp, "time")}</small>}
          </button>;
        })}
      </div>
      {has_older && <button type="button" className="prompt-nav__older text-button" onClick={on_older} disabled={loading_older}>
        {loading_older ? "Loading earlier prompts…" : "Load earlier history"}</button>}
    </div>}
  </div>;
}
