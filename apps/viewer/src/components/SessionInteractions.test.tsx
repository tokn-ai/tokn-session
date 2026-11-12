import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { listSessionInteractions, respondSessionInteraction } from "../lib/tauri";
import type { SessionInteraction, SessionInteractionsResponse, SessionSummary, SubmitSessionInputResponse } from "../lib/types";
import { SessionInteractions } from "./SessionInteractions";

const machine = vi.hoisted(() => ({ scope: "machine-one", invoke: vi.fn(), changes: new Set<() => void>(), closed: new Set<() => void>(), fail_listen: false }));
vi.mock("../lib/transport", () => ({ viewerStorageScope: () => machine.scope,
  captureTransport: () => ({ invoke: machine.invoke, listen: async (_event: string, changed: () => void) => {
    if (machine.fail_listen) throw new Error("Events unavailable");
    machine.changes.add(changed); return () => machine.changes.delete(changed);
  }, on_close: (closed: () => void) => { machine.closed.add(closed); return () => machine.closed.delete(closed); } }),
}));
vi.mock("../lib/tauri", () => ({ listSessionInteractions: vi.fn(), respondSessionInteraction: vi.fn() }));

const session = { session_key: "zcode:one", provider: "zcode", is_subagent: false } as SessionSummary;
const question: SessionInteraction = { interaction_id: "request-one", kind: "question", title: "Choose a direction", details: null,
  response_pending: false, permission_decisions: [], questions: [
    { question_id: "choice", prompt: "Which design?", options: [{ label: "Compact", description: "Less space" }, { label: "Detailed", description: null }],
      allows_free_text: true, allows_option_notes: true, is_secret: false, multiple: false },
    { question_id: "features", prompt: "Which features?", options: [{ label: "Search", description: null }, { label: "Export", description: null }],
      allows_free_text: false, allows_option_notes: false, is_secret: false, multiple: true },
    { question_id: "secret", prompt: "Private access code", options: [], allows_free_text: true, allows_option_notes: false, is_secret: true, multiple: false },
  ] };
const permission: SessionInteraction = { interaction_id: "permission-one", kind: "permission", title: "Run command", details: "printf 'fixture'\nwithin the project",
  response_pending: false, questions: [], permission_decisions: ["allow_once", "deny"] };
const snapshot = (interactions: SessionInteraction[] = [permission], overrides: Partial<SessionInteractionsResponse> = {}): SessionInteractionsResponse => ({
  session_key: session.session_key, runtime_id: "runtime", instance_id: "instance", interactions, ...overrides,
});
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}
async function ready() { await waitFor(() => expect(screen.getByRole("button", { name: "Refresh requests" })).toBeEnabled()); }
function changed() { act(() => { for (const handler of machine.changes) handler(); }); }

beforeEach(() => {
  vi.resetAllMocks(); machine.scope = "machine-one"; machine.changes.clear(); machine.closed.clear(); machine.fail_listen = false;
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot());
  vi.mocked(respondSessionInteraction).mockImplementation(async (request) => ({ request_id: request.request_id, status: "accepted", message: "Response sent" }));
});
afterEach(cleanup);

it("submits complete questions with multiple choices, notes and secret input using the runtime fence", async () => {
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([question]));
  render(<SessionInteractions session={session} />);
  await ready();
  expect(screen.getByRole("button", { name: "Submit answers" })).toBeDisabled();
  fireEvent.click(screen.getByRole("radio", { name: /Compact/ }));
  fireEvent.click(screen.getByRole("checkbox", { name: "Search" }));
  fireEvent.click(screen.getByRole("checkbox", { name: "Export" }));
  fireEvent.change(screen.getByLabelText("Additional note"), { target: { value: "Keep the tools" } });
  const secret = within(screen.getByRole("group", { name: "Private access code" })).getByLabelText("Written answer");
  expect(secret).toHaveAttribute("type", "password");
  fireEvent.change(secret, { target: { value: "private fixture" } });
  fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
  await screen.findByText("Response sent. Waiting for the request to close.");
  expect(respondSessionInteraction).toHaveBeenCalledExactlyOnceWith({ session_key: session.session_key, request_id: expect.any(String),
    runtime_id: "runtime", instance_id: "instance", interaction_id: question.interaction_id, response: { type: "answers", answers: [
      { question_id: "choice", answers: ["Compact"], custom: "Keep the tools" }, { question_id: "features", answers: ["Search", "Export"] },
      { question_id: "secret", answers: [], custom: "private fixture" },
    ] } }, machine.invoke);
  expect(screen.getByRole("form", { name: question.title })).toBeVisible();
  expect(screen.getByRole("button", { name: "Submit answers" })).toBeDisabled();
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([]));
  changed();
  await waitFor(() => expect(screen.queryByRole("region", { name: "Pending requests" })).not.toBeInTheDocument());
});

it("shows the requested operation and only permission decisions supplied by the host", async () => {
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([{ ...permission, permission_decisions: ["deny"] }]));
  render(<SessionInteractions session={session} />); await ready();
  expect(screen.getByText(/printf 'fixture'/).textContent).toBe(permission.details);
  expect(screen.queryByRole("button", { name: "Allow once" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Deny" }));
  await waitFor(() => expect(respondSessionInteraction).toHaveBeenCalledTimes(1));
  expect(vi.mocked(respondSessionInteraction).mock.calls[0][0].response).toEqual({ type: "permission", decision: "deny" });
});

it("retains an uncertain response across refresh and navigation without resubmitting", async () => {
  vi.mocked(respondSessionInteraction).mockRejectedValue(new Error("Connection ended after dispatch"));
  const view = render(<SessionInteractions session={session} />); await ready();
  fireEvent.click(screen.getByRole("button", { name: "Allow once" }));
  await screen.findByText(/Response delivery is not confirmed/);
  await ready();
  expect(screen.getByRole("button", { name: "Allow once" })).toBeDisabled();
  view.rerender(<SessionInteractions session={null} />);
  view.rerender(<SessionInteractions session={session} />);
  await ready();
  expect(screen.getByRole("button", { name: "Allow once" })).toBeDisabled();
  changed(); await ready();
  expect(respondSessionInteraction).toHaveBeenCalledTimes(1);
});

it("disables a native response already awaiting resolution in another window", async () => {
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([{ ...permission, response_pending: true }]));
  render(<SessionInteractions session={session} />); await ready();
  expect(screen.getByText("Response sent. Waiting for the request to close.")).toBeVisible();
  expect(screen.getByRole("button", { name: "Allow once" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Deny" })).toBeDisabled();
  expect(respondSessionInteraction).not.toHaveBeenCalled();
});

it("keeps manually refreshed requests actionable when event subscription fails", async () => {
  machine.fail_listen = true;
  render(<SessionInteractions session={session} />); await ready();
  expect(screen.getByText(/Live request updates are unavailable/)).toBeVisible();
  expect(screen.getByRole("button", { name: "Allow once" })).toBeEnabled();
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([]));
  fireEvent.click(screen.getByRole("button", { name: "Refresh requests" }));
  await waitFor(() => expect(screen.queryByRole("button", { name: "Allow once" })).not.toBeInTheDocument());
  expect(respondSessionInteraction).not.toHaveBeenCalled();
});

it("rejects delayed list replies from a previous session and releases its subscriptions", async () => {
  const old = deferred<SessionInteractionsResponse>();
  vi.mocked(listSessionInteractions).mockImplementation(async (request) => request.session_key === session.session_key
    ? old.promise : snapshot([{ ...permission, title: "Second session request" }], { session_key: "zcode:two" }));
  const view = render(<SessionInteractions session={session} />);
  view.rerender(<SessionInteractions session={{ ...session, session_key: "zcode:two" }} />);
  await screen.findByRole("form", { name: "Second session request" });
  await act(async () => old.resolve(snapshot()));
  expect(screen.queryByRole("form", { name: permission.title })).not.toBeInTheDocument();
  expect(machine.changes.size).toBe(1);
  view.unmount();
  expect(machine.changes.size).toBe(0);
  expect(machine.closed.size).toBe(0);
});

it("ignores responses and keeps question drafts scoped when the selected machine changes", async () => {
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([question]));
  const view = render(<SessionInteractions session={session} />); await ready();
  const private_input = () => within(screen.getByRole("group", { name: "Private access code" })).getByLabelText("Written answer");
  fireEvent.change(private_input(), { target: { value: "first machine draft" } });
  const old = deferred<SessionInteractionsResponse>();
  vi.mocked(listSessionInteractions).mockReturnValue(old.promise);
  changed();
  machine.scope = "machine-two";
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([question]));
  view.rerender(<SessionInteractions session={session} />);
  await ready();
  expect(private_input()).toHaveValue("");
  await act(async () => old.resolve(snapshot([{ ...permission, title: "Old machine approval" }])));
  expect(screen.queryByText("Old machine approval")).not.toBeInTheDocument();
  machine.scope = "machine-one";
  view.rerender(<SessionInteractions session={session} />);
  await ready();
  expect(private_input()).toHaveValue("first machine draft");
  expect(machine.changes.size).toBe(1);
});

it("uses written feedback instead of an option when the host disallows option notes", async () => {
  const feedback = { ...question, questions: [{ ...question.questions[0], allows_option_notes: false }] };
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([feedback]));
  render(<SessionInteractions session={session} />); await ready();
  const choice = screen.getByRole("radio", { name: /Compact/ });
  fireEvent.click(choice);
  fireEvent.change(screen.getByLabelText("Written answer"), { target: { value: "Please revise the plan" } });
  expect(choice).not.toBeChecked();
  fireEvent.click(screen.getByRole("button", { name: "Submit answers" }));
  await waitFor(() => expect(respondSessionInteraction).toHaveBeenCalledTimes(1));
  expect(vi.mocked(respondSessionInteraction).mock.calls[0][0].response).toEqual({ type: "answers", answers: [
    { question_id: "choice", answers: [], custom: "Please revise the plan" },
  ] });
});

it("keeps an old-instance dispatch from affecting a replacement and ignores double clicks", async () => {
  const response = deferred<SubmitSessionInputResponse>();
  vi.mocked(respondSessionInteraction).mockReturnValue(response.promise);
  render(<SessionInteractions session={session} />); await ready();
  const allow = screen.getByRole("button", { name: "Allow once" });
  act(() => { fireEvent.click(allow); fireEvent.click(allow); });
  expect(respondSessionInteraction).toHaveBeenCalledTimes(1);
  vi.mocked(listSessionInteractions).mockResolvedValue(snapshot([{ ...permission, title: "Replacement request" }], { instance_id: "replacement" }));
  changed();
  await screen.findByRole("form", { name: "Replacement request" });
  const request_id = vi.mocked(respondSessionInteraction).mock.calls[0][0].request_id;
  await act(async () => response.resolve({ request_id, status: "accepted", message: "Response sent" }));
  await ready();
  expect(screen.getByRole("button", { name: "Allow once" })).toBeEnabled();
  expect(screen.queryByText("Response sent. Waiting for the request to close.")).not.toBeInTheDocument();
});

it("clears requests on machine disconnect and does not query drafts, subagents or other providers", async () => {
  const view = render(<SessionInteractions session={session} />); await ready();
  act(() => { for (const closed of machine.closed) closed(); });
  expect(screen.queryByRole("button", { name: "Allow once" })).not.toBeInTheDocument();
  expect(screen.getByText(/Machine disconnected/)).toBeVisible();
  vi.mocked(listSessionInteractions).mockClear();
  view.rerender(<SessionInteractions session={{ ...session, is_subagent: true }} />);
  view.rerender(<SessionInteractions session={{ ...session, provider: "pi" }} />);
  view.rerender(<SessionInteractions session={null} />);
  expect(listSessionInteractions).not.toHaveBeenCalled();
});
