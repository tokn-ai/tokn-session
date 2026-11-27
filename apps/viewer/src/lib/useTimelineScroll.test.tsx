import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useTimelineScroll } from "./useTimelineScroll";
import { readReadingPosition } from "./readingPosition";

interface TimelineRow {
  key: string;
  top: number;
  height: number;
  hidden?: boolean;
  children?: TimelineRow[];
}

// jsdom has no layout. Model the browser's clamped scroll position and measure
// real keyed DOM nodes against it, so anchors move when content above them grows.
class TimelineLayout {
  height = 1000;
  viewport_height = 200;
  viewport_top = 100;
  row_measurements = 0;
  private scroll_top = 0;
  rows: TimelineRow[] = Array.from({ length: 5 }, (_, index) => ({
    key: `row-${index}`,
    top: index * 200,
    height: 200,
  }));

  get top() {
    this.scroll_top = Math.max(0, Math.min(this.scroll_top, this.height - this.viewport_height));
    return this.scroll_top;
  }

  set top(value: number) {
    this.scroll_top = Math.max(0, Math.min(value, this.height - this.viewport_height));
  }

  row(key: string): TimelineRow {
    const find = (rows: TimelineRow[]): TimelineRow | undefined => {
      for (const row of rows) {
        if (row.key === key) return row;
        const child = find(row.children ?? []);
        if (child) return child;
      }
      return undefined;
    };
    const row = find(this.rows);
    if (!row) throw new Error(`Missing layout row: ${key}`);
    return row;
  }

  attachViewport(element: HTMLDivElement) {
    Object.defineProperties(element, {
      scrollTop: { configurable: true, get: () => this.top, set: (value: number) => { this.top = value; } },
      scrollHeight: { configurable: true, get: () => this.height },
      clientHeight: { configurable: true, get: () => this.viewport_height },
    });
    element.getBoundingClientRect = () => new DOMRect(0, this.viewport_top, 500, this.viewport_height);
  }

  attachRow(element: HTMLDivElement, key: string) {
    element.getBoundingClientRect = () => {
      this.row_measurements++;
      const row = this.row(key);
      return new DOMRect(0, this.viewport_top + row.top - this.top, 500, row.height);
    };
  }
}

const observers = new Set<LayoutObserver>();
const animation_frames = new Map<number, FrameRequestCallback>();
let next_frame_id = 0;

function advanceAnimationFrame() {
  act(() => {
    const pending = [...animation_frames.values()];
    animation_frames.clear();
    for (const callback of pending) callback(0);
  });
}

class LayoutObserver {
  targets = new Set<Element>();

  constructor(private callback: ResizeObserverCallback) {
    observers.add(this);
  }

  observe(target: Element) {
    this.targets.add(target);
  }

  unobserve(target: Element) {
    this.targets.delete(target);
  }

  disconnect() {
    observers.delete(this);
    this.targets.clear();
  }

  deliver(target: Element) {
    if (this.targets.has(target)) this.callback([], this as unknown as ResizeObserver);
  }
}

interface HarnessOptions {
  session_key: string | null;
  initial_page_loaded: boolean;
  last_event?: string;
}

function TimelineHarness({
  layout,
  on_follow_change,
  ...options
}: HarnessOptions & { layout: TimelineLayout; on_follow_change: (following: boolean) => void }) {
  const scroll = useTimelineScroll({ ...options, on_follow_change });
  function renderRow(row: TimelineRow) {
    return (
      <div
        key={row.key}
        data-scroll-key={row.key}
        data-reading-slot={row.key}
        data-reading-type="message"
        data-reading-timestamp=""
        hidden={row.hidden}
        ref={(element) => { if (element) layout.attachRow(element, row.key); }}
      >
        {row.children?.map(renderRow)}
      </div>
    );
  }
  return (
    <>
      <div
        data-testid="viewport"
        ref={(element) => {
          scroll.timelineRef.current = element;
          if (element) layout.attachViewport(element);
        }}
        onScroll={scroll.onScroll}
        onWheel={(event) => scroll.noteUserScroll(event.deltaY < 0)}
      >
        <div data-testid="content" ref={scroll.contentRef}>{layout.rows.map(renderRow)}</div>
      </div>
      <button onClick={scroll.jumpToLatest}>Jump to latest</button>
    </>
  );
}

function mountTimeline(layout = new TimelineLayout(), initial: Partial<HarnessOptions> = {}) {
  let options: HarnessOptions = { session_key: "session-one", initial_page_loaded: true, ...initial };
  const on_follow_change = vi.fn();
  const view = render(<TimelineHarness layout={layout} on_follow_change={on_follow_change} {...options} />);
  const viewport = view.getByTestId("viewport");
  const content = view.getByTestId("content");
  return {
    layout,
    viewport,
    on_follow_change,
    commit(next: Partial<HarnessOptions> = {}) {
      options = { ...options, ...next };
      view.rerender(<TimelineHarness layout={layout} on_follow_change={on_follow_change} {...options} />);
    },
    resize(target = content) {
      act(() => { for (const observer of [...observers]) observer.deliver(target); });
    },
    readAt(top: number) {
      fireEvent.wheel(viewport, { deltaY: top < viewport.scrollTop ? -100 : 100 });
      viewport.scrollTop = top;
      fireEvent.scroll(viewport);
    },
    jump() {
      fireEvent.click(view.getByRole("button", { name: "Jump to latest" }));
    },
  };
}

beforeEach(() => {
  localStorage.clear();
  vi.stubGlobal("ResizeObserver", LayoutObserver);
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    const frame_id = ++next_frame_id;
    animation_frames.set(frame_id, callback);
    return frame_id;
  });
  vi.stubGlobal("cancelAnimationFrame", (frame_id: number) => animation_frames.delete(frame_id));
});

afterEach(() => {
  cleanup();
  observers.clear();
  animation_frames.clear();
  vi.unstubAllGlobals();
});

describe("useTimelineScroll", () => {
  it("bounds row measurements and coalesces bookmark writes while scrolling a long history", () => {
    const layout = new TimelineLayout();
    layout.rows = Array.from({ length: 1200 }, (_, i) => ({ key: `row-${i}`, top: i * 80, height: 80 }));
    layout.height = layout.rows.length * 80;
    const timeline = mountTimeline(layout, { last_event: "end" });
    timeline.readAt(40_000);
    const writes = vi.spyOn(Storage.prototype, "setItem");
    layout.row_measurements = 0;
    try {
      for (let i = 1; i <= 20; i++) {
        timeline.viewport.scrollTop = 40_000 + i;
        fireEvent.scroll(timeline.viewport);
      }
      const readingLine = { slot_key: "row-500", top: -20 };
      expect(readReadingPosition("session-one")?.anchors[0]).toMatchObject(readingLine);
      expect.soft(layout.row_measurements).toBeLessThan(400);
      expect.soft(writes).not.toHaveBeenCalled();
      fireEvent(window, new Event("pagehide"));
      expect(writes).toHaveBeenCalledTimes(1);
      expect(Object.values(JSON.parse(writes.mock.calls[0][1]))[0]).toMatchObject({
        anchors: [readingLine, { slot_key: "row-501" }, { slot_key: "row-502" }],
      });
    } finally { writes.mockRestore(); }
  });

  it("waits for the initial page, then starts each session at the bottom", () => {
    const timeline = mountTimeline(undefined, { initial_page_loaded: false });
    expect(timeline.viewport.scrollTop).toBe(0);

    timeline.commit({ initial_page_loaded: true });
    expect(timeline.viewport.scrollTop).toBe(800);
    timeline.readAt(300);

    timeline.layout.height = 1400;
    timeline.commit({ session_key: "session-two", initial_page_loaded: false });
    expect(timeline.viewport.scrollTop).toBe(300);
    timeline.commit({ initial_page_loaded: true });
    expect(timeline.viewport.scrollTop).toBe(1200);

    timeline.layout.height = 1500;
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(1300);
  });

  it("restores a saved reading line after switching sessions and after remount", () => {
    const timeline = mountTimeline(undefined, { last_event: "last-one" });
    timeline.readAt(350);
    timeline.commit({ session_key: "session-two" });
    expect(timeline.viewport.scrollTop).toBe(800);
    timeline.commit({ session_key: "session-one" });
    expect(timeline.viewport.scrollTop).toBe(350);
    cleanup();
    const reopened = mountTimeline(undefined, { last_event: "last-one" });
    expect(reopened.viewport.scrollTop).toBe(350);
    expect(reopened.on_follow_change).toHaveBeenCalledWith(false);
  });

  it("keeps a removed reading anchor near recent content without acknowledging unseen replies", () => {
    const original = mountTimeline(undefined, { last_event: "old-last" });
    original.readAt(350);
    cleanup();
    const layout = new TimelineLayout();
    layout.height = 10000;
    layout.rows = [{ key: "replacement", top: 0, height: 10000 }];
    const reopened = mountTimeline(layout, { last_event: "new-last" });
    expect(reopened.viewport.scrollTop).toBe(9600);
    expect(reopened.on_follow_change).toHaveBeenLastCalledWith(false);
    reopened.jump();
    expect(reopened.viewport.scrollTop).toBe(9800);
    expect(reopened.on_follow_change).toHaveBeenLastCalledWith(true);
  });

  it("restores the old end when new replies arrived while closed, then follows an explicit jump", () => {
    mountTimeline(undefined, { last_event: "old-last" });
    cleanup();
    const layout = new TimelineLayout();
    layout.height = 1400;
    layout.rows.push({ key: "new-reply", top: 1000, height: 400 });
    const reopened = mountTimeline(layout, { last_event: "new-last" });
    expect(reopened.viewport.scrollTop).toBe(800);
    expect(reopened.on_follow_change).toHaveBeenCalledWith(false);
    reopened.jump();
    expect(reopened.viewport.scrollTop).toBe(1200);
    expect(reopened.on_follow_change).toHaveBeenLastCalledWith(true);
    cleanup();
    const again = mountTimeline(layout, { last_event: "new-last" });
    expect(again.viewport.scrollTop).toBe(1200);
    expect(again.on_follow_change).not.toHaveBeenCalled();
  });

  it("follows React and asynchronous growth without treating programmatic scroll events as user input", () => {
    const timeline = mountTimeline();
    timeline.layout.height = 1200;
    timeline.commit();
    expect(timeline.viewport.scrollTop).toBe(1000);
    fireEvent.scroll(timeline.viewport);

    timeline.layout.height = 1400;
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(1200);
    fireEvent.scroll(timeline.viewport);

    // Browsers can dispatch a layout-driven scroll before ResizeObserver.
    timeline.layout.height = 1500;
    fireEvent.scroll(timeline.viewport);
    expect(timeline.viewport.scrollTop).toBe(1300);
    expect(timeline.on_follow_change).not.toHaveBeenCalled();
  });

  it("preserves the visible row offset when a React update changes content above history", () => {
    const timeline = mountTimeline();
    timeline.readAt(350);

    timeline.layout.row("row-0").height += 80;
    for (const row of timeline.layout.rows.slice(1)) row.top += 80;
    timeline.layout.height += 80;
    timeline.commit();

    expect(timeline.viewport.scrollTop).toBe(430);
    expect(timeline.layout.row("row-1").top - timeline.viewport.scrollTop).toBe(-150);
    fireEvent.scroll(timeline.viewport);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
  });

  it("preserves the visible row when content above it grows without a React commit", () => {
    const timeline = mountTimeline();
    timeline.readAt(450);

    timeline.layout.row("row-0").height += 125;
    for (const row of timeline.layout.rows.slice(1)) row.top += 125;
    timeline.layout.height += 125;
    timeline.resize();

    expect(timeline.viewport.scrollTop).toBe(575);
    expect(timeline.layout.row("row-2").top - timeline.viewport.scrollTop).toBe(-50);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
  });

  it("keeps reading mode after shrinking content clamps the viewport to the bottom", () => {
    const timeline = mountTimeline();
    timeline.readAt(600);
    timeline.layout.height = 650;
    timeline.layout.rows.forEach((row, index) => {
      row.top = index < 3 ? index * 150 : 450 + (index - 3) * 100;
      row.height = index < 3 ? 150 : 100;
    });

    // Native scroll clamping and its event happen before the observer callback.
    expect(timeline.viewport.scrollTop).toBe(450);
    fireEvent.scroll(timeline.viewport);
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(450);

    timeline.layout.height = 1000;
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(450);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
  });

  it("anchors a visible nested row inside a trajectory spanning several screens", () => {
    const layout = new TimelineLayout();
    layout.height = 1800;
    layout.rows = [{
      key: "trajectory",
      top: 0,
      height: 1800,
      children: [
        { key: "tool-before", top: 200, height: 400 },
        { key: "visible-message", top: 600, height: 100 },
        { key: "next-message", top: 700, height: 300 },
      ],
    }];
    const timeline = mountTimeline(layout);
    timeline.readAt(650);

    layout.row("tool-before").height += 120;
    layout.row("visible-message").top += 120;
    layout.row("next-message").top += 120;
    layout.row("trajectory").height += 120;
    layout.height += 120;
    timeline.resize();

    expect(timeline.viewport.scrollTop).toBe(770);
    expect(layout.row("visible-message").top - timeline.viewport.scrollTop).toBe(-50);
  });

  it("bounds layout reads inside a long turn and falls back when its anchored child is hidden", () => {
    const layout = new TimelineLayout();
    layout.height = 20_000;
    const children = Array.from({ length: 500 }, (_, i) => ({ key: `child-${i}`, top: i * 40, height: 40, hidden: false }));
    layout.rows = [{ key: "turn", top: 0, height: layout.height, children }];
    const timeline = mountTimeline(layout);
    timeline.readAt(7900);
    layout.row_measurements = 0;
    timeline.readAt(8045);
    expect(layout.row_measurements).toBeLessThan(25);

    // The primary anchor stays mounted behind a collapsed disclosure.
    children[201].hidden = true;
    for (const child of children.slice(202)) child.top -= 40;
    layout.height -= 40;
    layout.rows[0].height = layout.height;
    timeline.commit();
    expect(timeline.viewport.scrollTop).toBe(8005);
  });

  it("anchors the next visible child when the reading line is in a gap inside a trajectory", () => {
    const layout = new TimelineLayout();
    layout.height = 1800;
    layout.rows = [{
      key: "trajectory",
      top: 0,
      height: 1800,
      children: [
        { key: "tool-before", top: 200, height: 300 },
        { key: "visible-message", top: 600, height: 100 },
      ],
    }];
    const timeline = mountTimeline(layout);
    timeline.readAt(550);

    layout.row("tool-before").height += 120;
    layout.row("visible-message").top += 120;
    layout.row("trajectory").height += 120;
    layout.height += 120;
    timeline.resize();

    expect(timeline.viewport.scrollTop).toBe(670);
    expect(layout.row("visible-message").top - timeline.viewport.scrollTop).toBe(50);
  });

  it("uses the next visible sibling when the anchored child disappears", () => {
    const layout = new TimelineLayout();
    layout.height = 1800;
    layout.rows = [{
      key: "trajectory",
      top: 0,
      height: 1800,
      children: [
        { key: "removed-message", top: 600, height: 100 },
        { key: "next-message", top: 700, height: 300 },
      ],
    }];
    const timeline = mountTimeline(layout);
    timeline.readAt(650);

    layout.row("trajectory").children = [{ key: "next-message", top: 600, height: 300 }];
    layout.row("trajectory").height -= 100;
    layout.height -= 100;
    timeline.commit();

    expect(timeline.viewport.scrollTop).toBe(550);
    expect(layout.row("next-message").top - timeline.viewport.scrollTop).toBe(50);
  });

  it("compensates only for prepended history when new output arrives in the same commit", () => {
    const timeline = mountTimeline();
    timeline.readAt(450);
    for (const row of timeline.layout.rows) row.top += 200;
    timeline.layout.rows.unshift({ key: "older-history", top: 0, height: 200 });
    timeline.layout.rows.push({ key: "new-output", top: 1200, height: 300 });
    timeline.layout.height += 500;
    timeline.commit();

    expect(timeline.viewport.scrollTop).toBe(650);
    expect(timeline.layout.row("row-2").top - timeline.viewport.scrollTop).toBe(-50);
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(650);
  });

  it("resumes following after a downward gesture at the physical end with no scroll delta", () => {
    const timeline = mountTimeline();
    timeline.readAt(600);
    // A shortened card can clamp a paused reader to the physical bottom.
    timeline.layout.height = 650;
    fireEvent.scroll(timeline.viewport);
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(450);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
    fireEvent.wheel(timeline.viewport, { deltaY: 80 });
    fireEvent.scroll(timeline.viewport);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false], [true]]);
    timeline.layout.height = 1000;
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(800);
  });

  it("jumps immediately with unchanged data and resumes following future output", () => {
    const timeline = mountTimeline();
    timeline.readAt(300);
    timeline.jump();

    expect(timeline.viewport.scrollTop).toBe(800);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false], [true]]);
    fireEvent.scroll(timeline.viewport);
    timeline.layout.height += 200;
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(1000);
  });

  it("handles viewport resizing in both follow and history modes", () => {
    const timeline = mountTimeline();
    timeline.layout.viewport_height = 300;
    timeline.resize(timeline.viewport);
    expect(timeline.viewport.scrollTop).toBe(700);

    timeline.readAt(450);
    timeline.layout.viewport_height = 250;
    timeline.resize(timeline.viewport);
    expect(timeline.viewport.scrollTop).toBe(450);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
  });

  it("respects an upward user scroll while new output changes the geometry", () => {
    const timeline = mountTimeline();
    timeline.layout.height += 200;
    fireEvent.wheel(timeline.viewport, { deltaY: -60 });
    timeline.viewport.scrollTop = 740;
    fireEvent.scroll(timeline.viewport);
    timeline.resize();

    expect(timeline.viewport.scrollTop).toBe(740);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
    timeline.layout.height += 100;
    timeline.commit();
    expect(timeline.viewport.scrollTop).toBe(740);
  });

  it("keeps following paused after a small upward gesture within the bottom threshold", () => {
    const timeline = mountTimeline();
    timeline.readAt(780);
    advanceAnimationFrame();

    timeline.layout.height += 200;
    timeline.commit();
    expect(timeline.viewport.scrollTop).toBe(780);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
  });

  it.each([false, true])("does not mistake shrink clamping for movement after no-op input (frame elapsed: %s)", (frame_elapsed) => {
    const timeline = mountTimeline();
    timeline.readAt(600);
    fireEvent.wheel(timeline.viewport, { deltaY: 100 });
    if (frame_elapsed) advanceAnimationFrame();

    timeline.layout.height = 650;
    timeline.layout.rows.forEach((row, index) => {
      row.top = index < 3 ? index * 150 : 450 + (index - 3) * 100;
      row.height = index < 3 ? 150 : 100;
    });
    fireEvent.scroll(timeline.viewport);
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(450);

    timeline.layout.height = 1000;
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(450);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
  });

  it("accepts native user movement before a React commit even if its scroll event is delayed", () => {
    const timeline = mountTimeline();
    timeline.layout.height += 200;
    fireEvent.wheel(timeline.viewport, { deltaY: -60 });
    timeline.viewport.scrollTop = 740;
    timeline.commit();

    expect(timeline.viewport.scrollTop).toBe(740);
    fireEvent.scroll(timeline.viewport);
    timeline.resize();
    expect(timeline.viewport.scrollTop).toBe(740);
    expect(timeline.on_follow_change.mock.calls).toEqual([[false]]);
  });
});
