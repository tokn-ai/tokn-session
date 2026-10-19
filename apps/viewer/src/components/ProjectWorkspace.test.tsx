import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ManagedPiDialog, ManagedSessionControls, NewSessionDialog, RegisterProjectDialog } from "./ProjectWorkspace";
import type { ManagedSessionSnapshot, RegisteredProject } from "../lib/types";

afterEach(cleanup);
const project = (project_id = "repo", folder_path = "/repo"): RegisteredProject => ({ project_id, folder_path, group_key: "/repo", name: "Repo", created_at_ms: 1, available: true });
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

  it("chooses between registered worktree folders and retains an unconfirmed prompt", async () => {
    const create = vi.fn(async () => {});
    const props = { open: true, projects: [project(), project("task", "/trees/task/repo")], snapshot: null,
      creating: false, error: null, control_error: null, controlling: false, on_close: vi.fn(), on_create: create, on_stop: vi.fn(), on_open: vi.fn() };
    const { rerender } = render(<NewSessionDialog {...props} />);
    fireEvent.change(screen.getByRole("combobox", { name: "Project folder" }), { target: { value: "task" } });
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Fix the app" } });
    fireEvent.click(screen.getByRole("button", { name: "Start Pi session" }));
    expect(create).toHaveBeenCalledWith("task", "Fix the app");
    rerender(<NewSessionDialog {...props} uncertain error="Session start was not confirmed." />);
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Fix the app");
    expect(screen.getByRole("combobox", { name: "Project folder" })).toHaveValue("task");
    expect(screen.getByRole("button", { name: "Check session start" })).toBeEnabled();
  });

  it("shows starting until durable history exists and offers Stop", () => {
    const stop = vi.fn();
    render(<NewSessionDialog open projects={[project()]} snapshot={snapshot()} creating={false} error={null} control_error={null} controlling={false}
      on_close={vi.fn()} on_create={vi.fn()} on_stop={stop} on_open={vi.fn()} />);
    expect(screen.getByRole("status")).toHaveTextContent("Waiting for its conversation to be saved");
    expect(screen.getByRole("textbox", { name: "First message" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(stop).toHaveBeenCalledOnce();
  });

  it("preserves an unconfirmed request’s original folder when reopened from another project", () => {
    const create = vi.fn(async () => {});
    const original = project("first", "/repo");
    const other = { ...project("second", "/other"), group_key: "/other", name: "Other" };
    const props = { open: true, projects: [original, other], preferred_group_key: "/repo", snapshot: null,
      creating: false, error: null, control_error: null, controlling: false, on_close: vi.fn(), on_create: create, on_stop: vi.fn(), on_open: vi.fn() };
    const { rerender } = render(<NewSessionDialog {...props} />);
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Original prompt" } });
    fireEvent.click(screen.getByRole("button", { name: "Start Pi session" }));
    rerender(<NewSessionDialog {...props} open={false} uncertain error="Acknowledgement lost" />);
    rerender(<NewSessionDialog {...props} preferred_group_key="/other" uncertain error="Acknowledgement lost" />);
    expect(screen.getByRole("combobox", { name: "Project folder" })).toHaveValue("first");
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Original prompt");
    fireEvent.click(screen.getByRole("button", { name: "Check session start" }));
    expect(create).toHaveBeenLastCalledWith("first", "Original prompt");
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
