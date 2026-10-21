import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { SessionComposer } from "./SessionComposer";
import { useSessionModel } from "../lib/useSessionModel";
import { getSessionInputStatus, getSessionModelStatus, listSessionModels, submitSessionInput, uploadImageChunk, releaseImageUpload } from "../lib/tauri";
import { DEFAULT_IMAGE_LIMITS } from "../lib/imageDrafts";
import type { SessionSummary, SubmitSessionInputResponse, SessionModelStatus } from "../lib/types";

const transport = vi.hoisted(() => ({ scope: "local", invoke: vi.fn() }));
vi.mock("../lib/transport", () => ({ invoke: vi.fn(), listen: vi.fn(), viewerStorageScope: () => transport.scope,
  captureTransport: () => ({ invoke: transport.invoke, on_close: () => () => {}, release: vi.fn() }) }));
vi.mock("../lib/tauri", () => ({ getSessionInputStatus: vi.fn(), getSessionModelStatus: vi.fn(), listSessionModels: vi.fn(),
  submitSessionInput: vi.fn(), uploadImageChunk: vi.fn(), releaseImageUpload: vi.fn() }));
const session = { session_key: "pi:one", provider: "pi" } as SessionSummary;
const first = { model_provider: "anthropic", model_id: "first", display_name: "First model" };
const second = { model_provider: "anthropic", model_id: "second", display_name: "Second model" };
let actual: SessionModelStatus;
function Composer({ target = session, running = false, refresh_key = 0 }: { target?: SessionSummary; running?: boolean; refresh_key?: number }) {
  const model = useSessionModel(target.session_key, undefined, running, refresh_key);
  return <><span aria-label="Runtime model">{model.state?.model?.display_name}</span>
    <SessionComposer session={target} model={model} input_refresh_key={model.input_revision} /></>;
}
const field = () => screen.getByRole("textbox", { name: "Message this session" });
const prompt = (text = "Keep this message") => fireEvent.change(field(), { target: { value: text } });
async function chooseSecond() {
  fireEvent.click(await screen.findByRole("button", { name: /Model: First model/ }));
  fireEvent.click(await screen.findByRole("button", { name: /Second model anthropic · Images/ }));
}
beforeEach(() => {
  vi.resetAllMocks(); transport.scope = "local";
  Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => "blob:draft") }, revokeObjectURL: { configurable: true, value: vi.fn() } });
  actual = { state: { model: first, source: "live", revision: "1" }, can_select: true, can_list: true,
    runtime_generation: "runtime", reason: null, updates_pi_default: true, supports_model_input: true };
  vi.mocked(getSessionModelStatus).mockImplementation(async () => actual);
  vi.mocked(listSessionModels).mockImplementation(async () => ({ status: actual, models: [first, second].map((model) => ({ model, supports_images: model === second })) }));
  vi.mocked(getSessionInputStatus).mockImplementation(async () => ({ available: true, max_length: 1000, message: "",
    images_available: actual.state.model?.model_id === "second", images_stageable: true, ...DEFAULT_IMAGE_LIMITS }));
  vi.mocked(submitSessionInput).mockImplementation(async (request) => {
    actual = { ...actual, state: { model: second, source: "live", revision: "2" } };
    return { request_id: request.request_id, status: "accepted", message: "Sent", model_result: { outcome: "applied", state: actual.state } };
  });
  vi.mocked(uploadImageChunk).mockImplementation(async (request) => ({ upload_id: request.upload_id, received_bytes: request.total_bytes, image_id: "uploaded" }));
  vi.mocked(releaseImageUpload).mockResolvedValue();
});
afterEach(cleanup);

it("chooses locally, keeps the header actual, and applies the captured choice only on Send", async () => {
  render(<Composer />);
  await waitFor(() => expect(field()).toBeEnabled()); prompt();
  await chooseSecond();
  expect(submitSessionInput).not.toHaveBeenCalled();
  expect(screen.getByLabelText("Runtime model")).toHaveTextContent("First model");
  expect(screen.getByRole("button", { name: /Model: Second model/ })).toBeEnabled();
  expect(screen.getByLabelText("Image files")).toBeInTheDocument();
  act(() => { fireEvent.submit(screen.getByRole("form")); fireEvent.submit(screen.getByRole("form")); });
  await waitFor(() => expect(field()).toHaveValue(""));
  expect(submitSessionInput).toHaveBeenCalledExactlyOnceWith({ session_key: session.session_key,
    request_id: expect.any(String), text: "Keep this message", model_selection: { model_provider: "anthropic",
      model_id: "second", expected_revision: "1", runtime_generation: "runtime" } }, transport.invoke);
  expect(screen.getByLabelText("Runtime model")).toHaveTextContent("Second model");
  expect(screen.queryByRole("button", { name: "Use current model" })).not.toBeInTheDocument();
});

it.each(["accepted", "not_sent", "unknown"] as const)("resets a busy ignored choice from actual state even when delivery is %s", async (status) => {
  actual.can_select = false; actual.reason = "Wait for Pi to finish";
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({ request_id: request.request_id,
    status, message: "The current model was used", model_result: { outcome: "ignored_while_running", state: actual.state } }));
  render(<Composer running />);
  await waitFor(() => expect(field()).toBeEnabled()); prompt();
  await chooseSecond();
  expect(screen.getByRole("dialog")).toHaveTextContent("Send steers the current model and ignores this choice");
  expect(screen.getByLabelText("Image files")).toBeInTheDocument();
  fireEvent.submit(screen.getByRole("form"));
  await waitFor(() => expect(screen.queryByRole("button", { name: "Use current model" })).not.toBeInTheDocument());
  expect(screen.getByRole("button", { name: /Model: First model/ })).toBeInTheDocument();
  expect(screen.getByLabelText("Runtime model")).toHaveTextContent("First model");
  expect(field()).toHaveValue(status === "accepted" ? "" : "Keep this message");
  expect(vi.mocked(submitSessionInput).mock.calls[0][0].model_selection?.model_id).toBe("second");
});

it.each(["not_sent", "unknown"] as const)("retains text and model drafts after %s without replaying delivery", async (status) => {
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({ request_id: request.request_id, status, message: "Delivery failed" }));
  render(<Composer />);
  await waitFor(() => expect(field()).toBeEnabled()); prompt(); await chooseSecond();
  fireEvent.submit(screen.getByRole("form"));
  await screen.findByText(/Delivery failed/);
  expect(field()).toHaveValue("Keep this message");
  expect(screen.getByRole("button", { name: /Model: Second model/ })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Use current model" })).toBeInTheDocument();
  expect(screen.getByLabelText("Runtime model")).toHaveTextContent("First model");
  expect(submitSessionInput).toHaveBeenCalledTimes(1);
});

it("stages a busy vision draft and resets its choice after the host rejects images for the actual text model", async () => {
  actual.can_select = false;
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({ request_id: request.request_id,
    status: "not_sent", message: "The running model cannot receive images", model_result: {
      outcome: "ignored_while_running", state: actual.state,
    } }));
  render(<Composer running />);
  await waitFor(() => expect(field()).toBeEnabled()); await chooseSecond();
  fireEvent.change(screen.getByLabelText("Image files"), { target: { files: [new File(["png"], "screen.png", { type: "image/png" })] } });
  expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
  fireEvent.submit(screen.getByRole("form"));
  await screen.findByText("The running model cannot receive images");
  expect(uploadImageChunk).toHaveBeenCalledTimes(1);
  expect(vi.mocked(submitSessionInput).mock.calls[0][0]).toMatchObject({ image_ids: ["uploaded"], model_selection: { model_id: "second" } });
  expect(screen.queryByRole("button", { name: "Use current model" })).not.toBeInTheDocument();
  expect(screen.getByAltText("screen.png")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  expect(screen.getByLabelText("Runtime model")).toHaveTextContent("First model");
});

it("captures the latest runtime revision and retains each session's model draft", async () => {
  const view = render(<Composer />);
  await waitFor(() => expect(field()).toBeEnabled()); prompt(); await chooseSecond();
  fireEvent.click(screen.getByRole("button", { name: "Close model selection" }));
  const other = { ...session, session_key: "pi:two" };
  view.rerender(<Composer target={other} />);
  await waitFor(() => expect(field()).toBeEnabled());
  expect(field()).toHaveValue("");
  view.rerender(<Composer />);
  await waitFor(() => expect(field()).toBeEnabled());
  expect(field()).toHaveValue("Keep this message");
  expect(screen.getByRole("button", { name: /Model: Second model/ })).toBeInTheDocument();
  actual = { ...actual, state: { ...actual.state, revision: "fresh" }, runtime_generation: "fresh-runtime" };
  view.rerender(<Composer refresh_key={1} />);
  await waitFor(() => expect(getSessionModelStatus).toHaveBeenCalledTimes(4));
  fireEvent.submit(screen.getByRole("form"));
  await waitFor(() => expect(submitSessionInput).toHaveBeenCalledTimes(1));
  expect(vi.mocked(submitSessionInput).mock.calls[0][0].model_selection).toMatchObject({ expected_revision: "fresh", runtime_generation: "fresh-runtime" });
});

it("uses draft image capability, preserves attachments across local model changes, and uploads only on Send", async () => {
  render(<Composer />); await waitFor(() => expect(field()).toBeEnabled()); await chooseSecond();
  fireEvent.change(screen.getByLabelText("Image files"), { target: { files: [new File(["png"], "screen.png", { type: "image/png" })] } });
  expect(uploadImageChunk).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: /First model anthropic/ }));
  expect(screen.getByAltText("screen.png")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  expect(screen.getByText(/cannot use the attached images/)).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: /Second model anthropic · Images/ }));
  fireEvent.submit(screen.getByRole("form"));
  await waitFor(() => expect(submitSessionInput).toHaveBeenCalledTimes(1));
  expect(vi.mocked(submitSessionInput).mock.calls[0][0]).toMatchObject({ text: "", image_ids: ["uploaded"], model_selection: { model_id: "second" } });
});

it("keeps old hosts read-only for model choice and recovers when Retry availability finds a new bridge", async () => {
  vi.mocked(getSessionInputStatus).mockResolvedValueOnce({ available: false, max_length: 1000, message: "Bridge unavailable" });
  vi.mocked(getSessionModelStatus).mockResolvedValueOnce({ ...actual, supports_model_input: undefined, can_select: false });
  render(<Composer />);
  fireEvent.click(await screen.findByRole("button", { name: "Retry availability" }));
  expect(await screen.findByRole("button", { name: /Model: First model/ })).toBeEnabled();
  expect(listSessionModels).not.toHaveBeenCalled();
});

it("does not let a late old-host response reset the current host's model draft or header", async () => {
  let resolve!: (result: SubmitSessionInputResponse) => void;
  vi.mocked(submitSessionInput).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
  const view = render(<Composer />);
  await waitFor(() => expect(field()).toBeEnabled()); prompt("Old host prompt"); await chooseSecond();
  fireEvent.submit(screen.getByRole("form"));
  await waitFor(() => expect(submitSessionInput).toHaveBeenCalledTimes(1));
  const request_id = vi.mocked(submitSessionInput).mock.calls[0][0].request_id;
  transport.scope = "new-host";
  view.rerender(<Composer />);
  await waitFor(() => expect(field()).toBeEnabled()); prompt("New host prompt");
  await act(async () => resolve({ request_id, status: "accepted", message: "Sent", model_result: {
    outcome: "applied", state: { model: second, source: "live", revision: "old" },
  } }));
  expect(field()).toHaveValue("New host prompt");
  expect(screen.getByLabelText("Runtime model")).toHaveTextContent("First model");
});

it("clears an admitted original draft after returning to the session while preserving its fresh header", async () => {
  let resolve!: (result: SubmitSessionInputResponse) => void;
  vi.mocked(submitSessionInput).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
  const view = render(<Composer />);
  await waitFor(() => expect(field()).toBeEnabled()); prompt(); await chooseSecond();
  fireEvent.submit(screen.getByRole("form"));
  await waitFor(() => expect(submitSessionInput).toHaveBeenCalledTimes(1));
  const request_id = vi.mocked(submitSessionInput).mock.calls[0][0].request_id;
  view.rerender(<Composer target={{ ...session, session_key: "pi:other" }} />);
  await waitFor(() => expect(field()).toBeEnabled());
  actual = { ...actual, runtime_generation: "replacement-runtime", state: { model: first, source: "live", revision: "fresh" } };
  view.rerender(<Composer />);
  await waitFor(() => expect(screen.getByLabelText("Runtime model")).toHaveTextContent("First model"));
  await act(async () => resolve({ request_id, status: "accepted", message: "Sent", model_result: {
    outcome: "applied", state: { model: second, source: "live", revision: "old-admission" },
  } }));
  await waitFor(() => expect(field()).toHaveValue(""));
  expect(screen.queryByRole("button", { name: "Use current model" })).not.toBeInTheDocument();
  expect(screen.getByLabelText("Runtime model")).toHaveTextContent("First model");
});
