import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { HubAccountMenu } from "./HubAccountMenu";

vi.mock("../lib/useMachineSettings", () => ({ useMachineSettings: () => ({ settings: null, loading: false, error: null, retry: vi.fn(), applyReader: vi.fn() }) }));
vi.mock("../lib/viewerPreferences", () => ({ useViewerPreferences: () => ({ preferences: {}, updatePreferences: vi.fn(), resetPreferences: vi.fn() }) }));
vi.mock("../pages/SettingsPage", () => ({
  SettingsPage: ({ on_close, machine_label, machine_key, machine_connected }: {
    on_close: () => void; machine_label: string; machine_key: string; machine_connected: boolean;
  }) => <><p>{machine_label} · {machine_key} · {String(machine_connected)}</p><button onClick={on_close}>Close settings</button></>,
}));
vi.mock("./MachineHostingSettings", () => ({ MachineHostingSettings: () => null }));
vi.mock("./NativePreferenceSaveNotice", () => ({ NativePreferenceSaveNotice: () => null }));
vi.mock("./ActionNotifications", () => ({ ActionNotificationHost: () => null }));

afterEach(cleanup);

function trigger() {
  return screen.getByRole("button", { name: "Account menu for alice" });
}

it("opens from the keyboard, navigates items and returns focus on Escape", () => {
  render(<HubAccountMenu username="alice" signing_out={false} on_sign_out={vi.fn()} profile_active />);
  fireEvent.keyDown(trigger(), { key: "ArrowUp" });
  const profile = screen.getByRole("menuitem", { name: "Profile" });
  const settings = screen.getByRole("menuitem", { name: "Settings" });
  const sign_out = screen.getByRole("menuitem", { name: "Sign out" });
  expect(trigger()).toHaveAttribute("aria-expanded", "true");
  expect(sign_out).toHaveFocus();
  expect(profile).toHaveAttribute("href", "/account");
  expect(profile).toHaveAttribute("aria-current", "page");
  fireEvent.keyDown(sign_out, { key: "ArrowDown" });
  expect(profile).toHaveFocus();
  fireEvent.keyDown(profile, { key: "End" });
  expect(sign_out).toHaveFocus();
  fireEvent.keyDown(sign_out, { key: "Home" });
  expect(profile).toHaveFocus();
  fireEvent.keyDown(profile, { key: "s" });
  expect(settings).toHaveFocus();
  fireEvent.keyDown(settings, { key: "Escape" });
  expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  expect(trigger()).toHaveFocus();
});

it("dismisses for outside pointers, outside focus and Tab without trapping focus", () => {
  render(<><HubAccountMenu username="alice" signing_out={false} on_sign_out={vi.fn()} /><button>Outside</button></>);
  fireEvent.click(trigger());
  fireEvent.pointerDown(screen.getByRole("button", { name: "Outside" }));
  expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  fireEvent.click(trigger());
  act(() => screen.getByRole("button", { name: "Outside" }).focus());
  expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Outside" })).toHaveFocus();
  fireEvent.click(trigger());
  expect(fireEvent.keyDown(screen.getByRole("menuitem", { name: "Profile" }), { key: "Tab" })).toBe(true);
  expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  expect(trigger()).toHaveFocus();
});

it("opens disconnected Settings and returns to the username trigger when it closes", () => {
  render(<HubAccountMenu username="alice" signing_out={false} on_sign_out={vi.fn()} />);
  fireEvent.click(trigger());
  fireEvent.click(screen.getByRole("menuitem", { name: "Settings" }));
  expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  expect(screen.getByRole("dialog", { name: "Settings" })).toHaveTextContent("No machine selected · none · false");
  fireEvent.click(screen.getByRole("button", { name: "Close settings" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(trigger()).toHaveFocus();
});

it("preserves sign-out feedback after dismissal and permits retry after failure", () => {
  const on_sign_out = vi.fn();
  const view = render(<HubAccountMenu username="alice" signing_out={false} on_sign_out={on_sign_out} />);
  fireEvent.click(trigger());
  fireEvent.click(screen.getByRole("menuitem", { name: "Sign out" }));
  expect(on_sign_out).toHaveBeenCalledTimes(1);
  view.rerender(<HubAccountMenu username="alice" signing_out on_sign_out={on_sign_out} />);
  expect(screen.getByRole("menuitem", { name: "Signing out…" })).toBeDisabled();
  fireEvent.keyDown(screen.getByRole("menuitem", { name: "Settings" }), { key: "Escape" });
  expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  expect(screen.getByRole("status")).toHaveTextContent("Signing out…");
  view.rerender(<HubAccountMenu username="alice" signing_out={false} sign_out_error="Try again." on_sign_out={on_sign_out} />);
  expect(screen.getByRole("alert")).toHaveTextContent("Try again.");
  fireEvent.click(trigger());
  fireEvent.click(screen.getByRole("menuitem", { name: "Sign out" }));
  expect(on_sign_out).toHaveBeenCalledTimes(2);
});
