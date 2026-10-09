import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getSessionInputStatus, submitSessionInput } from "../lib/tauri";
import type { SessionInputStatus, SessionSummary, SubmitSessionInputResponse } from "../lib/types";
import { SessionComposer } from "./SessionComposer";

vi.mock("../lib/tauri", () => ({ getSessionInputStatus: vi.fn(), submitSessionInput: vi.fn() }));

const SESSION: SessionSummary = {
  session_key: "codex:root", session_id: "root", parent_session_id: null,
  is_subagent: false, provider: "codex", title: "First session", preview: null,
  project: null, cwd: null, updated_at_ms: null, timestamp: null,
  agent_path: null, agent_nickname: null, agent_role: null, child_count: 0,
  message_count: null, event_count: null, history_status: null, has_unread: false,
};
const OTHER_SESSION = { ...SESSION, session_key: "codex:other", session_id: "other" };
const AVAILABLE: SessionInputStatus = { available: true, message: "", max_length: 16_384 };

function deferred<T>() {
  let resolve!: (result: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function textbox() { return screen.getByRole("textbox", { name: "Message this session" }); }
function draft(text: string) { fireEvent.change(textbox(), { target: { value: text } }); }
function send() { fireEvent.click(screen.getByRole("button", { name: "Send" })); }
async function ready() { await waitFor(() => expect(textbox()).toBeEnabled()); }

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(getSessionInputStatus).mockResolvedValue(AVAILABLE);
  vi.mocked(submitSessionInput).mockImplementation(async (request) => ({
    request_id: request.request_id, status: "accepted", message: "Message sent.",
  }));
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("SessionComposer", () => {
  it("shows no input and makes no availability request without a selected session", () => {
    render(<SessionComposer session={null} />);
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    expect(getSessionInputStatus).not.toHaveBeenCalled();
  });

  it("waits for availability and explains an unsupported session with an explicit retry", async () => {
    const availability = deferred<SessionInputStatus>();
    vi.mocked(getSessionInputStatus).mockReturnValueOnce(availability.promise);
    render(<SessionComposer session={SESSION} />);
    expect(textbox()).toBeDisabled();
    expect(screen.getByText("Checking message availability…")).toBeInTheDocument();
    await act(async () => availability.resolve({ ...AVAILABLE, available: false, message: "Open this session in Codex App first." }));
    expect(textbox()).toBeDisabled();
    expect(screen.getByText("Open this session in Codex App first.")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Retry availability" }));
    await ready();
    expect(getSessionInputStatus).toHaveBeenLastCalledWith({ session_key: SESSION.session_key });
    expect(submitSessionInput).not.toHaveBeenCalled();
  });

  it("handles an older or disconnected server without enabling sends", async () => {
    vi.mocked(getSessionInputStatus).mockRejectedValue(new Error("Unknown command"));
    render(<SessionComposer session={SESSION} />);
    await screen.findByText("Message input is unavailable on this connection. Retry to check availability. Reason: Unknown command");
    expect(textbox()).toBeDisabled();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
  });

  it("rejects blank drafts and submits multiline text exactly, clearing it only after acceptance", async () => {
    const pending = deferred<SubmitSessionInputResponse>();
    vi.mocked(submitSessionInput).mockReturnValue(pending.promise);
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft(" \n  ");
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    draft("  First line\n第二行  ");
    send();
    expect(submitSessionInput).toHaveBeenCalledExactlyOnceWith({
      session_key: SESSION.session_key, request_id: expect.any(String), text: "  First line\n第二行  ",
    });
    expect(textbox()).toHaveValue("  First line\n第二行  ");
    expect(textbox()).toBeDisabled();
    expect(screen.getByRole("button", { name: "Sending…" })).toBeDisabled();
    await act(async () => pending.resolve({ request_id: vi.mocked(submitSessionInput).mock.calls[0][0].request_id, status: "accepted", message: "Message sent." }));
    expect(textbox()).toHaveValue("");
    expect(screen.getByText("Message sent.")).toBeInTheDocument();
  });

  it("counts Unicode characters, preserves an over-limit draft, and disables submission", async () => {
    vi.mocked(getSessionInputStatus).mockResolvedValue({ ...AVAILABLE, max_length: 3 });
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft("你好🙂");
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    draft("你好🙂!");
    expect(textbox()).toHaveValue("你好🙂!");
    expect(textbox()).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByText("4 / 3 characters")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.keyDown(textbox(), { key: "Enter", ctrlKey: true });
    expect(submitSessionInput).not.toHaveBeenCalled();
  });

  it.each(["ctrlKey", "metaKey"])("sends with %s + Enter but leaves Enter and composing keystrokes alone", async (modifier) => {
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft("Message");
    fireEvent.keyDown(textbox(), { key: "Enter" });
    fireEvent.keyDown(textbox(), { key: "Enter", [modifier]: true, isComposing: true });
    fireEvent.keyDown(textbox(), { key: "Enter", [modifier]: true, keyCode: 229 });
    expect(submitSessionInput).not.toHaveBeenCalled();
    await act(async () => fireEvent.keyDown(textbox(), { key: "Enter", [modifier]: true }));
    expect(submitSessionInput).toHaveBeenCalledOnce();
  });

  it("prevents duplicate form submissions before rendering the sending state", async () => {
    vi.mocked(submitSessionInput).mockReturnValue(new Promise(() => {}));
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft("Only once");
    const form = screen.getByRole("form");
    act(() => { fireEvent.submit(form); fireEvent.submit(form); });
    expect(submitSessionInput).toHaveBeenCalledOnce();
  });

  it("retains session drafts and scopes a completed send to its original target", async () => {
    const pending = deferred<SubmitSessionInputResponse>();
    vi.mocked(submitSessionInput).mockReturnValue(pending.promise);
    const view = render(<SessionComposer session={SESSION} />);
    await ready();
    draft("First session draft");
    view.rerender(<SessionComposer session={OTHER_SESSION} />);
    await ready();
    draft("Other session draft");
    view.rerender(<SessionComposer session={SESSION} />);
    await ready();
    expect(textbox()).toHaveValue("First session draft");
    send();
    view.rerender(<SessionComposer session={OTHER_SESSION} />);
    await ready();
    const request = vi.mocked(submitSessionInput).mock.calls[0][0];
    expect(request.session_key).toBe(SESSION.session_key);
    await act(async () => pending.resolve({ request_id: request.request_id, status: "accepted", message: "Message sent." }));
    expect(textbox()).toHaveValue("Other session draft");
    expect(screen.queryByText("Message sent.")).not.toBeInTheDocument();
    view.rerender(<SessionComposer session={SESSION} />);
    await ready();
    expect(textbox()).toHaveValue("");
    expect(screen.getByText("Message sent.")).toBeInTheDocument();
  });

  it.each(["unknown", "pending", "network", "mismatched"])("locks an unconfirmed %s result until explicit editing, with no automatic retry", async (status) => {
    vi.mocked(submitSessionInput).mockImplementation(async (request) => {
      if (status === "network") throw new Error("Connection lost");
      return { request_id: status === "mismatched" ? "different-request" : request.request_id,
        status: status === "mismatched" ? "accepted" : status as "unknown" | "pending", message: "" };
    });
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft("Possibly sent");
    send();
    const notice = await screen.findByText(/^Delivery not confirmed\. Check the conversation before sending again\./);
    if (status === "network") expect(notice).toHaveTextContent("Reason: Connection lost");
    if (status === "mismatched") expect(notice).toHaveTextContent("Reason: The server response did not match this message.");
    expect(textbox()).toHaveValue("Possibly sent");
    expect(textbox()).toBeDisabled();
    fireEvent.submit(screen.getByRole("form"));
    expect(submitSessionInput).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Edit message" }));
    expect(textbox()).toBeEnabled();
    expect(submitSessionInput).toHaveBeenCalledOnce();
    draft("Revised message");
    await act(async () => send());
    const requests = vi.mocked(submitSessionInput).mock.calls;
    expect(requests).toHaveLength(2);
    expect(requests[1][0].request_id).not.toBe(requests[0][0].request_id);
  });

  it.each(["unknown", "pending"] as const)("preserves backend diagnostics for %s delivery", async (status) => {
    vi.mocked(submitSessionInput).mockImplementation(async (request) => ({
      request_id: request.request_id, status, message: "Codex did not acknowledge the message before the connection closed.",
    }));
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft("Keep this draft");
    send();
    await screen.findByText("Delivery not confirmed. Check the conversation before sending again. Reason: Codex did not acknowledge the message before the connection closed.");
    expect(textbox()).toHaveValue("Keep this draft");
    expect(textbox()).toBeDisabled();
    expect(submitSessionInput).toHaveBeenCalledOnce();
  });

  it("preserves string errors from desktop command failures without retrying", async () => {
    vi.mocked(submitSessionInput).mockRejectedValue("Session input task failed: connection reset");
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft("Still possibly sent");
    send();
    await screen.findByText("Delivery not confirmed. Check the conversation before sending again. Reason: Session input task failed: connection reset");
    expect(textbox()).toBeDisabled();
    expect(submitSessionInput).toHaveBeenCalledOnce();
  });

  it("reports a request-ID preparation failure as definitely unsent and keeps the draft editable", async () => {
    vi.spyOn(crypto, "randomUUID").mockImplementation(() => { throw new Error("Random number generation is unavailable"); });
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft("Not submitted");
    send();
    await screen.findByText("Message was not sent. Could not prepare the message. Reason: Random number generation is unavailable");
    expect(textbox()).toHaveValue("Not submitted");
    expect(textbox()).toBeEnabled();
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    expect(screen.queryByRole("button", { name: "Edit message" })).not.toBeInTheDocument();
    expect(submitSessionInput).not.toHaveBeenCalled();
  });

  it("keeps a definitely unsent message editable with the failure reason", async () => {
    vi.mocked(submitSessionInput).mockImplementation(async (request) => ({
      request_id: request.request_id, status: "not_sent", message: "Session is no longer open.",
    }));
    render(<SessionComposer session={SESSION} />);
    await ready();
    draft("Still a draft");
    send();
    await screen.findByText("Session is no longer open.");
    expect(textbox()).toHaveValue("Still a draft");
    expect(textbox()).toBeEnabled();
    expect(screen.queryByRole("button", { name: "Edit message" })).not.toBeInTheDocument();
  });

  it("ignores an older availability response after returning to a session", async () => {
    const older = deferred<SessionInputStatus>();
    vi.mocked(getSessionInputStatus).mockReturnValueOnce(older.promise);
    const view = render(<SessionComposer session={SESSION} />);
    view.rerender(<SessionComposer session={OTHER_SESSION} />);
    await ready();
    view.rerender(<SessionComposer session={SESSION} />);
    await ready();
    await act(async () => older.resolve({ ...AVAILABLE, available: false, message: "Stale result" }));
    expect(textbox()).toBeEnabled();
    expect(screen.queryByText("Stale result")).not.toBeInTheDocument();
  });

  it("drops drafts when the viewer unmounts to switch machines", async () => {
    const view = render(<SessionComposer session={SESSION} />);
    await ready();
    draft("Local machine draft");
    view.unmount();
    render(<SessionComposer session={SESSION} />);
    await ready();
    expect(textbox()).toHaveValue("");
  });

  it("uses the latest acceptance callback with the original target after switching sessions", async () => {
    const pending = deferred<SubmitSessionInputResponse>();
    vi.mocked(submitSessionInput).mockReturnValue(pending.promise);
    const oldCallback = vi.fn();
    const latestCallback = vi.fn();
    const view = render(<SessionComposer session={SESSION} on_accepted={oldCallback} />);
    await ready();
    draft("First session message");
    send();
    view.rerender(<SessionComposer session={OTHER_SESSION} on_accepted={latestCallback} />);
    await ready();
    const request = vi.mocked(submitSessionInput).mock.calls[0][0];
    await act(async () => pending.resolve({ request_id: request.request_id, status: "accepted", message: "Message sent." }));
    expect(oldCallback).not.toHaveBeenCalled();
    expect(latestCallback).toHaveBeenCalledExactlyOnceWith(SESSION.session_key);
    expect(textbox()).toHaveValue("");
    expect(submitSessionInput).toHaveBeenCalledOnce();
  });

  it("does not refresh an unmounted viewer when acceptance arrives later", async () => {
    const pending = deferred<SubmitSessionInputResponse>();
    vi.mocked(submitSessionInput).mockReturnValue(pending.promise);
    const onAccepted = vi.fn();
    const view = render(<SessionComposer session={SESSION} on_accepted={onAccepted} />);
    await ready();
    draft("Message");
    send();
    view.unmount();
    const request = vi.mocked(submitSessionInput).mock.calls[0][0];
    await act(async () => pending.resolve({ request_id: request.request_id, status: "accepted", message: "Message sent." }));
    expect(onAccepted).not.toHaveBeenCalled();
  });

  it("keeps accepted delivery certain if the refresh callback fails", async () => {
    const onAccepted = vi.fn().mockRejectedValue(new Error("Refresh failed"));
    render(<SessionComposer session={SESSION} on_accepted={onAccepted} />);
    await ready();
    draft("Accepted message");
    send();
    await screen.findByText("Message sent. The conversation could not refresh.");
    expect(textbox()).toHaveValue("");
    expect(textbox()).toBeEnabled();
    expect(screen.queryByRole("button", { name: "Edit message" })).not.toBeInTheDocument();
    expect(submitSessionInput).toHaveBeenCalledOnce();
  });
});

it("starts as one line, expands on focus, and preserves a draft when focus leaves", async () => {
  render(<SessionComposer session={SESSION} />);
  await ready();
  expect(textbox()).toHaveAttribute("rows", "1");
  fireEvent.focus(textbox());
  expect(textbox()).toHaveAttribute("rows", "3");
  expect(screen.getByRole("button", { name: "Send" }).closest(".session-composer__box")).toContainElement(textbox());
  fireEvent.blur(textbox());
  expect(textbox()).toHaveAttribute("rows", "1");
  draft("Keep my draft\nand its newline");
  fireEvent.blur(textbox());
  expect(textbox()).toHaveAttribute("rows", "3");
  expect(textbox()).toHaveValue("Keep my draft\nand its newline");
  expect(submitSessionInput).not.toHaveBeenCalled();
});

it("opens a fresh session compactly and restores the previous session's expanded draft", async () => {
  const view = render(<SessionComposer session={SESSION} />);
  await ready();
  fireEvent.focus(textbox());
  draft("Saved draft");
  view.rerender(<SessionComposer session={OTHER_SESSION} />);
  await ready();
  expect(textbox()).toHaveAttribute("rows", "1");
  view.rerender(<SessionComposer session={SESSION} />);
  await ready();
  expect(textbox()).toHaveAttribute("rows", "3");
  expect(textbox()).toHaveValue("Saved draft");
});
