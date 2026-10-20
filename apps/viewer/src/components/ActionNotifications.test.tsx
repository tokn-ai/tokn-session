import { act, cleanup, fireEvent, render, renderHook, screen, within } from "@testing-library/react";
import { StrictMode, useLayoutEffect, useRef } from "react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { ActionNotificationHost, ActionNotificationsProvider, useActionNotification } from "./ActionNotifications";
import { HostDialog } from "./HostSetup";
import { RegisterProjectDialog } from "./ProjectWorkspace";
import { SessionDrawer } from "./SessionDrawer";
import { LocalHostContext, type LocalHostControls } from "../lib/localHost";

function Actions() {
  const notify = useActionNotification();
  return <><input aria-label="Conversation draft" />
    <button type="button" onClick={() => notify({ title: "Restart requested", message: "Pi is restarting." })}>Notify first</button>
    <button type="button" onClick={() => notify({ title: "Reader restarted", message: "Live updates are reconnecting." })}>Notify second</button>
  </>;
}
function ModalHost() {
  const dialog = useRef<HTMLDialogElement>(null);
  useLayoutEffect(() => { dialog.current?.showModal(); }, []);
  return <dialog aria-label="Settings" ref={dialog}><p>Runtime controls</p><ActionNotificationHost /></dialog>;
}
function Surface({ modal = false }: { modal?: boolean }) {
  return <ActionNotificationsProvider><Actions />{modal && <ModalHost />}</ActionNotificationsProvider>;
}
function advance(ms: number) { act(() => { vi.advanceTimersByTime(ms); }); }
beforeEach(() => { vi.useFakeTimers(); });
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("announces success politely and supports expiry and explicit dismissal", () => {
  render(<Surface />);
  fireEvent.click(screen.getByRole("button", { name: "Notify first" }));
  const notice = screen.getByRole("status");
  expect(notice).toHaveAttribute("aria-live", "polite");
  expect(notice).toHaveTextContent("Restart requested");
  expect(notice).toHaveTextContent("Pi is restarting.");
  advance(4999);
  expect(screen.getByRole("status")).toBeInTheDocument();
  advance(1);
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Notify second" }));
  fireEvent.click(screen.getByRole("button", { name: "Dismiss notification" }));
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
  expect(vi.getTimerCount()).toBe(0);
});

it("replaces the current notice and gives its replacement a fresh lifetime", () => {
  render(<Surface />);
  fireEvent.click(screen.getByRole("button", { name: "Notify first" }));
  advance(4000);
  fireEvent.click(screen.getByRole("button", { name: "Notify second" }));
  expect(screen.getAllByRole("status")).toHaveLength(1);
  expect(screen.queryByText("Restart requested")).not.toBeInTheDocument();
  advance(1000);
  expect(screen.getByRole("status")).toHaveTextContent("Reader restarted");
  advance(3999);
  expect(screen.getByRole("status")).toBeInTheDocument();
  advance(1);
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("pauses expiry while hovered and resumes only the remaining lifetime", () => {
  render(<Surface />);
  fireEvent.click(screen.getByRole("button", { name: "Notify first" }));
  advance(2000);
  fireEvent.mouseEnter(screen.getByRole("status"));
  advance(10_000);
  expect(screen.getByRole("status")).toBeInTheDocument();
  fireEvent.mouseLeave(screen.getByRole("status"));
  advance(2999);
  expect(screen.getByRole("status")).toBeInTheDocument();
  advance(1);
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("does not steal focus and pauses expiry during explicit keyboard focus", () => {
  render(<Surface />);
  const draft = screen.getByRole("textbox", { name: "Conversation draft" });
  act(() => { draft.focus(); });
  fireEvent.click(screen.getByRole("button", { name: "Notify first" }));
  expect(draft).toHaveFocus();
  const close = screen.getByRole("button", { name: "Dismiss notification" });
  act(() => { close.focus(); });
  advance(10_000);
  expect(screen.getByRole("status")).toBeInTheDocument();
  act(() => { draft.focus(); });
  advance(4999);
  expect(screen.getByRole("status")).toBeInTheDocument();
  advance(1);
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
  expect(draft).toHaveFocus();
});

it("renders inside the native modal host and returns its pending notice to the page on close", () => {
  const view = render(<Surface modal />);
  const dialog = screen.getByRole("dialog", { name: "Settings" });
  expect(dialog).toHaveAttribute("open");
  fireEvent.click(screen.getByRole("button", { name: "Notify first" }));
  expect(within(dialog).getByRole("status")).toHaveAttribute("data-placement", "dialog");
  advance(2000);
  fireEvent.mouseEnter(screen.getByRole("status"));
  advance(10_000);
  view.rerender(<Surface />);
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.getByRole("status")).toHaveAttribute("data-placement", "page");
  expect(screen.getByRole("status")).toHaveTextContent("Restart requested");
  advance(2999);
  expect(screen.getByRole("status")).toBeInTheDocument();
  advance(1);
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("moves an existing notice into a newly opened modal without resetting expiry", () => {
  const view = render(<Surface />);
  fireEvent.click(screen.getByRole("button", { name: "Notify first" }));
  advance(2000);
  view.rerender(<Surface modal />);
  const dialog = screen.getByRole("dialog", { name: "Settings" });
  expect(within(dialog).getByRole("status")).toHaveTextContent("Restart requested");
  advance(3000);
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("retains an initially mounted modal host through StrictMode effect replay", () => {
  render(<StrictMode><Surface modal /></StrictMode>);
  fireEvent.click(screen.getByRole("button", { name: "Notify first" }));
  expect(within(screen.getByRole("dialog", { name: "Settings" })).getByRole("status"))
    .toHaveAttribute("data-placement", "dialog");
  advance(5000);
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});

it("uses only open modal surfaces and falls back through the remaining dialog stack", () => {
  let compact = false;
  let resize = () => {};
  vi.stubGlobal("matchMedia", () => ({
    get matches() { return compact; },
    addEventListener: (_: string, listener: () => void) => { resize = listener; },
    removeEventListener: vi.fn(),
  }));
  const hosting: LocalHostControls = {
    status: { phase: "stopped", hub_url: "", name: "This computer", allow_control: false, machine_reference: null, error: null },
    busy: false, is_open: true, open: vi.fn(), close: vi.fn(), retry: vi.fn(),
    change: vi.fn(), stop_external: vi.fn(), remove_external: vi.fn(),
  };
  const surface = ({ settings = true, project = false, host = false, drawer = true } = {}) =>
    <ActionNotificationsProvider><Actions />
      {settings && <ModalHost />}
      <RegisterProjectDialog open={project} on_close={vi.fn()} on_register={vi.fn()} on_registered={vi.fn()} />
      <LocalHostContext value={hosting}>{host && <HostDialog />}</LocalHostContext>
      <SessionDrawer is_open={drawer} on_close={vi.fn()}><p>Saved sessions</p></SessionDrawer>
    </ActionNotificationsProvider>;
  const expectNoticeIn = (name: string) => expect(within(screen.getByRole("dialog", { name })).getByText("Restart requested")).toBeInTheDocument();
  const view = render(surface());
  fireEvent.click(screen.getByRole("button", { name: "Notify first" }));
  expectNoticeIn("Settings"); // The mounted hidden project dialog and desktop drawer cannot claim it.
  view.rerender(surface({ project: true }));
  expectNoticeIn("Add project");
  view.rerender(surface({ project: true, drawer: false }));
  act(() => { compact = true; resize(); });
  expectNoticeIn("Add project"); // A closed compact drawer cannot claim it either.
  view.rerender(surface({ project: true, drawer: true }));
  expectNoticeIn("Sessions");
  view.rerender(surface({ project: true, host: true, drawer: true }));
  expectNoticeIn("Host this computer");
  view.rerender(surface({ project: true, drawer: true }));
  expectNoticeIn("Sessions");
  view.rerender(surface({ project: true, drawer: false }));
  expectNoticeIn("Add project");
  view.rerender(surface({ drawer: false }));
  expectNoticeIn("Settings");
  view.rerender(surface({ settings: false, drawer: false }));
  expect(screen.getByRole("status")).toHaveAttribute("data-placement", "page");
  expect(screen.getAllByText("Restart requested")).toHaveLength(1);
});

it("keeps the context callback stable and defaults to a harmless no-op outside the provider", () => {
  const hook = renderHook(() => useActionNotification(), { wrapper: ActionNotificationsProvider });
  const notify = hook.result.current;
  act(() => { notify({ title: "Done", message: "Restart accepted" }); });
  hook.rerender();
  expect(hook.result.current).toBe(notify);
  hook.unmount();
  expect(vi.getTimerCount()).toBe(0);
  act(() => { notify({ title: "Late result", message: "The old viewer has closed" }); });
  expect(vi.getTimerCount()).toBe(0);
  const isolated = renderHook(() => useActionNotification());
  expect(() => isolated.result.current({ title: "Done", message: "No provider" })).not.toThrow();
  expect(screen.queryByRole("status")).not.toBeInTheDocument();
});
