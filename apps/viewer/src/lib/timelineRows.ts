// Keyed siblings are successive blocks in reading order. A trajectory's keyed
// children form their own sequence inside its outer row.
interface TimelineRow {
  element: HTMLElement;
  children: TimelineRow[];
}

function indexRows(content: HTMLElement) {
  const roots: TimelineRow[] = [];
  const byKey = new Map<string, HTMLElement>();
  const byElement = new Map<Element, TimelineRow>();
  for (const element of content.querySelectorAll<HTMLElement>("[data-scroll-key]")) {
    // Collapsed work groups can keep hundreds of children mounted.
    const hidden = element.closest("[hidden]");
    if (hidden && content.contains(hidden)) continue;
    const parent = element.parentElement?.closest("[data-scroll-key]");
    const siblings = (parent && byElement.get(parent)?.children) || roots;
    const row = { element, children: [] };
    siblings.push(row);
    byElement.set(element, row);
    byKey.set(element.dataset.scrollKey!, element);
  }
  return { roots, byKey };
}

type RowIndex = ReturnType<typeof indexRows>;

function rowsChanged(records: MutationRecord[]) {
  return records.some((record) => record.type === "attributes"
    || [...record.addedNodes, ...record.removedNodes].some((node) => node instanceof Element
      && (node.hasAttribute("data-scroll-key") || node.querySelector("[data-scroll-key]"))));
}

export function createTimelineRowIndex() {
  let root: HTMLElement | null = null;
  let observer: MutationObserver | null = null;
  let index: RowIndex | null = null;
  function disconnect() {
    observer?.disconnect();
    observer = null;
    index = null;
    root = null;
  }
  return {
    get(content: HTMLElement): RowIndex {
      if (root !== content) {
        disconnect();
        root = content;
        if (typeof MutationObserver !== "undefined") {
          observer = new MutationObserver((records) => { if (rowsChanged(records)) index = null; });
          observer.observe(content, {
            subtree: true, childList: true, attributes: true, attributeFilter: ["hidden", "data-scroll-key"],
          });
        }
      }
      // React's layout effect can run before the mutation observer callback.
      if (!observer || rowsChanged(observer.takeRecords())) index = null;
      return index ??= indexRows(content);
    },
    disconnect,
  };
}

export function visibleTimelineRows(roots: TimelineRow[], viewport: DOMRect) {
  const visible: { element: HTMLElement; rect: DOMRect }[] = [];
  function visit(siblings: TimelineRow[]) {
    let first = 0;
    let end = siblings.length;
    while (first < end) {
      const middle = Math.floor((first + end) / 2);
      if (siblings[middle].element.getBoundingClientRect().bottom <= viewport.top) first = middle + 1;
      else end = middle;
    }
    for (let i = first; i < siblings.length; i++) {
      const row = siblings[i];
      const rect = row.element.getBoundingClientRect();
      if (rect.top >= viewport.bottom) break;
      if (rect.height > 0) {
        visible.push({ element: row.element, rect });
        if (row.children.length) visit(row.children);
      }
    }
  }
  visit(roots);
  return visible;
}
