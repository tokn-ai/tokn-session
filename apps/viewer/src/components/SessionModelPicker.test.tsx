import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { SessionModelPicker } from "./SessionModelPicker";
import type { SessionModelController } from "../lib/useSessionModel";
import type { ModelIdentity, SessionModelState } from "../lib/types";

const first: ModelIdentity = { model_provider: "anthropic", model_id: "first", display_name: "First model" };
const second: ModelIdentity = { model_provider: "anthropic", model_id: "second", display_name: "Second model" };
const state: SessionModelState = { model: first, source: "live", revision: "one" };
function controller(overrides: Partial<SessionModelController> = {}): SessionModelController {
  return { status: { state, can_list: true, can_select: true, reason: null, runtime_generation: "pi:1", updates_pi_default: true, supports_model_input: true },
    state, checking: false, loading: false, choices: null, error: null, reason: null, input_revision: 0,
    running: false, can_open: true, can_select: true, refresh: vi.fn(async () => {}), loadChoices: vi.fn(async () => {}),
    choiceAllowed: vi.fn(() => true), acceptState: vi.fn(() => true), ...overrides };
}
const callbacks = () => ({ on_select: vi.fn(), on_clear: vi.fn() });
afterEach(cleanup);

it("opens the lazy draft picker with Send/Pi-default guidance and dismisses on Escape/outside", () => {
  const model = controller();
  render(<><SessionModelPicker model={model} provider="Pi" {...callbacks()} /><button>Outside</button></>);
  expect(model.loadChoices).not.toHaveBeenCalled();
  const trigger = screen.getByRole("button", { name: /Model: First model/ });
  fireEvent.click(trigger);
  const dialog = screen.getByRole("dialog", { name: "Model" });
  expect(dialog).toHaveFocus();
  expect(model.loadChoices).toHaveBeenCalledTimes(1);
  expect(within(dialog).getByText(/applied when you send/)).toBeVisible();
  expect(within(dialog).getByText(/updates Pi’s default/)).toBeVisible();
  fireEvent.keyDown(dialog, { key: "Escape" });
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(trigger).toHaveFocus();
  fireEvent.click(trigger);
  fireEvent.pointerDown(screen.getByRole("button", { name: "Outside" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});

it("checks the draft model separately from actual state and chooses a verified option locally", () => {
  const model = controller({ running: true, choices: [first, second].map((model) => ({ model, supports_images: model === second })) });
  const actions = callbacks();
  render(<SessionModelPicker model={model} provider="Pi" selected_model={{ model: second, supports_images: true }} {...actions} />);
  fireEvent.click(screen.getByRole("button", { name: /Model: Second model/ }));
  expect(screen.getByRole("button", { name: /Second model anthropic/ })).toHaveAttribute("aria-pressed", "true");
  expect(screen.getByRole("dialog")).toHaveTextContent("Send steers the current model");
  fireEvent.click(screen.getByRole("button", { name: /First model anthropic/ }));
  expect(actions.on_select).toHaveBeenCalledExactlyOnceWith({ model: first, supports_images: false });
  expect(model.state?.model).toEqual(first);
  fireEvent.click(screen.getByRole("button", { name: "Use current model" }));
  expect(actions.on_clear).toHaveBeenCalledTimes(1);
});

it("shows recorded/read-only reasons without opening a catalog", () => {
  const model = controller({ can_open: false, can_select: false, state: { ...state, source: "recorded" }, reason: "This session is read-only." });
  render(<SessionModelPicker model={model} provider="Pi" {...callbacks()} />);
  expect(screen.queryByRole("button", { name: /Change model/ })).not.toBeInTheDocument();
  expect(screen.getByText("Recorded")).toBeVisible();
  expect(screen.getByText("This session is read-only.")).toBeVisible();
  expect(model.loadChoices).not.toHaveBeenCalled();
});

it("retains draft selection through catalog errors and exposes retry", () => {
  const model = controller({ choices: [{ model: second, supports_images: true }] });
  const { rerender } = render(<SessionModelPicker model={model} provider="Pi" {...callbacks()} />);
  fireEvent.click(screen.getByRole("button", { name: /Change model/ }));
  rerender(<SessionModelPicker model={{ ...model, error: "Catalog unavailable", can_select: false }} provider="Pi"
    selected_model={{ model: second, supports_images: true }} {...callbacks()} />);
  expect(screen.getByRole("alert")).toHaveTextContent("Catalog unavailable");
  fireEvent.click(screen.getByRole("button", { name: "Retry models" }));
  expect(model.loadChoices).toHaveBeenCalledTimes(1);
});
