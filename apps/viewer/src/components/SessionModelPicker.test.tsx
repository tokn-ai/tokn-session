import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { SessionModelPicker } from "./SessionModelPicker";
import type { SessionModelController } from "../lib/useSessionModel";
import type { ModelIdentity, SessionModelState } from "../lib/types";

const first: ModelIdentity = { model_provider: "anthropic", model_id: "first", display_name: "First model" };
const second: ModelIdentity = { model_provider: "anthropic", model_id: "second", display_name: "Second model" };
const state: SessionModelState = { model: first, source: "live", revision: "one" };
function controller(overrides: Partial<SessionModelController> = {}): SessionModelController {
  return { status: { state, can_list: true, can_select: true, reason: null, runtime_generation: "pi:1", updates_pi_default: true },
    state, checking: false, loading: false, changing: false, uncertain: false, choices: null, error: null,
    reason: null, input_revision: 0, can_open: true, can_select: true,
    refresh: vi.fn(async () => {}), select: vi.fn(async () => {}), loadChoices: vi.fn(async () => {}), ...overrides };
}
afterEach(cleanup);

it("opens a lazy catalog above the composer, labels the Pi default consequence, and dismisses on Escape/outside", async () => {
  const model = controller();
  render(<><SessionModelPicker model={model} provider="Pi" /><button>Outside</button></>);
  expect(model.loadChoices).not.toHaveBeenCalled();
  const trigger = screen.getByRole("button", { name: /Model: First model/ });
  fireEvent.click(trigger);
  const dialog = screen.getByRole("dialog", { name: "Model" });
  expect(dialog).toHaveFocus();
  expect(model.loadChoices).toHaveBeenCalledTimes(1);
  expect(within(dialog).getByText(/updates Pi’s default/)).toBeVisible();
  fireEvent.keyDown(dialog, { key: "Escape" });
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(trigger).toHaveFocus();
  fireEvent.click(trigger);
  fireEvent.pointerDown(screen.getByRole("button", { name: "Outside" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});

it("renders verified model choices with the current checkmark and submits the exact selected identity", () => {
  const model = controller({ choices: [first, second].map((model) => ({ model, supports_images: model === second })) });
  render(<SessionModelPicker model={model} provider="Pi" />);
  fireEvent.click(screen.getByRole("button", { name: /Change model/ }));
  const first_choice = screen.getByRole("button", { name: /First model anthropic/ });
  expect(first_choice).toHaveAttribute("aria-pressed", "true");
  expect(first_choice).toBeDisabled();
  const next = screen.getByRole("button", { name: /Second model anthropic · Images/ });
  fireEvent.click(next);
  expect(model.select).toHaveBeenCalledExactlyOnceWith(second);
});

it("shows recorded/read-only reasons without fetching unverified models", () => {
  const model = controller({ can_open: false, can_select: false, state: { ...state, source: "recorded" }, reason: "Resume this Pi session before changing its model." });
  render(<SessionModelPicker model={model} provider="Pi" />);
  expect(screen.queryByRole("button", { name: /Change model/ })).not.toBeInTheDocument();
  expect(screen.getByText("First model")).toBeVisible();
  expect(screen.getByText("Recorded")).toBeVisible();
  expect(screen.getByText(/Resume this Pi session/)).toBeVisible();
  expect(model.loadChoices).not.toHaveBeenCalled();
});

it("keeps the panel and current model on failure and offers an explicit status refresh for uncertainty", async () => {
  const model = controller({ choices: [{ model: second, supports_images: true }] });
  const { rerender } = render(<SessionModelPicker model={model} provider="Pi" />);
  fireEvent.click(screen.getByRole("button", { name: /Change model/ }));
  const uncertain = { ...model, uncertain: true, can_select: false, error: "Model change was not confirmed." };
  rerender(<SessionModelPicker model={uncertain} provider="Pi" />);
  expect(screen.getByRole("dialog")).toBeVisible();
  expect(screen.getByRole("alert")).toHaveTextContent("Model change was not confirmed.");
  expect(screen.getByRole("button", { name: /Second model/ })).toBeDisabled();
  fireEvent.click(screen.getByRole("button", { name: "Refresh model status" }));
  await waitFor(() => expect(model.refresh).toHaveBeenCalledTimes(1));
  expect(model.select).not.toHaveBeenCalled();
});
