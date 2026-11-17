import { useEffect, useRef, type RefObject } from "react";
import { useViewerVisible } from "./viewerVisibility";

/** Shared dismissal and focus behavior for non-modal status panels. */
export function useFloatingPanel(
  is_open: boolean,
  on_close: () => void,
  trigger_ref: RefObject<HTMLButtonElement | null>,
) {
  const visible = useViewerVisible();
  const panel_ref = useRef<HTMLDivElement>(null);
  const had_focus = useRef(false);
  useEffect(() => {
    if (!visible) {
      had_focus.current = false;
      return;
    }
    if (is_open) {
      had_focus.current = true;
      panel_ref.current?.focus();
    } else if (had_focus.current) {
      had_focus.current = false;
      trigger_ref.current?.focus();
    }
  }, [is_open, visible, trigger_ref]);

  useEffect(() => {
    if (!is_open) return;
    if (!visible) { on_close(); return; }
    function outside(target: EventTarget | null) {
      return target instanceof Node
        && !panel_ref.current?.contains(target) && !trigger_ref.current?.contains(target);
    }
    function onPointerDown(event: PointerEvent) {
      if (outside(event.target)) on_close();
    }
    function onFocusIn(event: FocusEvent) {
      if (outside(event.target)) {
        // Tab may leave a non-modal panel; keep focus where the user moved it.
        had_focus.current = false;
        on_close();
      }
    }
    function onKeyDown(event: KeyboardEvent) {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      on_close();
    }
    document.addEventListener("pointerdown", onPointerDown, true);
    document.addEventListener("focusin", onFocusIn, true);
    document.addEventListener("keydown", onKeyDown, true);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown, true);
      document.removeEventListener("focusin", onFocusIn, true);
      document.removeEventListener("keydown", onKeyDown, true);
    };
  }, [visible, is_open, on_close, trigger_ref]);
  return panel_ref;
}
