import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ManagedPiDialog, ManagedSessionControls, RegisterProjectDialog } from "./ProjectWorkspace";
import type { ManagedSessionSnapshot, RegisteredProject } from "../lib/types";

afterEach(cleanup);
function snapshot(overrides: Partial<ManagedSessionSnapshot> = {}): ManagedSessionSnapshot {
  return { revision: 1, request_id: "request", project_id: "repo", provider: "pi", status: "running", session_id: null, session_key: null, session: null, message: null, pending_dialog: null, ...overrides };
}

describe("project workspace dialogs", () => {
  it("keeps a registration draft on failure and prevents duplicate writes", async () => {
    let reject!: (error: Error) => void;
    const register = vi.fn(() => new Promise<RegisteredProject>((_resolve, failure) => { reject = failure; }));
    render(<RegisterProjectDialog open on_close={vi.fn()} on_registered={vi.fn()} on_register={register} />);
    fireEvent.change(screen.getByRole("textbox", { name: "Folder path" }), { target: { value: "/missing" } });
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    fireEvent.submit(screen.getByRole("textbox", { name: "Folder path" }).closest("form")!);
    expect(register).toHaveBeenCalledOnce();
    reject(new Error("Folder does not exist"));
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("Folder does not exist"));
    expect(screen.getByRole("textbox", { name: "Folder path" })).toHaveValue("/missing");
  });




  it("keeps stopped owned history resumable without enabling unrelated history controls", () => {
    const control = vi.fn();
    const { rerender } = render(<ManagedSessionControls snapshot={null} busy={false} error={null} on_control={control} />);
    expect(screen.queryByRole("button", { name: "Resume Pi" })).not.toBeInTheDocument();
    const stopped = snapshot({ status: "stopped" });
    rerender(<ManagedSessionControls snapshot={stopped} busy={false} error={null} on_control={control} />);
    fireEvent.click(screen.getByRole("button", { name: "Resume Pi" }));
    expect(control).toHaveBeenCalledWith("resume", stopped);
  });

  it("keeps an uncertain native-creation warning visible after its planned history can open", () => {
    const pending = snapshot({ provider: "codex", creation_phase: "native_create_uncertain", creation_input_state: "not_attempted", message: "Native creation was not confirmed. The first message was not sent." });
    const control = vi.fn();
    const { rerender } = render(<ManagedSessionControls snapshot={pending} busy={false} error={null} on_control={control} />);
    expect(screen.getByRole("alert")).toHaveTextContent("Native creation was not confirmed");
    const opened = { ...pending, revision: 2, session_id: "planned-id", session_key: "codex:planned-id", session: {
      session_key: "codex:planned-id", session_id: "planned-id", parent_session_id: null, is_subagent: false, provider: "codex" as const,
      title: "Saved native session", preview: null, project: "Repo", cwd: "/repo", updated_at_ms: 1, timestamp: null,
      agent_path: null, agent_nickname: null, agent_role: null, child_count: 0, message_count: null, event_count: null, history_status: null, has_unread: false,
    } };
    rerender(<ManagedSessionControls snapshot={opened} busy={false} error={null} on_control={control} />);
    expect(screen.getByRole("alert")).toHaveTextContent("The first message was not sent");
    expect(screen.queryByRole("button", { name: "Resume Pi" })).not.toBeInTheDocument();
    expect(control).not.toHaveBeenCalled();
  });

  it("shows a failed turn on a healthy idle Pi process without offering Resume", () => {
    render(<ManagedSessionControls snapshot={snapshot({ status: "idle", last_turn_error: "Connection error" })}
      busy={false} error={null} on_control={vi.fn()} />);
    expect(screen.getByRole("alert", { name: "Last turn failed" })).toHaveTextContent("Connection error");
    expect(screen.getByRole("status")).toHaveTextContent("Pi · Idle");
    expect(screen.queryByText("Pi · Ready")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Resume Pi" })).not.toBeInTheDocument();
  });

  it("retains Resume for a stopped process with a failed last turn", () => {
    const control = vi.fn();
    const stopped = snapshot({ status: "stopped", last_turn_error: "Connection error" });
    render(<ManagedSessionControls snapshot={stopped} busy={false} error={null} on_control={control} />);
    expect(screen.getByRole("alert", { name: "Last turn failed" })).toHaveTextContent("Connection error");
    fireEvent.click(screen.getByRole("button", { name: "Resume Pi" }));
    expect(control).toHaveBeenCalledWith("resume", stopped);
  });

  it("clears the failed-turn alert when a new turn starts and retains Stop", () => {
    const control = vi.fn();
    const { rerender } = render(<ManagedSessionControls snapshot={snapshot({ status: "idle", last_turn_error: "Connection error" })}
      busy={false} error={null} on_control={control} />);
    const running = snapshot({ revision: 2, status: "running", last_turn_error: null });
    rerender(<ManagedSessionControls snapshot={running} busy={false} error={null} on_control={control} />);
    expect(screen.queryByRole("alert", { name: "Last turn failed" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(control).toHaveBeenCalledWith("stop", running);
    rerender(<ManagedSessionControls snapshot={snapshot({ revision: 3, status: "idle" })} busy={false} error={null} on_control={control} />);
    expect(screen.getByRole("status")).toHaveTextContent("Pi · Ready");
  });


  it("cancels confirmation explicitly and never grants through dismissal", () => {
    const respond = vi.fn();
    render(<ManagedPiDialog snapshot={snapshot({ pending_dialog: { dialog_id: "permission", kind: "confirm", title: "Allow operation?", message: "Pi extension asks for access", options: [], initial_value: null } })}
      busy={false} error={null} on_respond={respond} />);
    expect(respond).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Close Allow operation?" }));
    expect(respond).toHaveBeenCalledExactlyOnceWith({ request_id: "request", dialog_id: "permission", cancelled: true, confirmed: false });
  });

  it.each(["select", "input", "editor"])("submits an explicit %s extension answer", (kind) => {
    const respond = vi.fn();
    render(<ManagedPiDialog snapshot={snapshot({ pending_dialog: { dialog_id: "question", kind, title: "Pi question", message: null, options: ["First", "Second"], initial_value: kind === "editor" ? "Draft" : null } })}
      busy={false} error={null} on_respond={respond} />);
    const field = screen.getByRole(kind === "select" ? "combobox" : "textbox");
    fireEvent.change(field, { target: { value: kind === "select" ? "Second" : "Written answer" } });
    fireEvent.click(screen.getByRole("button", { name: "Submit" }));
    expect(respond).toHaveBeenCalledExactlyOnceWith({ request_id: "request", dialog_id: "question", cancelled: false, value: kind === "select" ? "Second" : "Written answer" });
  });
});
