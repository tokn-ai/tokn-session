import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useState, type ComponentProps } from "react";
import { ManagedPiDialog, ManagedSessionControls, NewSessionDialog as CreationDialog, RegisterProjectDialog } from "./ProjectWorkspace";
import type { ManagedSessionSnapshot, RegisteredProject, RegisterProjectRequest, SessionCreationOptions } from "../lib/types";

afterEach(cleanup);
const project = (project_id = "repo", folder_path = "/repo"): RegisteredProject => ({ project_id, folder_path, group_key: "/repo", name: "Repo", created_at_ms: 1, available: true });
function creationOptions(project_id = "repo"): SessionCreationOptions {
  return { project_id, providers: [
    { provider: "pi", available: true, reason: null, supports_images: true, default_profile_id: null, runtimes: [] },
    ...(["codex", "open_code", "dsh"] as const).map((provider) => ({ provider, available: true, reason: null, supports_images: provider !== "dsh", default_profile_id: `${provider}-default`, runtimes: [
      { profile_id: `${provider}-default`, revision: 7, name: "Default runtime", available: true, reason: null },
      { profile_id: `${provider}-other`, revision: 7, name: "Other runtime", available: true, reason: null },
      { profile_id: `${provider}-missing`, revision: 7, name: "Missing executable", available: false, reason: "Executable is unavailable" },
    ] })),
  ] };
}
const loadOptions = async (project_id: string) => creationOptions(project_id);
function NewSessionDialog(props: Omit<ComponentProps<typeof CreationDialog>, "on_load_options"> & Partial<Pick<ComponentProps<typeof CreationDialog>, "on_load_options">>) {
  return <CreationDialog on_load_options={loadOptions} {...props} />;
}
async function optionsReady() { await waitFor(() => expect(screen.queryByText("Checking agent configuration…")).not.toBeInTheDocument()); }
function deferred<T>() { let resolve!: (value: T) => void; let reject!: (error: Error) => void; const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
function snapshot(overrides: Partial<ManagedSessionSnapshot> = {}): ManagedSessionSnapshot {
  return { revision: 1, request_id: "request", project_id: "repo", provider: "pi", status: "running", session_id: null, session_key: null, session: null, message: null, pending_dialog: null, ...overrides };
}
const creationProps = () => ({ open: true, projects: [project()], snapshot: null, creating: false, error: null,
  control_error: null, controlling: false, on_close: vi.fn(), on_create: vi.fn(async () => {}), on_stop: vi.fn(), on_open: vi.fn() });

describe("agent session creation", () => {
  it("opens with an empty registry and offers folder registration without querying agent options", () => {
    const load = vi.fn(loadOptions);
    const register = vi.fn();
    render(<NewSessionDialog {...creationProps()} projects={[]} on_load_options={load} on_register={register} />);
    expect(screen.getByText("No folders are available. Add an existing folder to start a session.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Start Pi session" })).toBeDisabled();
    expect(load).not.toHaveBeenCalled();
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Keep my first task" } });
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    expect(screen.getByRole("dialog", { name: "Add project" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: "New session" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Keep my first task");
    expect(register).not.toHaveBeenCalled();
  });

  it("selects an added folder and preserves the provider, prompt, and attachments", async () => {
    Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => "blob:creation") }, revokeObjectURL: { configurable: true, value: vi.fn() } });
    const props = creationProps();
    const added = project("worktree", "/trees/task/repo");
    const register = vi.fn(async (_request: RegisterProjectRequest) => added);
    const load = vi.fn(loadOptions);
    function Workspace() {
      const [projects, setProjects] = useState([project()]);
      return <NewSessionDialog {...props} projects={projects} on_load_options={load} on_register={async (request) => {
        const registered = await register(request);
        setProjects((current) => [...current, registered]);
        return registered;
      }} />;
    }
    render(<Workspace />);
    await optionsReady();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "codex" } });
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Inspect the screenshot" } });
    fireEvent.change(screen.getByLabelText("Image files"), { target: { files: [new File(["png"], "draft.png", { type: "image/png" })] } });
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Folder path" }), { target: { value: added.folder_path } });
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Project folder" })).toHaveValue("worktree"));
    await optionsReady();
    expect(register).toHaveBeenCalledExactlyOnceWith({ folder_path: "/trees/task/repo", name: null });
    expect(load).toHaveBeenLastCalledWith("worktree");
    expect(screen.getByRole("combobox", { name: "Agent" })).toHaveValue("codex");
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Inspect the screenshot");
    expect(screen.getByRole("button", { name: "Remove draft.png" })).toBeInTheDocument();
    expect(URL.revokeObjectURL).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Start Codex session" }));
    expect(props.on_create).toHaveBeenCalledWith("worktree", "Inspect the screenshot", [expect.objectContaining({ file: expect.objectContaining({ name: "draft.png" }) })], { provider: "codex" });
  });

  it("explains unavailable folders and does not query their agent options", () => {
    const load = vi.fn(loadOptions);
    const reason = "No saved sessions reference this folder. Add it manually to keep using it.";
    render(<NewSessionDialog {...creationProps()} projects={[{ ...project(), available: false, origin: "discovered", discovery_state: "absent", unavailable_reason: reason }]} on_load_options={load} on_register={vi.fn()} />);
    expect(screen.getByRole("option", { name: "Repo · /repo (unavailable)" })).toBeDisabled();
    fireEvent.click(screen.getByText("1 unavailable folder"));
    expect(screen.getByText(reason)).toBeVisible();
    expect(screen.getByRole("button", { name: "Add project" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Start Pi session" })).toBeDisabled();
    expect(load).not.toHaveBeenCalled();
  });

  it("ignores a late registration result after returning to an existing folder and starting a session", async () => {
    const pending_registration = deferred<RegisteredProject>();
    const props = { ...creationProps(), projects: [project(), project("other", "/other")], on_register: vi.fn(() => pending_registration.promise) };
    const { rerender } = render(<NewSessionDialog {...props} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Use the original folder" } });
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Folder path" }), { target: { value: "/other" } });
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    fireEvent.click(screen.getByRole("button", { name: "Start Pi session" }));
    rerender(<NewSessionDialog {...props} creating />);
    await act(async () => pending_registration.resolve(project("other", "/other")));
    expect(screen.getByRole("combobox", { name: "Project folder" })).toHaveValue("repo");
    expect(props.on_create).toHaveBeenCalledExactlyOnceWith("repo", "Use the original folder", [], { provider: "pi" });
  });

  it.each(["creating", "uncertain", "admitted", "controlling"])("prevents changing or adding folders while %s", async (state) => {
    const props = creationProps();
    const register = vi.fn();
    const { rerender } = render(<NewSessionDialog {...props} on_register={register} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Original task" } });
    const locked = { creating: state === "creating", uncertain: state === "uncertain", controlling: state === "controlling",
      snapshot: state === "admitted" ? snapshot({ provider: "codex", status: "failed", creation_phase: "native_create_uncertain" }) : null };
    rerender(<NewSessionDialog {...props} {...locked} on_register={register} />);
    expect(screen.getByRole("combobox", { name: "Project folder" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Add project" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    expect(screen.queryByRole("dialog", { name: "Add project" })).not.toBeInTheDocument();
    expect(register).not.toHaveBeenCalled();
    expect(screen.getByRole("combobox", { name: "Project folder" })).toHaveValue("repo");
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Original task");
  });

  it("waits for host options and retains the draft when loading fails before retry", async () => {
    const pending = deferred<SessionCreationOptions>();
    const load = vi.fn().mockReturnValueOnce(pending.promise).mockResolvedValue(creationOptions());
    const props = creationProps();
    render(<NewSessionDialog {...props} on_load_options={load} />);
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Keep this draft" } });
    expect(screen.getByRole("status")).toHaveTextContent("Checking agent configuration");
    expect(screen.getByRole("button", { name: "Start Pi session" })).toBeDisabled();
    await act(async () => pending.reject(new Error("Runtime settings are unavailable")));
    expect(screen.getByRole("alert")).toHaveTextContent("Runtime settings are unavailable");
    expect(props.on_create).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Start Pi session" })).toBeEnabled());
    expect(load).toHaveBeenNthCalledWith(2, "repo");
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Keep this draft");
  });

  it.each([ ["codex", "Codex"], ["open_code", "OpenCode"], ["dsh", "DSH"] ])("selects %s and sends an explicit runtime revision", async (provider, name) => {
    const props = creationProps();
    render(<NewSessionDialog {...props} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: provider } });
    expect(screen.getByRole("combobox", { name: "Runtime" })).toHaveValue("");
    expect(screen.getByRole("option", { name: "Missing executable (unavailable)" })).toBeDisabled();
    fireEvent.change(screen.getByRole("combobox", { name: "Runtime" }), { target: { value: `${provider}-other` } });
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Create a task" } });
    fireEvent.click(screen.getByRole("button", { name: `Start ${name} session` }));
    expect(props.on_create).toHaveBeenCalledExactlyOnceWith("repo", "Create a task", [], { provider, runtime_profile_id: `${provider}-other`, runtime_revision: 7 });
  });

  it("requires an explicit runtime when the host cannot choose a default", async () => {
    const choices = creationOptions();
    const codex = choices.providers.find((provider) => provider.provider === "codex")!;
    codex.available = false; codex.default_profile_id = null; codex.reason = "Choose a runtime for this project";
    const load = vi.fn(async () => choices);
    const props = creationProps();
    render(<NewSessionDialog {...props} on_load_options={load} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "codex" } });
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Use this project" } });
    expect(screen.getByRole("alert")).toHaveTextContent("Choose a runtime for this project");
    expect(screen.getByRole("button", { name: "Start Codex session" })).toBeDisabled();
    fireEvent.change(screen.getByRole("combobox", { name: "Runtime" }), { target: { value: "codex-other" } });
    expect(screen.getByRole("button", { name: "Start Codex session" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "Start Codex session" }));
    expect(props.on_create).toHaveBeenCalledWith("repo", "Use this project", [], { provider: "codex", runtime_profile_id: "codex-other", runtime_revision: 7 });
  });

  it("retains images and prompt while DSH blocks unsupported attachments", async () => {
    Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => "blob:creation") }, revokeObjectURL: { configurable: true, value: vi.fn() } });
    const props = creationProps();
    render(<NewSessionDialog {...props} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Inspect the screenshot" } });
    fireEvent.change(screen.getByLabelText("Image files"), { target: { files: [new File(["png"], "draft.png", { type: "image/png" })] } });
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "dsh" } });
    expect(screen.getByRole("alert")).toHaveTextContent("DSH cannot accept these images");
    expect(screen.getByRole("button", { name: "Attach images" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Start DSH session" })).toBeDisabled();
    fireEvent.submit(screen.getByRole("textbox", { name: "First message" }).closest("form")!);
    expect(props.on_create).not.toHaveBeenCalled();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Inspect the screenshot");
    expect(screen.getByRole("button", { name: "Remove draft.png" })).toBeInTheDocument();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "codex" } });
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Start Codex session" })).toBeEnabled();
    expect(URL.revokeObjectURL).not.toHaveBeenCalled();
  });

  it("keeps a failed creation draft and locks every input until an uncertain request is checked", async () => {
    const props = creationProps();
    const { rerender } = render(<NewSessionDialog {...props} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Original task" } });
    fireEvent.click(screen.getByRole("button", { name: "Start Pi session" }));
    rerender(<NewSessionDialog {...props} error="Folder is unavailable" snapshot={snapshot({ status: "failed" })} />);
    expect(screen.getByRole("button", { name: "Try again" })).toBeEnabled();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Original task");
    rerender(<NewSessionDialog {...props} />);
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "open_code" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Runtime" }), { target: { value: "open_code-other" } });
    fireEvent.click(screen.getByRole("button", { name: "Start OpenCode session" }));
    rerender(<NewSessionDialog {...props} uncertain error="Acknowledgement lost" />);
    for (const name of ["Agent", "Project folder", "Runtime"]) expect(screen.getByRole("combobox", { name })).toBeDisabled();
    expect(screen.getByRole("textbox", { name: "First message" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Attach images" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Check session start" }));
    expect(props.on_create).toHaveBeenLastCalledWith("repo", "Original task", [], { provider: "open_code", runtime_profile_id: "open_code-other", runtime_revision: 7 });
  });

  it.each(["success", "failure"])("ignores a stale folder options %s after selecting another folder", async (outcome) => {
    const stale = deferred<SessionCreationOptions>();
    const current = deferred<SessionCreationOptions>();
    const load = vi.fn((project_id: string) => project_id === "repo" ? stale.promise : current.promise);
    render(<NewSessionDialog {...creationProps()} projects={[project(), project("other", "/other")]} on_load_options={load} />);
    fireEvent.change(screen.getByRole("combobox", { name: "Project folder" }), { target: { value: "other" } });
    await act(async () => current.resolve(creationOptions("other")));
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Current project" } });
    expect(screen.getByRole("button", { name: "Start Pi session" })).toBeEnabled();
    await act(async () => {
      if (outcome === "success") stale.resolve(creationOptions("repo"));
      else stale.reject(new Error("Old folder disappeared"));
    });
    expect(screen.queryByText("Old folder disappeared")).not.toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Project folder" })).toHaveValue("other");
    expect(screen.getByRole("button", { name: "Start Pi session" })).toBeEnabled();
  });

  it("keeps an admitted failed RPC receipt locked and offers to check that same start", async () => {
    const props = creationProps();
    const { rerender } = render(<NewSessionDialog {...props} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "codex" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Runtime" }), { target: { value: "codex-other" } });
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "One native session" } });
    fireEvent.click(screen.getByRole("button", { name: "Start Codex session" }));
    rerender(<NewSessionDialog {...props} snapshot={snapshot({ provider: "codex", status: "failed", creation_phase: "failed", message: "Native session exists; input failed" })} />);
    expect(screen.getByRole("combobox", { name: "Agent" })).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "Runtime" })).toBeDisabled();
    expect(screen.getByRole("textbox", { name: "First message" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Check session start" }));
    expect(props.on_create).toHaveBeenLastCalledWith("repo", "One native session", [], { provider: "codex", runtime_profile_id: "codex-other", runtime_revision: 7 });
  });

  it("checks the original runtime identity while reopened options are loading or have changed", async () => {
    const refreshed = deferred<SessionCreationOptions>();
    const load = vi.fn().mockResolvedValueOnce(creationOptions()).mockReturnValueOnce(refreshed.promise);
    const props = creationProps();
    const { rerender } = render(<NewSessionDialog {...props} on_load_options={load} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "codex" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Runtime" }), { target: { value: "codex-other" } });
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Original" } });
    fireEvent.click(screen.getByRole("button", { name: "Start Codex session" }));
    rerender(<NewSessionDialog {...props} on_load_options={load} open={false} uncertain />);
    rerender(<NewSessionDialog {...props} on_load_options={load} uncertain />);
    fireEvent.click(screen.getByRole("button", { name: "Check session start" }));
    expect(props.on_create).toHaveBeenLastCalledWith("repo", "Original", [], { provider: "codex", runtime_profile_id: "codex-other", runtime_revision: 7 });
    const changed = creationOptions();
    changed.providers.find((provider) => provider.provider === "codex")!.runtimes.forEach((runtime) => { runtime.revision = 8; });
    await act(async () => refreshed.resolve(changed));
    fireEvent.click(screen.getByRole("button", { name: "Check session start" }));
    expect(props.on_create).toHaveBeenLastCalledWith("repo", "Original", [], { provider: "codex", runtime_profile_id: "codex-other", runtime_revision: 7 });
  });

  it("retains an unadmitted RPC draft after history appears and clears it only after input acceptance", async () => {
    Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => "blob:unadmitted") }, revokeObjectURL: { configurable: true, value: vi.fn() } });
    const props = creationProps();
    const { rerender } = render(<NewSessionDialog {...props} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "codex" } });
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Keep the unsent prompt" } });
    fireEvent.change(screen.getByLabelText("Image files"), { target: { files: [new File(["png"], "unsent.png", { type: "image/png" })] } });
    fireEvent.click(screen.getByRole("button", { name: "Start Codex session" }));
    const history = snapshot({ provider: "codex", session_id: "planned", session_key: "codex:planned", creation_phase: "native_create_uncertain", creation_input_state: "not_attempted", session: {
      session_key: "codex:planned", session_id: "planned", parent_session_id: null, is_subagent: false, provider: "codex",
      title: "Saved native session", preview: null, project: "Repo", cwd: "/repo", updated_at_ms: 1, timestamp: null,
      agent_path: null, agent_nickname: null, agent_role: null, child_count: 0, message_count: null, event_count: null, history_status: null, has_unread: false,
    } });
    rerender(<NewSessionDialog {...props} snapshot={history} />);
    expect(screen.getByRole("button", { name: "Open conversation" })).toBeEnabled();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Keep the unsent prompt");
    expect(screen.getByRole("button", { name: "Remove unsent.png" })).toBeInTheDocument();
    expect(URL.revokeObjectURL).not.toHaveBeenCalled();
    rerender(<NewSessionDialog {...props} snapshot={{ ...history, revision: 2, creation_phase: "ready", creation_input_state: "accepted" }} />);
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("");
    expect(screen.queryByRole("button", { name: "Remove unsent.png" })).not.toBeInTheDocument();
    expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:unadmitted");
    expect(props.on_create).toHaveBeenCalledOnce();
  });
});
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
    await optionsReady();
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Fix the app" } });
    fireEvent.click(screen.getByRole("button", { name: "Start Pi session" }));
    expect(create).toHaveBeenCalledWith("task", "Fix the app", [], { provider: "pi" });
    rerender(<NewSessionDialog {...props} uncertain error="Session start was not confirmed." />);
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Fix the app");
    expect(screen.getByRole("combobox", { name: "Project folder" })).toHaveValue("task");
    expect(screen.getByRole("button", { name: "Check session start" })).toBeEnabled();
  });

  it("shows starting until durable history exists and offers Stop", async () => {
    const stop = vi.fn();
    render(<NewSessionDialog open projects={[project()]} snapshot={snapshot()} creating={false} error={null} control_error={null} controlling={false}
      on_close={vi.fn()} on_create={vi.fn()} on_stop={stop} on_open={vi.fn()} />);
    await optionsReady();
    expect(screen.getByRole("status")).toHaveTextContent("Waiting for its conversation to be saved");
    expect(screen.getByRole("textbox", { name: "First message" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(stop).toHaveBeenCalledOnce();
  });

  it("preserves an unconfirmed request’s original folder when reopened from another project", async () => {
    const create = vi.fn(async () => {});
    const original = project("first", "/repo");
    const other = { ...project("second", "/other"), group_key: "/other", name: "Other" };
    const props = { open: true, projects: [original, other], preferred_group_key: "/repo", snapshot: null,
      creating: false, error: null, control_error: null, controlling: false, on_close: vi.fn(), on_create: create, on_stop: vi.fn(), on_open: vi.fn() };
    const { rerender } = render(<NewSessionDialog {...props} />);
    await optionsReady();
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Original prompt" } });
    fireEvent.click(screen.getByRole("button", { name: "Start Pi session" }));
    rerender(<NewSessionDialog {...props} open={false} uncertain error="Acknowledgement lost" />);
    rerender(<NewSessionDialog {...props} preferred_group_key="/other" uncertain error="Acknowledgement lost" />);
    expect(screen.getByRole("combobox", { name: "Project folder" })).toHaveValue("first");
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Original prompt");
    fireEvent.click(screen.getByRole("button", { name: "Check session start" }));
    expect(create).toHaveBeenLastCalledWith("first", "Original prompt", [], { provider: "pi" });
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

  it("keeps a failed first turn visible while waiting to open its persisted history", () => {
    render(<NewSessionDialog open projects={[project()]} snapshot={snapshot({ status: "idle", last_turn_error: "Connection error" })}
      creating={false} error={null} control_error={null} controlling={false} on_close={vi.fn()} on_create={vi.fn()} on_stop={vi.fn()} on_open={vi.fn()} />);
    expect(screen.getByRole("alert", { name: "Last turn failed" })).toHaveTextContent("Connection error");
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

it("allows an image-only first Pi message with removable previews", async () => {
  Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => "blob:pi") }, revokeObjectURL: { configurable: true, value: vi.fn() } });
  const create = vi.fn(async () => {});
  render(<NewSessionDialog open projects={[project()]} snapshot={null} creating={false} error={null} control_error={null} controlling={false}
    on_close={vi.fn()} on_create={create} on_stop={vi.fn()} on_open={vi.fn()} />);
  await optionsReady();
  fireEvent.change(screen.getByLabelText("Image files"), { target: { files: [new File(["png"], "new.png", { type: "image/png" })] } });
  expect(screen.getByRole("textbox", { name: "First message" })).not.toBeRequired();
  fireEvent.click(screen.getByRole("button", { name: "Start Pi session" }));
  expect(create).toHaveBeenCalledWith("repo", "", [expect.objectContaining({ file: expect.any(File), url: "blob:pi" })], { provider: "pi" });
  fireEvent.click(screen.getByRole("button", { name: "Remove new.png" }));
  expect(screen.getByRole("button", { name: "Start Pi session" })).toBeDisabled();
  expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:pi");
});

it("places Pi attachment previews above the first message while keeping its picker below", async () => {
  Object.defineProperties(URL, { createObjectURL: { configurable: true, value: vi.fn(() => "blob:pi") }, revokeObjectURL: { configurable: true, value: vi.fn() } });
  render(<NewSessionDialog open projects={[project()]} snapshot={null} creating={false} error={null} control_error={null} controlling={false}
    on_close={vi.fn()} on_create={vi.fn()} on_stop={vi.fn()} on_open={vi.fn()} />);
  await optionsReady();
  fireEvent.change(screen.getByLabelText("Image files"), { target: { files: [new File(["png"], "new.png", { type: "image/png" })] } });
  const previews = screen.getByRole("list", { name: "Attached images" });
  const prompt = screen.getByRole("textbox", { name: "First message" });
  const picker = screen.getByRole("button", { name: "Attach images" });
  expect(previews.compareDocumentPosition(prompt) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(prompt.compareDocumentPosition(picker) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
});
