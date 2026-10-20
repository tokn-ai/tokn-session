import { createContext, useCallback, useContext, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { CloseIcon } from "./Icons";
import "./ActionNotifications.css";

export type ActionNotification = { title: string; message: string };
interface PendingNotice extends ActionNotification { id: number; }
interface NotificationContext {
  notify: (notice: ActionNotification) => void;
  registerHost: (host: HTMLElement) => () => void;
}
const NOOP = () => {};
const Context = createContext<NotificationContext>({ notify: NOOP, registerHost: () => NOOP });
const DISMISS_AFTER_MS = 5000;

export function ActionNotificationsProvider({ children }: { children: ReactNode }) {
  const [notice, setNotice] = useState<PendingNotice | null>(null);
  const [host, setHost] = useState<HTMLElement | null>(null);
  const current = useRef<PendingNotice | null>(null);
  const hosts = useRef<HTMLElement[]>([]);
  const sequence = useRef(0);
  const mounted = useRef(true);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const remaining = useRef(DISMISS_AFTER_MS);
  const deadline = useRef<number | null>(null);
  const paused = useRef({ hover: false, focus: false });

  const clearTimer = useCallback(() => {
    if (timer.current !== null) clearTimeout(timer.current);
    timer.current = null;
    deadline.current = null;
  }, []);
  const stopClock = useCallback(() => {
    if (deadline.current !== null) remaining.current = Math.max(0, deadline.current - Date.now());
    clearTimer();
  }, [clearTimer]);
  const dismiss = useCallback(() => {
    clearTimer();
    current.current = null;
    paused.current = { hover: false, focus: false };
    if (mounted.current) setNotice(null);
  }, [clearTimer]);
  const startClock = useCallback(() => {
    if (!current.current || !mounted.current || paused.current.hover || paused.current.focus) return;
    clearTimer();
    const id = current.current.id;
    deadline.current = Date.now() + remaining.current;
    timer.current = setTimeout(() => { if (current.current?.id === id) dismiss(); }, remaining.current);
  }, [clearTimer, dismiss]);
  const pause = useCallback((reason: "hover" | "focus", value: boolean) => {
    if (paused.current[reason] === value) return;
    if (value && !paused.current.hover && !paused.current.focus) stopClock();
    paused.current[reason] = value;
    if (!paused.current.hover && !paused.current.focus) startClock();
  }, [startClock, stopClock]);

  const notify = useCallback((next: ActionNotification) => {
    if (!mounted.current) return;
    clearTimer();
    const pending = { title: next.title, message: next.message, id: ++sequence.current };
    current.current = pending;
    remaining.current = DISMISS_AFTER_MS;
    setNotice(pending);
    startClock();
  }, [clearTimer, startClock]);
  const registerHost = useCallback((target: HTMLElement) => {
    hosts.current.push(target);
    if (mounted.current) setHost(target);
    return () => {
      hosts.current = hosts.current.filter((candidate) => candidate !== target);
      if (mounted.current) setHost(hosts.current[hosts.current.length - 1] ?? null);
    };
  }, []);

  useLayoutEffect(() => {
    mounted.current = true;
    // Child hosts register before this layout effect, including StrictMode's
    // replay while the previous cleanup has marked the provider unmounted.
    setHost(hosts.current[hosts.current.length - 1] ?? null);
    if (timer.current === null) startClock();
    return () => { mounted.current = false; stopClock(); };
  }, [startClock, stopClock]);
  useLayoutEffect(() => {
    // Moving the same notice between a native dialog and the page preserves
    // its remaining lifetime. Old pointer/focus state belongs to the old node.
    stopClock();
    paused.current = { hover: false, focus: false };
    startClock();
  }, [host, startClock, stopClock]);
  const context = useMemo(() => ({ notify, registerHost }), [notify, registerHost]);

  return <Context.Provider value={context}>{children}{notice && createPortal(
    <div className="action-notification" data-placement={host ? "dialog" : "page"}
      role="status" aria-live="polite" aria-atomic="true"
      onMouseEnter={() => pause("hover", true)} onMouseLeave={() => pause("hover", false)}
      onFocusCapture={() => pause("focus", true)}
      onBlurCapture={(event) => { if (!(event.relatedTarget instanceof Node) || !event.currentTarget.contains(event.relatedTarget)) pause("focus", false); }}>
      <span className="action-notification__icon" aria-hidden="true">✓</span>
      <div className="action-notification__text"><strong>{notice.title}</strong><p>{notice.message}</p></div>
      <button className="action-notification__close" type="button" aria-label="Dismiss notification" onClick={dismiss}><CloseIcon /></button>
    </div>, host ?? document.body,
  )}</Context.Provider>;
}

export function useActionNotification(): (notice: ActionNotification) => void {
  return useContext(Context).notify;
}

/** Native showModal dialogs need their notification inside the top layer. */
export function ActionNotificationHost() {
  const target = useRef<HTMLDivElement>(null);
  const { registerHost } = useContext(Context);
  useLayoutEffect(() => {
    if (target.current) return registerHost(target.current);
  }, [registerHost]);
  return <div className="action-notification-host" ref={target} />;
}
