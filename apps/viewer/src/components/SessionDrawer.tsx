import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";

interface SessionDrawerProps {
  desktop_hidden?: boolean;
  is_open: boolean;
  on_close: () => void;
  children: ReactNode;
}

// A native modal keeps focus and pointer interaction inside the mobile drawer.
// On desktop the same mounted sidebar remains a normal, non-modal panel.
export function SessionDrawer({ desktop_hidden = false, is_open, on_close, children }: SessionDrawerProps) {
  const [compact, setCompact] = useState(() => window.matchMedia("(max-width: 860px)").matches);
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const query = window.matchMedia("(max-width: 860px)");
    const update = () => setCompact(query.matches);
    query.addEventListener("change", update);
    update();
    return () => query.removeEventListener("change", update);
  }, []);

  useLayoutEffect(() => {
    const panel = dialog.current;
    if (!panel) return;
    if (!compact) {
      panel.setAttribute("open", "");
      return () => panel.removeAttribute("open");
    }
    if (!is_open) return;
    const previous = document.activeElement;
    panel.showModal();
    panel.querySelector<HTMLInputElement>('input[type="search"]')?.focus();
    return () => {
      panel.close();
      if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
    };
  }, [compact, is_open]);

  return (
    <dialog
      aria-label="Sessions"
      aria-hidden={!compact && desktop_hidden ? true : undefined}
      inert={!compact && desktop_hidden}
      className="sidebar-shell"
      onCancel={(event) => { event.preventDefault(); on_close(); }}
      onClick={(event) => { if (compact && event.target === event.currentTarget) on_close(); }}
      ref={dialog}
    >
      {children}
    </dialog>
  );
}
