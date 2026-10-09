import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { SessionDrawer } from "./SessionDrawer";

afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("removes a collapsed desktop sidebar from keyboard navigation while preserving its contents", () => {
  const view = (hidden: boolean) => <SessionDrawer desktop_hidden={hidden} is_open={false} on_close={vi.fn()}>
    <input aria-label="Search sessions" type="search" defaultValue="saved search" />
  </SessionDrawer>;
  const { rerender } = render(view(false));
  const field = screen.getByRole("searchbox");
  rerender(view(true));
  expect(field.closest("dialog")).toHaveAttribute("inert");
  expect(screen.queryByRole("searchbox")).not.toBeInTheDocument();
  rerender(view(false));
  expect(screen.getByRole("searchbox")).toBe(field);
  expect(field).toHaveValue("saved search");
});

it("focuses mobile search, dismisses on Escape, and preserves sidebar state across resizing", () => {
  let matches = true;
  let resize = () => {};
  vi.stubGlobal("matchMedia", () => ({
    get matches() { return matches; },
    addEventListener: (_: string, listener: () => void) => { resize = listener; },
    removeEventListener: vi.fn(),
  }));
  const close = vi.fn();
  const view = (is_open: boolean) => <SessionDrawer desktop_hidden is_open={is_open} on_close={close}>
    <input aria-label="Search sessions" type="search" defaultValue="saved search" />
  </SessionDrawer>;
  const { rerender } = render(view(false));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  rerender(view(true));
  expect(screen.getByRole("searchbox")).toHaveFocus();
  fireEvent(screen.getByRole("dialog"), new Event("cancel", { cancelable: true }));
  expect(close).toHaveBeenCalledOnce();
  rerender(view(false));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  act(() => { matches = false; resize(); });
  expect(screen.queryByRole("searchbox")).not.toBeInTheDocument();
  expect(screen.getByRole("dialog", { hidden: true })).toHaveAttribute("inert");
  act(() => { matches = true; resize(); });
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});
