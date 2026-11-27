import { useLayoutEffect, useMemo, useRef, useState } from "react";

import { flushReadingPositions, readReadingPosition, saveReadingPosition } from "./readingPosition";
import { createTimelineRowIndex, visibleTimelineRows } from "./timelineRows";

interface Anchor {
  key: string;
  top: number;
}

interface ScrollPosition {
  top: number;
  height: number;
  viewport: number;
  anchors: Anchor[];
}

interface TimelineScrollOptions {
  session_key: string | null;
  initial_page_loaded: boolean;
  last_event?: string;
  on_follow_change?: (following: boolean) => void;
}

const BOTTOM_THRESHOLD = 48;

function restorePosition(timeline: HTMLElement, position: ScrollPosition, rows?: Map<string, HTMLElement>) {
  let top = position.top;
  for (const anchor of position.anchors) {
    const row = rows?.get(anchor.key);
    if (row) {
      top = timeline.scrollTop + row.getBoundingClientRect().top - timeline.getBoundingClientRect().top - anchor.top;
      break;
    }
  }
  timeline.scrollTop = Math.max(0, Math.min(top, timeline.scrollHeight - timeline.clientHeight));
}

/** One owner for both React updates and asynchronous changes to layout. */
export function useTimelineScroll({ session_key, initial_page_loaded, last_event, on_follow_change }: TimelineScrollOptions) {
  const [isFollowing, setIsFollowing] = useState(true);
  const [returnPosition, setReturnPosition] = useState<ScrollPosition | null>(null);
  const timelineRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const rowIndex = useMemo(createTimelineRowIndex, []);
  const inputFrame = useRef<number | null>(null);
  const observation = useRef<{
    observer: ResizeObserver;
    timeline: HTMLDivElement;
    content: HTMLDivElement | null;
  } | null>(null);
  const state = useRef({
    session_key,
    initialized: false,
    following: true,
    user_scroll: false,
    downward_scroll: false,
    position: null as ScrollPosition | null,
  });
  const followCallback = useRef(on_follow_change);
  followCallback.current = on_follow_change;

  function setFollowing(following: boolean) {
    if (state.current.following === following) return;
    state.current.following = following;
    if (following) setReturnPosition(null);
    setIsFollowing(following);
    followCallback.current?.(following);
  }

  function capture() {
    const timeline = timelineRef.current;
    if (!timeline) return;
    const viewport = timeline.getBoundingClientRect();
    const visible = contentRef.current ? visibleTimelineRows(rowIndex.get(contentRef.current).roots, viewport) : [];
    // Prefer the smallest row crossing the reading line. A turn can span many
    // screens; anchoring its outer box misses changes above a visible child.
    const ancestors = new Set<HTMLElement>();
    for (const { element } of visible) {
      for (let parent = element.parentElement; parent && parent !== timeline; parent = parent.parentElement) {
        ancestors.add(parent);
      }
    }
    const leaves = visible.filter(({ element }) => !ancestors.has(element));
    const crossing = leaves.filter(({ rect }) => rect.top <= viewport.top)
      .sort((left, right) => left.rect.height - right.rect.height);
    const below = leaves.filter(({ rect }) => rect.top > viewport.top);
    const fallback = visible.filter(({ element }) => ancestors.has(element));
    state.current.position = {
      top: timeline.scrollTop,
      height: timeline.scrollHeight,
      viewport: timeline.clientHeight,
      anchors: [...crossing, ...below, ...fallback].map(({ element, rect }) => ({
        key: element.dataset.scrollKey!, top: rect.top - viewport.top,
      })),
    };
    if (state.current.initialized && state.current.session_key && last_event) {
      const anchors = visible.filter(({ element }) => element.dataset.readingSlot)
        .slice(0, 8).map(({ element, rect }) => ({
          slot_key: element.dataset.readingSlot!,
          type: element.dataset.readingType!,
          timestamp: element.dataset.readingTimestamp || null,
          top: rect.top - viewport.top,
        }));
      if (anchors.length) saveReadingPosition(state.current.session_key, {
        anchors, last_event, at_end: state.current.following,
      });
    }
  }

  function acceptUserScroll(): boolean {
    const timeline = timelineRef.current;
    const previous = state.current.position;
    if (!timeline || !previous) return false;
    const movement = timeline.scrollTop - previous.top;
    const resized = timeline.scrollHeight !== previous.height || timeline.clientHeight !== previous.viewport;
    const bottom = Math.max(0, timeline.scrollHeight - timeline.clientHeight);
    const clamped = resized && previous.top > bottom && Math.abs(timeline.scrollTop - bottom) < 1;
    if (clamped) {
      // Layout clamping can reuse the same scroll event as a pending gesture.
      // Consume that hint before a later observer update reads it as a new
      // downward action at the bottom.
      state.current.user_scroll = false;
      state.current.downward_scroll = false;
      return false;
    }
    // Preserve a pending gesture when a React commit precedes native scrolling.
    if (resized && Math.abs(movement) <= 0.5) return false;
    if (Math.abs(movement) <= 0.5) {
      // A downward wheel/End press at the physical end can produce no scroll
      // delta. It still means the reader has reached the newest content.
      if (!resized && state.current.user_scroll && state.current.downward_scroll
        && bottom - timeline.scrollTop < BOTTOM_THRESHOLD) {
        state.current.user_scroll = false;
        state.current.downward_scroll = false;
        setFollowing(true);
        capture();
        return true;
      }
      return false;
    }
    state.current.user_scroll = false;
    state.current.downward_scroll = false;
    // Even a small upward gesture pauses following. The proximity threshold
    // only resumes it when the reader actually moves down toward the end.
    setFollowing(movement > 0 && bottom - timeline.scrollTop < BOTTOM_THRESHOLD);
    capture();
    return true;
  }

  function reconcile() {
    const timeline = timelineRef.current;
    if (!timeline || !state.current.initialized) return;
    // A commit can happen after native scrolling but before its scroll event.
    if (state.current.user_scroll && acceptUserScroll()) return;
    const previous = state.current.position;
    if (state.current.following) {
      timeline.scrollTop = Math.max(0, timeline.scrollHeight - timeline.clientHeight);
    } else if (previous) {
      restorePosition(timeline, previous, contentRef.current ? rowIndex.get(contentRef.current).byKey : undefined);
    }
    capture();
  }

  // Run after every parent commit, including loading banners and expanded detail.
  // Child-only changes (translation, font reflow) are covered by ResizeObserver.
  useLayoutEffect(() => {
    if (state.current.session_key !== session_key) {
      flushReadingPositions();
      setIsFollowing(true);
      setReturnPosition(null);
      state.current = { session_key, initialized: false, following: true, user_scroll: false, downward_scroll: false, position: null };
    }
    if (session_key && initial_page_loaded && !state.current.initialized) {
      const saved = last_event ? readReadingPosition(session_key) : null;
      if (saved) {
        const following = saved.at_end && saved.last_event === last_event;
        setFollowing(following);
        if (!following && timelineRef.current) {
          const timeline = timelineRef.current;
          const elements = [...timeline.querySelectorAll<HTMLElement>("[data-reading-slot]")];
          const anchor = saved.anchors.flatMap((anchor) => {
            const element = elements.find((element) => element.dataset.readingSlot === anchor.slot_key
              && element.dataset.readingType === anchor.type
              && (element.dataset.readingTimestamp || null) === anchor.timestamp);
            return element ? [{ anchor, element }] : [];
          })[0];
          // A removed anchor must not send the reader to the start of a large
          // retained history. Show recent context, paused, until an explicit
          // jump or downward scroll acknowledges the displayed end.
          timeline.scrollTop = anchor
            ? timeline.scrollTop + anchor.element.getBoundingClientRect().top
              - timeline.getBoundingClientRect().top - anchor.anchor.top
            : Math.max(0, timeline.scrollHeight - 2 * timeline.clientHeight);
          capture();
        }
      } else {
        setFollowing(true);
      }
      state.current.initialized = true;
    }
    reconcile();
  });

  useLayoutEffect(() => {
    const onVisibility = () => { if (document.visibilityState === "hidden") flushReadingPositions(); };
    window.addEventListener("pagehide", flushReadingPositions);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.removeEventListener("pagehide", flushReadingPositions);
      document.removeEventListener("visibilitychange", onVisibility);
      flushReadingPositions();
      rowIndex.disconnect();
      if (inputFrame.current !== null) cancelAnimationFrame(inputFrame.current);
      observation.current?.observer.disconnect();
      observation.current = null;
    };
  }, [rowIndex]);

  const reconcileRef = useRef(reconcile);
  reconcileRef.current = reconcile;
  useLayoutEffect(() => {
    const timeline = timelineRef.current;
    const content = contentRef.current;
    if (observation.current?.timeline === timeline && observation.current?.content === content) return;
    observation.current?.observer.disconnect();
    observation.current = null;
    if (!timeline || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => reconcileRef.current());
    observer.observe(timeline);
    if (content) observer.observe(content);
    observation.current = { observer, timeline, content };
  });

  function onScroll() {
    const timeline = timelineRef.current;
    if (!timeline || !state.current.initialized) return;
    if (!acceptUserScroll()) {
      // Programmatic scrolling and browser clamping after a shrinking card must
      // not turn reading mode back into follow mode (or vice versa).
      reconcile();
    }
  }

  function noteUserScroll(upward?: boolean) {
    state.current.user_scroll = true;
    state.current.downward_scroll = upward === false;
    if (upward) setFollowing(false);
    if (inputFrame.current !== null) cancelAnimationFrame(inputFrame.current);
    inputFrame.current = requestAnimationFrame(() => {
      inputFrame.current = null;
      state.current.user_scroll = false;
      state.current.downward_scroll = false;
    });
  }

  function pause() {
    state.current.user_scroll = false;
    state.current.downward_scroll = false;
    setFollowing(false);
    capture();
  }

  function jumpToLatest() {
    setReturnPosition(null);
    state.current.user_scroll = false;
    state.current.downward_scroll = false;
    setFollowing(true);
    reconcile();
  }

  function reveal(element: HTMLElement | null | undefined, remember_position = true) {
    const timeline = timelineRef.current;
    if (!element || !timeline?.contains(element)) return;
    const offset = element.getBoundingClientRect().top - timeline.getBoundingClientRect().top - timeline.clientTop;
    const wasReading = !state.current.following && timeline.scrollTop < timeline.scrollHeight - timeline.clientHeight - 1;
    pause();
    if (remember_position && wasReading && !returnPosition && Math.abs(offset) > 1) setReturnPosition(state.current.position);
    // scrollIntoView can also move the app shell or a containing dialog.
    timeline.scrollTop += offset;
    element.focus({ preventScroll: true });
    capture();
  }

  function returnToPosition() {
    const timeline = timelineRef.current;
    if (!timeline || !returnPosition) return;
    pause();
    restorePosition(timeline, returnPosition, contentRef.current ? rowIndex.get(contentRef.current).byKey : undefined);
    timeline.focus({ preventScroll: true });
    capture();
    setReturnPosition(null);
  }

  return { timelineRef, contentRef, onScroll, noteUserScroll, pause, reveal, jumpToLatest,
    returnToPosition, canReturn: !!returnPosition, isFollowing };
}
