import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ManagedSessionSnapshot, RegisteredProject, SessionCreationOptions, SessionSummary } from "../lib/types";
import type { SessionDraft } from "../lib/useSessionDrafts";
import { DraftConversation, type DraftConversationProps } from "./DraftConversation";

afterEach(cleanup);

const project: RegisteredProject = { project_id: "repo", folder_path: "/repo", group_key: "/repo", name: "Repo", created_at_ms: 1, available: true };
function options(project_id = "repo"): SessionCreationOptions {
  return { project_id, providers: [
    { provider: "pi", available: true, reason: null, supports_images: true, default_profile_id: null, runtimes: [] },
    { provider: "codex", available: true, reason: null, supports_images: true, default_profile_id: "local", runtimes: [
      { profile_id: "local", revision: 3, name: "Local Codex", available: true, reason: null },
      { profile_id: "missing", revision: 1, name: "Missing executable", available: false, reason: "Executable unavailable" },
    ] },
    { provider: "dsh", available: true, reason: null, supports_images: false, default_profile_id: null, runtimes: [] },
  ] };
}
function draft(overrides: Partial<SessionDraft> = {}): SessionDraft {
  return { draft_id: "draft-one", project_id: "repo", selection: { provider: "pi" }, model: null, prompt: "", images: [], snapshot: null,
    sending: false, uncertain: false, locked: false, can_discard: true, error: null, image_error: null, ...overrides };
}
function snapshot(overrides: Partial<ManagedSessionSnapshot> = {}): ManagedSessionSnapshot {
  return { revision: 1, request_id: "request", project_id: "repo", provider: "codex", status: "running", session_id: null,
    session_key: null, session: null, message: null, pending_dialog: null, ...overrides };
}
function props(overrides: Partial<DraftConversationProps> = {}): DraftConversationProps {
  return { draft: draft(), projects: [project], workspace_available: true, on_change: vi.fn(), on_add_images: vi.fn(),
    on_remove_image: vi.fn(), on_send: vi.fn(), on_check: vi.fn(), on_discard: vi.fn(), on_add_project: vi.fn(),
    on_sidebar_open: vi.fn(), on_load_options: vi.fn(async (project_id) => options(project_id)),
    on_load_models: vi.fn(async () => ({ models: [], catalog_revision: null, runtime_profile_id: null, runtime_revision: null, reason: "Model discovery is unsupported." })), ...overrides };
}
async function ready() { await waitFor(() => expect(screen.queryByText("Checking agent configuration…")).not.toBeInTheDocument()); }
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>((done) => { resolve = done; }); return { promise, resolve }; }

describe("draft conversation", () => {
  it("does not suggest saved RPC profiles to repair managed Pi configuration", async () => {
    const configured = options();
    configured.providers[0] = { ...configured.providers[0], available: false, reason: "Managed Pi is unavailable." };
    render(<DraftConversation {...props({ on_load_options: vi.fn(async () => configured), on_configure_runtime: vi.fn() })} />);
    await ready();
    expect(screen.getByText("Managed Pi is unavailable.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Configure agent runtime" })).not.toBeInTheDocument();
  });

  it("loads models on opening the picker, keeps selection local, and gates images by the selected model", async () => {
    const file = new File(["png"], "draft.png", { type: "image/png" });
    const chosen = { model: { model_provider: "provider", model_id: "text-only", display_name: "Text model" }, supports_images: false };
    const input = props({ on_load_models: vi.fn(async () => ({ models: [chosen], catalog_revision: "verified",
      runtime_profile_id: null, runtime_revision: null, reason: null })) });
    function Stateful() {
      const [value, setValue] = useState(draft({ prompt: "Inspect", images: [{ draft_id: "image", file, url: "blob:screen" }] }));
      return <DraftConversation {...input} draft={value} on_change={(patch) => setValue((previous) => ({ ...previous, ...patch }))} />;
    }
    render(<Stateful />); await ready();
    expect(input.on_load_models).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: /Model: Runtime default/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Text model provider/ }));
    expect(input.on_load_models).toHaveBeenCalledExactlyOnceWith({ project_id: "repo", provider: "pi" });
    expect(input.on_send).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    expect(screen.getByText(/This model cannot accept these images/)).toBeInTheDocument();
    expect(screen.getByAltText("draft.png")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Use runtime default" }));
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    expect(screen.queryByText("Recorded")).not.toBeInTheDocument();
  });

  it("allows sending with runtime default after model catalog failure", async () => {
    const input = props({ draft: draft({ prompt: "Task" }), on_load_models: vi.fn(async () => { throw new Error("Catalog failed"); }) });
    render(<DraftConversation {...input} />); await ready();
    fireEvent.click(screen.getByRole("button", { name: /Model: Runtime default/ }));
    await screen.findByText("Catalog failed");
    expect(screen.getByRole("button", { name: "Retry models" })).toBeEnabled();
    expect(screen.getByRole("button", { name: /Runtime default Use the agent/ })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(input.on_send).toHaveBeenCalledOnce();
  });

  it("refreshes a reopened catalog and lets the same model be selected with its new proof", async () => {
    const chosen = { model: { model_provider: "provider", model_id: "chosen", display_name: "Chosen" }, supports_images: true };
    const load = vi.fn().mockResolvedValueOnce({ models: [chosen], catalog_revision: "first", runtime_profile_id: null, runtime_revision: null, reason: null })
      .mockResolvedValueOnce({ models: [chosen], catalog_revision: "second", runtime_profile_id: null, runtime_revision: null, reason: null });
    const input = props({ on_load_models: load });
    function Stateful() {
      const [value, setValue] = useState(draft({ prompt: "Task" }));
      return <DraftConversation {...input} draft={value} on_change={(patch) => setValue((previous) => ({ ...previous, ...patch }))} />;
    }
    render(<Stateful />); await ready();
    fireEvent.click(screen.getByRole("button", { name: /Model: Runtime default/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Chosen provider/ }));
    fireEvent.click(screen.getByRole("button", { name: "Close model selection" }));
    fireEvent.click(screen.getByRole("button", { name: /Model: Chosen/ }));
    await screen.findByText(/The model catalog changed/);
    expect(load).toHaveBeenCalledTimes(2);
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: /Chosen provider/ }));
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    expect(screen.queryByText(/The model catalog changed/)).not.toBeInTheDocument();
    expect(input.on_send).not.toHaveBeenCalled();
  });

  it("opens an empty conversation and only checks configuration until the first Send", async () => {
    const input = props();
    render(<DraftConversation {...input} />);
    await ready();
    expect(screen.getByRole("heading", { name: "New session" })).toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "First message" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.submit(screen.getByRole("form", { name: "Send the first message" }));
    expect(input.on_load_options).toHaveBeenCalledExactlyOnceWith("repo");
    expect(input.on_send).not.toHaveBeenCalled();
    expect(input.on_check).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Open sessions" }));
    expect(input.on_sidebar_open).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Discard draft" }));
    expect(input.on_discard).toHaveBeenCalledOnce();
  });

  it("keeps the composer editable with no project and allows adding a folder", () => {
    const input = props({ draft: draft({ project_id: "", prompt: "A task for later" }), projects: [] });
    render(<DraftConversation {...input} />);
    expect(input.on_load_options).not.toHaveBeenCalled();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("A task for later");
    expect(screen.getByRole("textbox", { name: "First message" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Add project" }));
    expect(input.on_add_project).toHaveBeenCalledOnce();
  });

  it("preserves draft text and attachments while changing provider and clears only the runtime choice", async () => {
    const file = new File(["png"], "draft.png", { type: "image/png" });
    const initial = draft({ selection: { provider: "codex", runtime_profile_id: "local", runtime_revision: 3 }, prompt: "Inspect the screenshot", images: [{ draft_id: "image", file, url: "blob:screen" }] });
    const input = props();
    function Stateful() {
      const [value, setValue] = useState(initial);
      return <DraftConversation {...input} draft={value} on_change={(patch) => setValue((previous) => ({ ...previous, ...patch }))} />;
    }
    render(<Stateful />); await ready();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "dsh" } });
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Inspect the screenshot");
    expect(screen.getByAltText("draft.png")).toHaveAttribute("src", "blob:screen");
    expect(screen.getByText("DSH cannot accept these images. Remove them or choose an agent with image support.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: "codex" } });
    expect(screen.getByRole("combobox", { name: "Runtime" })).toHaveValue("");
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(input.on_send).toHaveBeenCalledOnce();
  });

  it("waits for verified options, ignores obsolete results, and requires reselecting a changed runtime", async () => {
    const first = deferred<SessionCreationOptions>();
    const second = deferred<SessionCreationOptions>();
    const load = vi.fn((project_id: string) => project_id === "repo" ? first.promise : second.promise);
    const input = props({ draft: draft({ prompt: "Task", selection: { provider: "codex", runtime_profile_id: "local", runtime_revision: 2 } }), on_load_options: load });
    const view = render(<DraftConversation {...input} />);
    await waitFor(() => expect(load).toHaveBeenCalledOnce());
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    const next = { ...input, draft: { ...input.draft, project_id: "other" }, projects: [{ ...project, project_id: "other" }] };
    view.rerender(<DraftConversation {...next} />);
    await act(async () => first.resolve(options()));
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    await act(async () => second.resolve(options("other")));
    expect(screen.getByText("The selected runtime changed. Choose a runtime again before sending.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.change(screen.getByRole("combobox", { name: "Runtime" }), { target: { value: "local" } });
    expect(input.on_change).toHaveBeenCalledWith({ selection: { provider: "codex", runtime_profile_id: "local", runtime_revision: 3 } });
  });

  it("retains text while configuration fails and retries without starting a session", async () => {
    const load = vi.fn().mockRejectedValueOnce(new Error("Runtime unavailable")).mockResolvedValueOnce(options());
    const input = props({ draft: draft({ prompt: "Keep this task" }), on_load_options: load });
    render(<DraftConversation {...input} />);
    await screen.findByText("Runtime unavailable");
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Retry configuration" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeEnabled());
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Keep this task");
    expect(input.on_send).not.toHaveBeenCalled();
  });

  it.each(["codex", "open_code", "dsh"] as const)("uses a verified explicit %s runtime when automatic selection is unavailable", async (provider) => {
    const available = options();
    available.providers = [{ provider, available: false, reason: "Choose an explicit runtime", supports_images: false,
      default_profile_id: null, runtimes: [{ profile_id: "verified", revision: 9, name: "Configured runtime", available: true, reason: null }] }];
    const input = props({ on_load_options: async () => available });
    function Stateful() {
      const [value, setValue] = useState(draft({ prompt: "Task", selection: { provider } }));
      return <DraftConversation {...input} draft={value} on_change={(patch) => setValue((previous) => ({ ...previous, ...patch }))} />;
    }
    render(<Stateful />); await ready();
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    expect(screen.getByText("Choose an explicit runtime")).toBeInTheDocument();
    fireEvent.change(screen.getByRole("combobox", { name: "Runtime" }), { target: { value: "verified" } });
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(input.on_send).toHaveBeenCalledOnce();
  });

  it("supports image-only messages and delegates attachment ownership to the draft controller", async () => {
    const file = new File(["png"], "screen.png", { type: "image/png" });
    const input = props({ draft: draft({ images: [{ draft_id: "image", file, url: "blob:screen" }] }) });
    const revoke = vi.fn();
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revoke });
    const view = render(<DraftConversation {...input} />); await ready();
    expect(screen.getByRole("button", { name: "Send" })).toBeEnabled();
    const field = screen.getByRole("textbox", { name: "First message" });
    expect(screen.getByRole("list", { name: "Attached images" }).compareDocumentPosition(field) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(field.compareDocumentPosition(screen.getByRole("button", { name: "Attach images" })) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    fireEvent.paste(screen.getByRole("textbox", { name: "First message" }), { clipboardData: { files: [file] } });
    expect(input.on_add_images).toHaveBeenCalledWith([file]);
    fireEvent.drop(screen.getByRole("form", { name: "Send the first message" }), { dataTransfer: { types: ["Files"], files: [file] } });
    expect(input.on_add_images).toHaveBeenCalledTimes(2);
    fireEvent.click(screen.getByRole("button", { name: "Remove screen.png" }));
    expect(input.on_remove_image).toHaveBeenCalledWith("image");
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(input.on_send).toHaveBeenCalledOnce();
    view.unmount();
    expect(revoke).not.toHaveBeenCalled();
  });

  it("checks uncertain creation without replaying or losing the first message", async () => {
    const input = props({ draft: draft({ prompt: "Original task", locked: true, can_discard: false, uncertain: true,
      snapshot: snapshot({ session: { session_key: "codex:one" } as SessionSummary, creation_input_state: "uncertain", creation_phase: "first_input_uncertain" }) }), on_open_session: vi.fn() });
    render(<DraftConversation {...input} />); await ready();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Original task");
    expect(screen.getByRole("textbox", { name: "First message" })).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "Project folder" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Add project" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Discard draft" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Check session start" }));
    expect(input.on_check).toHaveBeenCalledOnce();
    expect(input.on_send).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Open saved conversation" }));
    expect(input.on_open_session).toHaveBeenCalledOnce();
  });

  it("keeps terminal rejected input visible and allows an explicitly safe discard", async () => {
    const input = props({ draft: draft({ prompt: "Rejected task", locked: true, can_discard: true,
      snapshot: snapshot({ status: "failed", creation_input_state: "rejected", creation_phase: "failed" }) }) });
    render(<DraftConversation {...input} />); await ready();
    expect(screen.getByText("The session was created, but the first message was not accepted. Your draft is retained.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Discard draft" })).toBeEnabled();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Rejected task");
  });

  it("keeps first-turn failures visible and renders existing managed controls", async () => {
    const stop = vi.fn();
    const input = props({ draft: draft({ prompt: "Task", locked: true, can_discard: false,
      snapshot: snapshot({ provider: "pi", status: "idle", last_turn_error: "Connection error" }) }),
    managed_controls: <button type="button" onClick={stop}>Stop</button> });
    render(<DraftConversation {...input} />); await ready();
    expect(screen.getByText("Connection error")).toBeInTheDocument();
    expect(screen.getByText("Waiting for the conversation to be saved. You can return to this draft later.")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(stop).toHaveBeenCalledOnce();
  });

  it("guards keyboard submission against IME, limits, pending state, and empty input", async () => {
    const input = props({ draft: draft({ prompt: "Task" }) });
    const view = render(<DraftConversation {...input} />); await ready();
    const field = screen.getByRole("textbox", { name: "First message" });
    fireEvent.keyDown(field, { key: "Enter", ctrlKey: true, isComposing: true });
    fireEvent.keyDown(field, { key: "Enter", metaKey: true, keyCode: 229 });
    expect(input.on_send).not.toHaveBeenCalled();
    fireEvent.keyDown(field, { key: "Enter", metaKey: true });
    expect(input.on_send).toHaveBeenCalledOnce();
    view.rerender(<DraftConversation {...input} draft={draft({ prompt: "x".repeat(16_385) })} />);
    expect(field).toHaveAttribute("aria-invalid", "true");
    fireEvent.keyDown(field, { key: "Enter", ctrlKey: true });
    expect(input.on_send).toHaveBeenCalledOnce();
    view.rerender(<DraftConversation {...input} draft={draft({ prompt: "Task", sending: true, locked: true, can_discard: false })} />);
    fireEvent.submit(screen.getByRole("form", { name: "Send the first message" }));
    expect(input.on_check).not.toHaveBeenCalled();
    expect(input.on_send).toHaveBeenCalledOnce();
  });

  it("keeps unavailable folders visible with their explanation and blocks native creation", () => {
    const input = props({ projects: [{ ...project, available: false, unavailable_reason: "No saved sessions reference this folder." }], draft: draft({ prompt: "Task" }) });
    render(<DraftConversation {...input} />);
    expect(input.on_load_options).not.toHaveBeenCalled();
    expect(screen.getAllByText("No saved sessions reference this folder.").length).toBeGreaterThan(0);
    expect(screen.getByRole("button", { name: "Send" })).toBeDisabled();
    expect(screen.getByRole("textbox", { name: "First message" })).toBeEnabled();
  });
});
