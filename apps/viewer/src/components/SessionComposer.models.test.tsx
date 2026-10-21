import { useMemo } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SessionComposer } from "./SessionComposer";
import { useSessionModel } from "../lib/useSessionModel";
import { getSessionInputStatus, getSessionModelStatus, listSessionModels, selectSessionModel, submitSessionInput } from "../lib/tauri";
import { DEFAULT_IMAGE_LIMITS } from "../lib/imageDrafts";
import type { SessionMutationLock } from "../lib/sessionModels";
import type { SessionSummary, SelectSessionModelResponse } from "../lib/types";

vi.mock("../lib/tauri", () => ({ getSessionInputStatus: vi.fn(), getSessionModelStatus: vi.fn(), listSessionModels: vi.fn(), selectSessionModel: vi.fn(), submitSessionInput: vi.fn() }));
const session = { session_key: "pi:one", provider: "pi" } as SessionSummary;
const first = { model_provider: "anthropic", model_id: "first", display_name: "First model" };
const second = { model_provider: "anthropic", model_id: "second", display_name: "Second model" };
function Composer() {
  const guard = useMemo<SessionMutationLock>(() => ({ send_busy: false, model_busy: false }), []);
  const model = useSessionModel(session.session_key, undefined, guard);
  return <SessionComposer session={session} model={model} mutation_lock={guard} input_refresh_key={model.input_revision} />;
}
beforeEach(() => {
  vi.resetAllMocks();
  const status = { state: { model: first, source: "live" as const, revision: "1" }, can_select: true, can_list: true,
    runtime_generation: "runtime", reason: null, updates_pi_default: true };
  vi.mocked(getSessionModelStatus).mockResolvedValue(status);
  vi.mocked(listSessionModels).mockResolvedValue({ status, models: [first, second].map((model) => ({ model, supports_images: model === second })) });
  vi.mocked(getSessionInputStatus).mockResolvedValue({ available: true, max_length: 1000, message: "" });
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({ request_id: request.request_id, status: "accepted", message: "Sent" }));
});
afterEach(cleanup);

it("rechecks model controls with input availability when a missing Pi bridge starts later", async () => {
  vi.mocked(getSessionInputStatus).mockResolvedValueOnce({ available: false, max_length: 1000, message: "The Pi input bridge is not running." });
  vi.mocked(getSessionModelStatus).mockResolvedValueOnce({ state: { model: null, source: "unknown", revision: "unknown" },
    can_select: false, can_list: false, reason: "The Pi input bridge is not running.", runtime_generation: null, updates_pi_default: false });
  render(<Composer />);
  const retry = await screen.findByRole("button", { name: "Retry availability" });
  await waitFor(() => expect(getSessionModelStatus).toHaveBeenCalledTimes(1));
  expect(screen.queryByRole("button", { name: /Change model/ })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Refresh model status" })).not.toBeInTheDocument();
  fireEvent.click(retry);
  expect(await screen.findByRole("button", { name: /Model: First model. Change model/ })).toBeEnabled();
  expect(screen.getByRole("textbox", { name: "Message this session" })).toBeEnabled();
  expect(vi.mocked(getSessionInputStatus).mock.calls.length).toBeGreaterThanOrEqual(2);
  expect(getSessionModelStatus).toHaveBeenCalledTimes(2);
  expect(listSessionModels).not.toHaveBeenCalled();
});

it("preserves the prompt while changing model and refreshes image input capabilities only after application", async () => {
  let resolve!: (value: SelectSessionModelResponse) => void;
  vi.mocked(selectSessionModel).mockReturnValue(new Promise((done) => { resolve = done; }));
  render(<Composer />);
  const field = screen.getByRole("textbox", { name: "Message this session" });
  await waitFor(() => expect(field).toBeEnabled());
  fireEvent.change(field, { target: { value: "Keep this prompt" } });
  expect(screen.queryByLabelText("Image files")).not.toBeInTheDocument();
  fireEvent.click(await screen.findByRole("button", { name: /Model: First model/ }));
  fireEvent.click(await screen.findByRole("button", { name: /Second model anthropic · Images/ }));
  expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  fireEvent.submit(screen.getByRole("form"));
  expect(submitSessionInput).not.toHaveBeenCalled();
  expect(field).toHaveValue("Keep this prompt");
  vi.mocked(getSessionInputStatus).mockResolvedValue({ available: true, max_length: 1000, message: "", images_available: true, ...DEFAULT_IMAGE_LIMITS });
  const request_id = vi.mocked(selectSessionModel).mock.calls[0][0].request_id;
  await act(async () => resolve({ request_id, status: "applied", message: "Changed", state: { model: second, source: "live", revision: "2" } }));
  await waitFor(() => expect(getSessionInputStatus).toHaveBeenCalledTimes(2));
  expect(screen.getByRole("button", { name: /Model: Second model/ })).toBeInTheDocument();
  expect(screen.getByLabelText("Image files")).toBeInTheDocument();
  expect(field).toHaveValue("Keep this prompt");
  // Closing the model panel returns to the same draft and new input capability.
  fireEvent.click(screen.getByRole("button", { name: "Close model selection" }));
  expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
});

it("refreshes image input after an uncertain change is resolved by live model status", async () => {
  vi.mocked(selectSessionModel).mockImplementation(async (request) => ({ request_id: request.request_id,
    status: "unknown", state: null, message: "The connection was interrupted" }));
  render(<Composer />);
  const field = screen.getByRole("textbox", { name: "Message this session" });
  await waitFor(() => expect(field).toBeEnabled());
  fireEvent.change(field, { target: { value: "Keep the draft after recovery" } });
  fireEvent.click(await screen.findByRole("button", { name: /Model: First model/ }));
  fireEvent.click(await screen.findByRole("button", { name: /Second model anthropic · Images/ }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Model change was not confirmed.");
  expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  expect(getSessionInputStatus).toHaveBeenCalledTimes(1);
  const recovered = { state: { model: second, source: "live" as const, revision: "2" },
    can_select: true, can_list: true, reason: null, runtime_generation: "runtime", updates_pi_default: true };
  vi.mocked(getSessionModelStatus).mockResolvedValue(recovered);
  vi.mocked(listSessionModels).mockResolvedValue({ status: recovered, models: [first, second].map((model) => ({ model, supports_images: model === second })) });
  vi.mocked(getSessionInputStatus).mockResolvedValue({ available: true, max_length: 1000, message: "", images_available: true, ...DEFAULT_IMAGE_LIMITS });
  fireEvent.click(screen.getByRole("button", { name: "Refresh model status" }));
  await waitFor(() => expect(getSessionInputStatus).toHaveBeenCalledTimes(2));
  expect(screen.getByLabelText("Image files")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: /Model: Second model/ })).toBeInTheDocument();
  expect(field).toHaveValue("Keep the draft after recovery");
  expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
  expect(selectSessionModel).toHaveBeenCalledTimes(1);
});
