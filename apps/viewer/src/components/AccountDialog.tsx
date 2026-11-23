import { useId, useLayoutEffect, useRef, type MouseEvent, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import { CloseIcon } from "./Icons";
import "./AccountDialog.css";

export type AccountFailure = (error: unknown, report: (message: string) => void) => void;

function isBackdrop(event: MouseEvent<HTMLDialogElement>): boolean {
  if (event.target !== event.currentTarget) return false;
  const bounds = event.currentTarget.getBoundingClientRect();
  return event.clientX < bounds.left || event.clientX > bounds.right || event.clientY < bounds.top || event.clientY > bounds.bottom;
}

export function AccountDialog({ title, description, children, busy = false, on_close, return_focus, initial_focus }: {
  title: string;
  description?: string;
  children: ReactNode;
  busy?: boolean;
  on_close: () => void;
  return_focus: RefObject<HTMLElement | null>;
  initial_focus?: RefObject<HTMLElement | null>;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const backdrop_pressed = useRef(false);
  const title_id = useId();
  const description_id = useId();
  useLayoutEffect(() => {
    const element = dialog.current;
    element?.showModal();
    initial_focus?.current?.focus();
    return () => {
      element?.close();
      if (return_focus.current?.isConnected) return_focus.current.focus({ preventScroll: true });
    };
  }, [initial_focus, return_focus]);

  function close() { if (!busy) on_close(); }

  return createPortal(<dialog ref={dialog} className="account-dialog" aria-labelledby={title_id}
    aria-describedby={description ? description_id : undefined}
    onCancel={(event) => { event.preventDefault(); close(); }}
    onPointerDown={(event) => { backdrop_pressed.current = event.button === 0 && isBackdrop(event); }}
    onPointerCancel={() => { backdrop_pressed.current = false; }}
    onClick={(event) => {
      const started_outside = backdrop_pressed.current;
      backdrop_pressed.current = false;
      if (started_outside && event.button === 0 && isBackdrop(event)) close();
    }}>
    <header className="account-dialog-header">
      <div><h2 id={title_id}>{title}</h2>{description && <p id={description_id}>{description}</p>}</div>
      <button type="button" className="account-dialog-close" aria-label={`Close ${title}`} disabled={busy} onClick={close}><CloseIcon /></button>
    </header>
    <div className="account-dialog-body">{children}</div>
  </dialog>, document.body);
}
