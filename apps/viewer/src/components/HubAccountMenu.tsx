import { HubPageLink } from "./HubNavigation";
import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { ChevronIcon, SettingsIcon } from "./Icons";
import { SettingsDialog } from "./SettingsDialog";
import "./HubAccountMenu.css";

interface HubAccountMenuProps {
  username: string;
  signing_out: boolean;
  sign_out_error?: string;
  on_sign_out: () => void;
  profile_active?: boolean;
}

export function HubAccountMenu({ username, signing_out, sign_out_error, on_sign_out, profile_active = false }: HubAccountMenuProps) {
  const [open, setOpen] = useState(false);
  const [settings_open, setSettingsOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const initial_focus = useRef<"first" | "last">("first");
  const trigger_id = useId();
  const menu_id = useId();

  function items() {
    return [...(menu.current?.querySelectorAll<HTMLElement>('[role="menuitem"]:not(:disabled)') ?? [])];
  }

  function closeMenu(restore_focus = false) {
    setOpen(false);
    if (restore_focus) trigger.current?.focus({ preventScroll: true });
  }

  function openMenu(position: "first" | "last" = "first") {
    initial_focus.current = position;
    if (open) {
      const choices = items();
      choices[position === "first" ? 0 : choices.length - 1]?.focus();
    } else setOpen(true);
  }

  useLayoutEffect(() => {
    if (!open) return;
    const choices = items();
    choices[initial_focus.current === "first" ? 0 : choices.length - 1]?.focus({ preventScroll: true });
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const outside = (event: Event) => {
      if (event.target instanceof Node && !root.current?.contains(event.target)) setOpen(false);
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("focusin", outside);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("focusin", outside);
    };
  }, [open]);

  function navigate(event: KeyboardEvent<HTMLDivElement>) {
    const choices = items();
    const current = choices.indexOf(document.activeElement as HTMLElement);
    let next: number | undefined;
    if (event.key === "ArrowDown") next = (current + 1) % choices.length;
    else if (event.key === "ArrowUp") next = (current - 1 + choices.length) % choices.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = choices.length - 1;
    else if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      closeMenu(true);
      return;
    } else if (event.key === "Tab") {
      // Let the browser continue from the trigger to the next page control.
      closeMenu(true);
      return;
    } else if (event.key === " " && event.target instanceof HTMLAnchorElement) {
      event.preventDefault();
      event.target.click();
      return;
    } else if (event.key.length === 1 && event.key.trim() && !event.altKey && !event.ctrlKey && !event.metaKey) {
      const ordered = [...choices.slice(current + 1), ...choices.slice(0, current + 1)];
      const match = ordered.find((item) => item.textContent?.trim().toLowerCase().startsWith(event.key.toLowerCase()));
      if (match) { event.preventDefault(); match.focus(); }
      return;
    }
    if (next !== undefined) {
      event.preventDefault();
      choices[next]?.focus();
    }
  }

  const feedback = signing_out ? <p className="hub-account-menu__feedback" role="status">Signing out…</p>
    : sign_out_error ? <p className="hub-account-menu__feedback hub-account-menu__error" role="alert">{sign_out_error}</p> : null;

  return <div className="hub-account-menu" ref={root}>
    <button ref={trigger} id={trigger_id} className="hub-account-menu__trigger" type="button"
      aria-label={`Account menu for ${username}`} aria-haspopup="menu" aria-expanded={open} aria-controls={open ? menu_id : undefined}
      onClick={() => { if (open) closeMenu(); else openMenu(); }}
      onKeyDown={(event) => {
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          event.preventDefault();
          openMenu(event.key === "ArrowUp" ? "last" : "first");
        } else if (event.key === "Escape" && open) {
          event.preventDefault();
          closeMenu(true);
        }
      }}>
      <span className="hub-account-menu__avatar" aria-hidden="true">{Array.from(username)[0]?.toUpperCase()}</span>
      <span className="hub-account-menu__username">{username}</span>
      <ChevronIcon className="hub-account-menu__chevron" />
    </button>
    {(open || feedback) && <div className="hub-account-menu__panel">
      {open && <div ref={menu} id={menu_id} role="menu" aria-labelledby={trigger_id} onKeyDown={navigate}>
        <HubPageLink className="hub-account-menu__item" href="/account" role="menuitem" tabIndex={-1}
          aria-current={profile_active ? "page" : undefined} onClick={() => closeMenu()}>
          <svg aria-hidden="true" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round">
            <circle cx="12" cy="8" r="3.5" /><path d="M5 20v-2a7 7 0 0 1 14 0v2" />
          </svg>Profile
        </HubPageLink>
        <button className="hub-account-menu__item" type="button" role="menuitem" tabIndex={-1} aria-haspopup="dialog"
          onClick={() => { closeMenu(); setSettingsOpen(true); }}><SettingsIcon />Settings</button>
        <div className="hub-account-menu__divider" role="separator" />
        <button className="hub-account-menu__item hub-account-menu__sign-out" type="button" role="menuitem" tabIndex={-1}
          disabled={signing_out} onClick={on_sign_out}>
          <svg aria-hidden="true" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
            <path d="M10 4H5v16h5M9 12h12m-4-4 4 4-4 4" />
          </svg>{signing_out ? "Signing out…" : "Sign out"}
        </button>
      </div>}
      {feedback}
    </div>}
    {settings_open && <SettingsDialog remote connected={false} machine_label="No machine selected" machine_key="none"
      return_focus={trigger} on_close={() => setSettingsOpen(false)} />}
  </div>;
}
