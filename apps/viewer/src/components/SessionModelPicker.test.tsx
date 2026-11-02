import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { useState } from "react";
import { SessionModelPicker } from "./SessionModelPicker";
import type { SessionModelController } from "../lib/useSessionModel";
import type { ModelIdentity, SessionModelState } from "../lib/types";
import { useSessionCreationModels, type SessionDraftModel } from "../lib/useSessionCreationModels";

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
afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

it("repositions an open model panel when a mobile keyboard resizes only the visual viewport", () => {
  const viewport = Object.assign(new EventTarget(), { offsetTop: 0 });
  vi.stubGlobal("visualViewport", viewport);
  render(<SessionModelPicker model={controller()} provider="Pi" {...callbacks()} />);
  const trigger = screen.getByRole("button", { name: /Change model/ });
  let top = 600;
  vi.spyOn(trigger, "getBoundingClientRect").mockImplementation(() => ({ left: 20, top }) as DOMRect);
  fireEvent.click(trigger);
  expect(screen.getByRole("dialog")).toHaveStyle({ maxHeight: "460px" });
  top = 260;
  act(() => viewport.dispatchEvent(new Event("resize")));
  expect(screen.getByRole("dialog")).toHaveStyle({ maxHeight: "236px" });
  viewport.offsetTop = 30;
  act(() => viewport.dispatchEvent(new Event("scroll")));
  expect(screen.getByRole("dialog")).toHaveStyle({ maxHeight: "206px" });
});

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

it.each(["DSH", "Pi"] as const)("keeps %s model choice local until Send and discloses its native default change", async (provider) => {
  const request = { project_id: "fixture", provider: provider === "DSH" ? "dsh" as const : "pi" as const };
  const choice = { model: first, supports_images: false };
  const load = vi.fn(async () => ({ models: [choice], can_select: true, updates_default: true,
    catalog_revision: "verified-catalog", runtime_profile_id: "host-runtime", runtime_revision: 1, reason: null }));
  const actions = callbacks();
  function DraftPicker() {
    const model = useSessionCreationModels(request, "verified-scope", true, load);
    const [selected, setSelected] = useState<SessionDraftModel | null>(null);
    return <SessionModelPicker model={model} provider={provider} mode="creation" selected_model={selected?.choice}
      on_select={(choice) => { const selection = model.selectionFor(choice); if (selection) { setSelected(selection); actions.on_select(choice); } }}
      on_clear={() => { setSelected(null); actions.on_clear(); }} />;
  }
  render(<DraftPicker />);
  expect(load).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: /Model: Runtime default/ }));
  const model_choice = await screen.findByRole("button", { name: /First model anthropic/ });
  expect(model_choice).toBeEnabled();
  expect(screen.getByText(`Applying a model on first Send also updates ${provider}’s default for new sessions on this host.`)).toBeVisible();
  expect(screen.getByText("Your model choice is applied when you send the first message.")).toBeVisible();
  fireEvent.click(model_choice);
  expect(actions.on_select).toHaveBeenCalledExactlyOnceWith(choice);
  expect(screen.getByRole("button", { name: /Model: First model/ })).toBeInTheDocument();
  const default_choice = within(screen.getByRole("dialog")).getByRole("button", { name: /Runtime default Use the agent’s configured model/ });
  expect(default_choice).toBeEnabled();
  fireEvent.click(default_choice);
  expect(actions.on_clear).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("button", { name: /Model: Runtime default/ })).toBeInTheDocument();
  expect(load).toHaveBeenCalledExactlyOnceWith(request);
});
