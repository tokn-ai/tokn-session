import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type RefObject } from "react";
import { readingEventKey } from "../lib/readingPosition";
import { formatTimestamp } from "../lib/state";
import type { EventSummary } from "../lib/types";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import { ChevronIcon, PromptListIcon, SearchIcon } from "./Icons";

interface PromptNavigatorProps {
  events: EventSummary[];
  has_older: boolean;
  loading_older: boolean;
  following: boolean;
  timeline_ref: RefObject<HTMLDivElement | null>;
  on_older: () => void;
  on_jump: (row: HTMLElement, remember_position: boolean) => void;
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

interface PromptLocation {
  current: number;
  previous: number;
  next: number;
}

const EMPTY_LOCATION: PromptLocation = { current: -1, previous: -1, next: -1 };

function locatePrompts(timeline: HTMLElement, rows: HTMLElement[], selected?: number): PromptLocation {
  if (!rows.length) return EMPTY_LOCATION;
  if (selected !== undefined) return { current: selected, previous: selected - 1, next: selected + 1 };
  const viewport = timeline.getBoundingClientRect();
  if (!viewport.height) return { ...EMPTY_LOCATION, next: 0 };
  const line = viewport.top + timeline.clientTop;
  let first = 0;
  let end = rows.length;
  while (first < end) {
    const middle = Math.floor((first + end) / 2);
    if (rows[middle].getBoundingClientRect().top < line - 1) first = middle + 1;
    else end = middle;
  }
  const nextTop = rows[first]?.getBoundingClientRect().top ?? Infinity;
  const atPrompt = nextTop <= line + 1;
  return {
    current: atPrompt || (first === 0 && nextTop < viewport.bottom) ? first : first - 1,
    previous: first - 1,
    next: atPrompt ? first + 1 : first,
  };
}

export function PromptNavigator({ events, has_older, loading_older, following, timeline_ref, on_older, on_jump }: PromptNavigatorProps) {
  const prompts = useMemo(() => events.filter(isVisiblePrompt).map((event) => ({
    key: readingEventKey(event), timestamp: event.timestamp,
    label: event.summary.trim() || (event.images?.length ? "Image attachment" : event.title),
  })), [events]);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [location, setLocation] = useState(EMPTY_LOCATION);
  const rows = useRef<HTMLElement[]>([]);
  const landing = useRef<{ key: string; index: number; top: number; offset: number } | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const close = useCallback(() => setOpen(false), []);
  const panel = useFloatingPanel(open, close, trigger);
  const id = useId();

  const readLocation = useCallback(() => {
    const timeline = timeline_ref.current;
    if (!timeline) return EMPTY_LOCATION;
    const last = landing.current;
    // Several prompts can share the same clamped scroll position at the end.
    // Keep stepping from the chosen prompt until the reader scrolls away.
    if (last && Math.abs(timeline.scrollTop - last.top) > 1) {
      const row = rows.current[last.index];
      const offset = row && row.getBoundingClientRect().top - timeline.getBoundingClientRect().top;
      // Prepended history can change scrollTop while the chosen row stays put.
      if (offset !== undefined && Math.abs(offset - last.offset) <= 1) last.top = timeline.scrollTop;
      else landing.current = null;
    }
    const next = locatePrompts(timeline, rows.current, landing.current?.index);
    setLocation(last => last.current === next.current && last.previous === next.previous && last.next === next.next ? last : next);
    return next;
  }, [timeline_ref]);

  const jump = useCallback((index: number) => {
    const timeline = timeline_ref.current;
    const row = rows.current[index];
    if (!timeline || !row) return false;
    const continuing = landing.current && Math.abs(timeline.scrollTop - landing.current.top) <= 1;
    on_jump(row, !continuing);
    landing.current = { key: row.dataset.scrollKey!, index, top: timeline.scrollTop,
      offset: row.getBoundingClientRect().top - timeline.getBoundingClientRect().top };
    readLocation();
    return true;
  }, [on_jump, readLocation, timeline_ref]);

  useEffect(() => {
    const timeline = timeline_ref.current;
    if (!timeline) return;
    rows.current = [...timeline.querySelectorAll<HTMLElement>("[data-reading-prompt]")];
    if (landing.current) {
      const key = landing.current.key;
      const index = rows.current.findIndex((row) => row.dataset.scrollKey === key);
      if (index >= 0) landing.current.index = index;
      else landing.current = null;
    }
    readLocation();
    if (!rows.current.length) return;
    timeline.addEventListener("scroll", readLocation, { passive: true });
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(readLocation);
    observer?.observe(timeline);
    if (timeline.lastElementChild) observer?.observe(timeline.lastElementChild);
    return () => {
      observer?.disconnect();
      timeline.removeEventListener("scroll", readLocation);
    };
  }, [prompts, readLocation, timeline_ref]);

  useEffect(() => {
    if (following && landing.current) {
      landing.current = null;
      readLocation();
    }
  }, [following, readLocation]);

  useEffect(() => {
    const timeline = timeline_ref.current;
    if (!timeline) return;
    function onKeyDown(event: globalThis.KeyboardEvent) {
      if (!event.altKey || event.ctrlKey || event.metaKey || event.shiftKey || event.defaultPrevented || event.isComposing) return;
      if (event.target !== timeline && !(event.target instanceof HTMLElement && event.target.hasAttribute("data-reading-slot"))) return;
      if (event.key !== "ArrowUp" && event.key !== "ArrowDown") return;
      const where = readLocation();
      if (jump(event.key === "ArrowUp" ? where.previous : where.next)) event.preventDefault();
    }
    timeline.addEventListener("keydown", onKeyDown);
    return () => timeline.removeEventListener("keydown", onKeyDown);
  }, [jump, readLocation, timeline_ref]);

  useLayoutEffect(() => {
    const row = list.current?.querySelector<HTMLElement>('[aria-current="location"]');
    if (list.current && row) showRow(list.current, row);
  }, [open]);
  useEffect(() => {
    // A touch user can open the index without bringing up the on-screen keyboard.
    if (open && !window.matchMedia?.("(pointer: coarse)").matches) search.current?.focus({ preventScroll: true });
  }, [open]);

  if (!open && !has_older && !prompts.length) return null;
  const term = query.trim().toLocaleLowerCase();
  const showSteps = prompts.length > 1;
  function navigateResults(event: KeyboardEvent) {
    if (!list.current || event.nativeEvent.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const results = [...list.current.querySelectorAll<HTMLButtonElement>(".prompt-nav__row")];
    const index = results.findIndex((row) => row === event.target);
    if (event.target !== search.current && index === -1) return;
    if (event.key === "Enter" && event.target === search.current && results.length) {
      event.preventDefault();
      results[0].click();
    } else if (event.key === "ArrowDown" || (event.key === "ArrowUp" && index >= 0)) {
      event.preventDefault();
      if (event.key === "ArrowUp" && index === 0) {
        search.current?.focus({ preventScroll: true });
        return;
      }
      const next = results[event.key === "ArrowUp" ? index - 1 : Math.min(index + 1, results.length - 1)];
      if (next) {
        next.focus({ preventScroll: true });
        showRow(list.current, next);
      }
    }
  }

  const matches = open ? prompts.map((prompt, index) => ({ prompt, index }))
    .filter(({ prompt }) => prompt.label.toLocaleLowerCase().includes(term)) : [];
  return <div className="prompt-nav">
    <div className="prompt-nav__controls" data-stepping={showSteps || undefined} role="group" aria-label="Prompt navigation">
      {showSteps && <button type="button" className="icon-button prompt-nav__step prompt-nav__step--previous"
        aria-label="Previous prompt" aria-keyshortcuts="Alt+ArrowUp" title="Previous prompt (Alt+↑)"
        disabled={location.previous < 0} onClick={() => jump(readLocation().previous)}><ChevronIcon /></button>}
      <button type="button" aria-label="Jump to prompt" title="Jump to prompt" aria-expanded={open} aria-controls={open ? id : undefined}
        ref={trigger} className="icon-button" onClick={() => {
          if (!open) { setQuery(""); readLocation(); }
          setOpen(!open);
        }}><PromptListIcon /></button>
      {showSteps && <button type="button" className="icon-button prompt-nav__step"
        aria-label="Next prompt" aria-keyshortcuts="Alt+ArrowDown" title="Next prompt (Alt+↓)"
        disabled={location.next < 0 || location.next >= prompts.length} onClick={() => jump(readLocation().next)}><ChevronIcon /></button>}
    </div>
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
        {matches.map(({ prompt, index }) => {
          const active = index === location.current;
          return <button type="button" key={prompt.key} className="prompt-nav__row"
            aria-current={active ? "location" : undefined}
            onClick={() => { jump(index); close(); }}>
            <span>{prompt.label}</span>
            {active ? <small>Reading</small> : prompt.timestamp && <small>{formatTimestamp(prompt.timestamp, "time")}</small>}
          </button>;
        })}
      </div>
      {has_older && <button type="button" className="prompt-nav__older text-button" onClick={on_older} disabled={loading_older}>
        {loading_older ? "Loading earlier prompts…" : "Load earlier history"}</button>}
    </div>}
  </div>;
}
