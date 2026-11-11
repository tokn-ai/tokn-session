import { type MouseEvent, type ReactNode, useEffect, useId, useRef } from "react";
import { createPortal } from "react-dom";
import { ActionNotificationHost } from "./ActionNotifications";
import { CloseIcon } from "./Icons";
import "./HubPairingDialog.css";

function isBackdrop(event: MouseEvent<HTMLDialogElement>): boolean {
  if (event.target !== event.currentTarget) return false;
  const bounds = event.currentTarget.getBoundingClientRect();
  return event.clientX < bounds.left || event.clientX > bounds.right || event.clientY < bounds.top || event.clientY > bounds.bottom;
}

export function HubPairingDialog({ children, on_close }: { children: ReactNode; on_close: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const backdrop_pressed = useRef(false);
  const title_id = useId();
  useEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const element = dialog.current;
    element?.showModal();
    return () => {
      element?.close();
      if (opener?.isConnected) opener.focus({ preventScroll: true });
    };
  }, []);
  return createPortal(<dialog ref={dialog} className="hub-pair-dialog" aria-labelledby={title_id}
    onCancel={(event) => { event.preventDefault(); on_close(); }}
    onPointerDown={(event) => { backdrop_pressed.current = event.button === 0 && isBackdrop(event); }}
    onPointerCancel={() => { backdrop_pressed.current = false; }}
    onClick={(event) => {
      const started_outside = backdrop_pressed.current;
      backdrop_pressed.current = false;
      if (started_outside && event.button === 0 && isBackdrop(event)) on_close();
    }}>
    <header><h2 id={title_id}>Add machine</h2>
      <button type="button" className="icon-button" aria-label="Close Add machine" onClick={on_close}><CloseIcon /></button>
    </header>
    <div className="hub-pair-dialog__body">{children}</div>
    <ActionNotificationHost />
  </dialog>, document.body);
}
