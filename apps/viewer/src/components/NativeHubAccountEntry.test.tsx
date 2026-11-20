import { StrictMode } from "react";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { NativeHubAccountEntry } from "./NativeHubAccountEntry";

afterEach(cleanup);

const session = { user_id: "550e8400-e29b-41d4-a716-446655440000", username: "alice" };
const challenge = {
  verification_url: "https://hub.example/account?user_code=ABCD-EFGH&device=viewer",
  user_code: "ABCD-EFGH",
};

function callbacks() {
  return { on_start_sign_in: vi.fn(), on_cancel_sign_in: vi.fn(), on_sign_out: vi.fn() };
}

it("shares the sign-in control and shows the approval details without relying on opening a browser", () => {
  const actions = callbacks();
  const view = render(<NativeHubAccountEntry {...actions} starting />);
  const trigger = screen.getByRole("button", { name: "Sign in" });
  fireEvent.click(trigger);
  const dialog = screen.getByRole("dialog", { name: "Sign in to Hub" });
  expect(actions.on_start_sign_in).toHaveBeenCalledOnce();
  expect(within(dialog).getByRole("status")).toHaveTextContent("Requesting an approval code…");

  view.rerender(<NativeHubAccountEntry {...actions} challenge={challenge} />);
  expect(within(dialog).getByText(challenge.verification_url)).toBeInTheDocument();
  expect(within(dialog).getByText(challenge.user_code)).toBeInTheDocument();
  fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
  expect(actions.on_cancel_sign_in).toHaveBeenCalledOnce();
  expect(trigger).toHaveFocus();
});

it("cancels a pending attempt against the latest owner when its header unmounts", () => {
  const actions = callbacks();
  const renderEntry = (on_cancel_sign_in: () => void) =>
    <StrictMode><NativeHubAccountEntry {...actions} on_cancel_sign_in={on_cancel_sign_in} /></StrictMode>;
  const view = render(renderEntry(actions.on_cancel_sign_in));
  fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  const latest_cancel = vi.fn();
  view.rerender(renderEntry(latest_cancel));
  expect(actions.on_cancel_sign_in).not.toHaveBeenCalled();
  expect(latest_cancel).not.toHaveBeenCalled();
  view.unmount();
  expect(latest_cancel).toHaveBeenCalledOnce();
});

it("reuses the signed-in menu and shows the shared profile without navigating outside the applet", () => {
  const actions = callbacks();
  const view = render(<NativeHubAccountEntry {...actions} challenge={challenge} />);
  fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  view.rerender(<NativeHubAccountEntry {...actions} session={session} />);
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(actions.on_cancel_sign_in).not.toHaveBeenCalled();
  const menu = screen.getByRole("button", { name: "Account menu for alice" });
  fireEvent.click(menu);
  fireEvent.click(screen.getByRole("menuitem", { name: "Profile" }));
  const profile = screen.getByRole("dialog", { name: "Profile" });
  expect(within(profile).getByText(session.user_id)).toBeInTheDocument();
  fireEvent.click(within(profile).getByRole("button", { name: "Close Profile" }));
  expect(menu).toHaveFocus();
  fireEvent.click(menu);
  fireEvent.click(screen.getByRole("menuitem", { name: "Sign out" }));
  expect(actions.on_sign_out).toHaveBeenCalledOnce();
  view.rerender(<NativeHubAccountEntry {...actions} />);
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});
