import { createContext, useCallback, useContext, useEffect, useRef, useState, type ComponentProps } from "react";
import { createUuid } from "../lib/id";

type HubPath = "/" | "/account" | "/admin";
export const HubNavigation = createContext<((path: HubPath) => void) | undefined>(undefined);

/** Keep regular anchor behavior for new tabs and when rendered outside the Hub shell. */
export function HubPageLink({ href, onClick, ...props }: Omit<ComponentProps<"a">, "href"> & { href: HubPath }) {
  const navigate = useContext(HubNavigation);
  return <a {...props} href={href} onClick={(event) => {
    onClick?.(event);
    if (!navigate || event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey ||
      event.shiftKey || event.altKey || (props.target && props.target !== "_self") || props.download != null) return;
    event.preventDefault();
    navigate(href);
  }} />;
}

function readPage() {
  const { pathname, search } = window.location;
  const page = pathname === "/account" ? "profile" : pathname === "/admin" ? "admin" : "machines";
  return { page, search, entry: window.history.state?.tokn_machine_view as unknown };
}

export function useHubNavigation() {
  const [route, setRoute] = useState(readPage);
  // History can outlive a document. Only this tab's current warm machine can
  // resolve a history entry, so Back/Forward never initiates authentication.
  const machine = useRef<{ host_id: string; entry: string } | undefined>(undefined);
  useEffect(() => {
    const pop = () => setRoute(readPage());
    window.addEventListener("popstate", pop);
    return () => window.removeEventListener("popstate", pop);
  }, []);

  const navigate = useCallback((path: HubPath) => {
    if (location.pathname === path && !location.search && !location.hash && !history.state?.tokn_machine_view) return;
    history.pushState(null, "", path);
    setRoute(readPage());
  }, []);

  const showMachine = useCallback((host_id: string | null) => {
    if (!host_id) { navigate("/"); return; }
    if (machine.current?.host_id !== host_id) machine.current = { host_id, entry: createUuid() };
    if (location.pathname === "/" && history.state?.tokn_machine_view === machine.current.entry) return;
    history.pushState({ tokn_machine_view: machine.current.entry }, "", "/");
    setRoute(readPage());
  }, [navigate]);
  const current = machine.current;
  const host_id = route.page === "machines" && current && current.entry === route.entry ? current.host_id : null;
  return { ...route, host_id, navigate, showMachine };
}
