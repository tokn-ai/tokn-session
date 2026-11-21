import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getSessionInputStatus, submitSessionInput } from "../lib/tauri";
import { uploadImageDrafts } from "../lib/imageDrafts";
import type { SessionInputStatus, SessionSummary } from "../lib/types";
import { SessionComposer } from "./SessionComposer";

const machine = vi.hoisted(() => ({ scope: "local", invoke: vi.fn(), changes: new Set<() => void>(), closed: new Set<() => void>() }));
vi.mock("../lib/transport", () => ({ viewerStorageScope: () => machine.scope,
  captureTransport: () => ({ invoke: machine.invoke, release: vi.fn(), listen: async (_name: string, handler: () => void) => {
    machine.changes.add(handler); return () => machine.changes.delete(handler);
  }, on_close: (handler: () => void) => { machine.closed.add(handler); return () => machine.closed.delete(handler); } }),
}));
vi.mock("../lib/tauri", () => ({ getSessionInputStatus: vi.fn(), submitSessionInput: vi.fn() }));
vi.mock("../lib/imageDrafts", async (original) => ({ ...await original<typeof import("../lib/imageDrafts")>(), uploadImageDrafts: vi.fn(async () => []) }));
const session = { session_key: "zcode:session", provider: "zcode", is_running: true } as SessionSummary;
const available: SessionInputStatus = { available: true, message: "Connected", max_length: 16_384, delivery_modes: ["guide", "queue"] };
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
const field = () => screen.getByRole("textbox", { name: "Message this session" });
const draft = (text: string) => fireEvent.change(field(), { target: { value: text } });
async function ready() { await waitFor(() => expect(field()).toBeEnabled()); }
function changed() { act(() => { for (const handler of machine.changes) handler(); }); }
beforeEach(() => {
  vi.clearAllMocks(); machine.scope = "local"; machine.changes.clear(); machine.closed.clear();
  vi.mocked(getSessionInputStatus).mockResolvedValue(available);
  vi.mocked(uploadImageDrafts).mockResolvedValue([]);
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({ request_id: request.request_id, status: "accepted", message: "Message admitted" }));
});
afterEach(cleanup);

it("keeps legacy Send unchanged when the host does not advertise delivery modes", async () => {
  vi.mocked(getSessionInputStatus).mockResolvedValue({ available: true, message: "Ready", max_length: 16_384 });
  render(<SessionComposer session={session} />); await ready();
  expect(screen.queryByRole("button", { name: "Guide" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Queue" })).not.toBeInTheDocument();
  draft("Start this task");
  fireEvent.click(screen.getByRole("button", { name: "Send" }));
  await waitFor(() => expect(submitSessionInput).toHaveBeenCalledTimes(1));
  expect(submitSessionInput).toHaveBeenCalledExactlyOnceWith({ session_key: session.session_key, request_id: expect.any(String), text: "Start this task" });
  expect(machine.changes.size).toBe(0);
});

it.each(["guide", "queue"] as const)("sends an explicit %s once and leaves Enter bound to normal Send", async (delivery) => {
  vi.mocked(getSessionInputStatus).mockResolvedValue({ ...available, delivery_modes: ["start", "guide", "queue"] });
  render(<SessionComposer session={session} />); await ready();
  await waitFor(() => expect(machine.changes.size).toBe(1));
  draft("Keep going");
  expect(screen.getByText(/Guide may become a follow-up/)).toBeVisible();
  const action = screen.getByRole("button", { name: delivery === "guide" ? "Guide" : "Queue" });
  act(() => { fireEvent.click(action); fireEvent.click(action); });
  await waitFor(() => expect(field()).toHaveValue(""));
  expect(submitSessionInput).toHaveBeenCalledExactlyOnceWith({ session_key: session.session_key, request_id: expect.any(String), text: "Keep going", delivery }, machine.invoke);
  await ready();
  draft("A new turn");
  fireEvent.keyDown(field(), { key: "Enter", ctrlKey: true });
  await waitFor(() => expect(submitSessionInput).toHaveBeenCalledTimes(2));
  expect(vi.mocked(submitSessionInput).mock.calls[1][0]).toEqual({ session_key: session.session_key, request_id: expect.any(String), text: "A new turn" });
});

it("uses primary Send for advisory guidance with Queue as the explicit secondary action", async () => {
  render(<SessionComposer session={session} />); await ready();
  draft("Guide the current work");
  expect(screen.queryByRole("button", { name: "Guide" })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Queue" })).toBeEnabled();
  const send = screen.getByRole("button", { name: "Send" });
  expect(send).toBeEnabled();
  expect(send).toHaveAttribute("title", "Start if idle, guide active work when possible, or queue a follow-up.");
  expect(screen.getByText(/Send may guide active work or become a follow-up/)).toBeVisible();
  fireEvent.click(send);
  await waitFor(() => expect(submitSessionInput).toHaveBeenCalledTimes(1));
  expect(vi.mocked(submitSessionInput).mock.calls[0][0].delivery).toBeUndefined();
});

it("refreshes newly available modes after the first accepted Send without activating them during editing", async () => {
  let bound = false;
  vi.mocked(getSessionInputStatus).mockImplementation(async () => bound ? available : { ...available, delivery_modes: [] });
  vi.mocked(submitSessionInput).mockImplementation(async (request) => {
    bound = true;
    return { request_id: request.request_id, status: "accepted", message: "Message admitted" };
  });
  render(<SessionComposer session={session} />); await ready();
  draft("First message");
  expect(screen.queryByRole("button", { name: "Guide" })).not.toBeInTheDocument();
  expect(submitSessionInput).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Send" }));
  await screen.findByRole("button", { name: "Queue" });
  expect(vi.mocked(submitSessionInput).mock.calls[0][0].delivery).toBeUndefined();
});

it("withdraws unsupported actions on a runtime change while preserving the draft", async () => {
  const view = render(<SessionComposer session={session} />); await ready();
  await waitFor(() => expect(machine.changes.size).toBe(1));
  draft("Preserve this guidance");
  vi.mocked(getSessionInputStatus).mockResolvedValue({ ...available, delivery_modes: [] });
  changed();
  await waitFor(() => expect(screen.queryByRole("button", { name: "Queue" })).not.toBeInTheDocument());
  expect(screen.queryByRole("button", { name: "Queue" })).not.toBeInTheDocument();
  expect(field()).toHaveValue("Preserve this guidance");
  expect(submitSessionInput).not.toHaveBeenCalled();
  view.unmount();
  expect(machine.changes.size).toBe(0);
});

it("revalidates explicit delivery after asynchronous preparation without changing its intent", async () => {
  const staging = deferred<string[]>();
  vi.mocked(uploadImageDrafts).mockReturnValue(staging.promise);
  render(<SessionComposer session={session} />); await ready();
  await waitFor(() => expect(machine.changes.size).toBe(1));
  draft("Queue this work");
  fireEvent.click(screen.getByRole("button", { name: "Queue" }));
  vi.mocked(getSessionInputStatus).mockResolvedValue({ ...available, delivery_modes: [] });
  changed();
  await waitFor(() => expect(screen.queryByRole("button", { name: "Queue" })).not.toBeInTheDocument());
  await act(async () => staging.resolve([]));
  expect(submitSessionInput).not.toHaveBeenCalled();
  expect(field()).toHaveValue("Queue this work");
  expect(screen.getByText(/This delivery mode is no longer available/)).toBeVisible();
});

it("keeps an uncertain Queue locked through runtime updates without repeating it", async () => {
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({ request_id: request.request_id, status: "unknown", message: "Queue admission was not confirmed" }));
  render(<SessionComposer session={session} />); await ready();
  await waitFor(() => expect(machine.changes.size).toBe(1));
  draft("One follow-up");
  fireEvent.click(screen.getByRole("button", { name: "Queue" }));
  await screen.findByText(/Delivery not confirmed/);
  changed();
  expect(field()).toHaveValue("One follow-up");
  expect(screen.getByRole("button", { name: "Queue" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  expect(submitSessionInput).toHaveBeenCalledTimes(1);
});
