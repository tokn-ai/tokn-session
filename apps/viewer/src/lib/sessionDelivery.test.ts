import { beforeEach, expect, it, vi } from "vitest";
import { invoke, type CommandInvoker } from "./transport";
import { getSessionModelStatus, listSessionModels, listSessionCreationModels, createProjectSession, inspectSessionEvent, openSession, releaseSession, loadToolDetails, renewSessionSubscriptions, submitSessionInput, getSessionControl, setSessionControl, cancelSessionTurn } from "./tauri";
import type { OpenSessionRequest, SessionUpdate } from "./types";

vi.mock("./transport", () => ({ invoke: vi.fn(), listen: vi.fn(), isDesktop: () => false }));
beforeEach(() => vi.mocked(invoke).mockReset());

it("keeps draft model discovery and creation proof on the captured machine without local fallback", async () => {
  const { send, calls } = machineInvoker();
  calls.mockResolvedValue({});
  const target = { project_id: "repo", provider: "codex" as const, runtime_profile_id: "local", runtime_revision: 3, runtime_plan_id: "verified-plan" };
  const request = { ...target, request_id: "first", prompt: "Hello", model: { model_provider: "openai", model_id: "chosen",
    catalog_revision: "catalog", runtime_profile_id: "local", runtime_revision: 3, runtime_plan_id: "verified-plan" } };
  await listSessionCreationModels(target, send);
  await createProjectSession(request, send);
  expect(calls.mock.calls).toEqual([["list_session_creation_models", { request: target }], ["create_project_session", { request }]]);
  calls.mockRejectedValueOnce(new Error("Machine disconnected"));
  await expect(listSessionCreationModels(target, send)).rejects.toThrow("Machine disconnected");
  expect(invoke).not.toHaveBeenCalled();
});

it("keeps session transport selection and cancellation on the captured machine without fallback", async () => {
  const { send, calls } = machineInvoker();
  calls.mockResolvedValue({});
  const target = { session_key: "one" };
  const selection = { ...target, request_id: "choice", transport: "desktop" as const };
  const cancel = { ...target, request_id: "stop" };
  await getSessionControl(target, send);
  await setSessionControl(selection, send);
  await cancelSessionTurn(cancel, send);
  expect(calls.mock.calls).toEqual([
    ["get_session_control", { request: target }],
    ["set_session_control", { request: selection }],
    ["cancel_session_turn", { request: cancel }],
  ]);
  calls.mockRejectedValueOnce(new Error("Remote unavailable"));
  await expect(setSessionControl(selection, send)).rejects.toThrow("Remote unavailable");
  expect(invoke).not.toHaveBeenCalled();
});

it("routes model reads and mutation through the captured session machine without local fallback", async () => {
  const { send, calls } = machineInvoker();
  calls.mockResolvedValue({});
  const target = { session_key: "one" };
  const message = { ...target, request_id: "uuid", text: "next", model_selection: { model_provider: "anthropic", model_id: "sonnet", expected_revision: "revision", runtime_generation: "pi-runtime" } };
  await getSessionModelStatus(target, send);
  await listSessionModels(target, send);
  await submitSessionInput(message, send);
  expect(calls.mock.calls).toEqual([
    ["get_session_model_status", { request: target }],
    ["list_session_models", { request: target }],
    ["submit_session_input", { request: message }],
  ]);
  expect(invoke).not.toHaveBeenCalled();
});
const request: OpenSessionRequest = { subscription_id: "modern", session_key: "one", level: "steps", scope: { history: "latest_turn", group_keys: [] } };
const update: SessionUpdate = { ...request, generation: "source", revision: "1", snapshot: true, base_revision: null,
  items: [], groups: [], removed_items: [], item_order: [], state: { total_events: 0, previous_cursor: null, next_cursor: null, history_status: "complete" } };

it("opens cold and retained projections in one request each on their captured machine", async () => {
  const { send, calls } = machineInvoker();
  calls.mockResolvedValue(update);
  const resume = { ...request, resume: { subscription_id: "earlier", generation: "source", revision: "1" } };
  const group = { ...resume, scope: { ...request.scope!, group_keys: ["activity:a"] } };
  for (const open of [request, resume, group]) expect(await openSession(open, send)).toBe(update);
  expect(calls.mock.calls).toEqual([request, resume, group].map((request) => ["open_session", { request }]));
  calls.mockRejectedValueOnce(new Error("Unknown viewer API route"));
  await expect(openSession(request, send)).rejects.toThrow("Unknown viewer API route");
  expect(calls).toHaveBeenCalledTimes(4);
  expect(invoke).not.toHaveBeenCalled();
});

it("keeps tool display and source inspection requests distinct", async () => {
  vi.mocked(invoke).mockResolvedValue({});
  const event = { session_key: "one", event_key: "tool" };
  await loadToolDetails(event); await inspectSessionEvent(event);
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["load_session_details", { request: { kind: "tool", request: event } }],
    ["inspect_session_event", { request: event }],
  ]);
});
it("releases and renews interests without requesting timeline data", async () => {
  const leases = { renewed_ids: ["live"], missing_ids: ["expired"] };
  vi.mocked(invoke).mockResolvedValueOnce(null).mockResolvedValueOnce(leases);
  await releaseSession(request);
  expect(await renewSessionSubscriptions(["live", "expired"])).toEqual(leases);
  expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toEqual(["release_session", "renew_session_subscriptions"]);
});

function machineInvoker() {
  const calls = vi.fn<(command: string, payload?: Record<string, unknown>) => Promise<unknown>>();
  const send: CommandInvoker = <T,>(command: string, payload?: Record<string, unknown>) => calls(command, payload) as Promise<T>;
  return { send, calls };
}
