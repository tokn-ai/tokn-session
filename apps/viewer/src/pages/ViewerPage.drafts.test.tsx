import { useCallback, useState, type ReactNode } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { SessionDraftsProvider } from "../components/SessionDraftsProvider";
import type { SessionDraft } from "../lib/useSessionDrafts";
import type { CreateProjectSessionRequest, ManagedSessionDialogResponse, ManagedSessionSnapshot, RegisteredProject, SessionSummary } from "../lib/types";
import { ViewerPage } from "./ViewerPage";

const calls = vi.hoisted(() => ({ clear: vi.fn(), select: vi.fn(), open_created: vi.fn(), respond: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("../lib/transport", async (original) => ({ ...await original<typeof import("../lib/transport")>(), isDesktop: () => true,
  captureTransport: () => ({ invoke, listen, release: invoke, on_close: () => () => {} }),
}));
vi.mock("../components/TranslationProvider", () => ({ TranslationProvider: ({ children }: { children: ReactNode }) => children }));
vi.mock("../components/QuestionAnswersProvider", () => ({ QuestionAnswersProvider: ({ children }: { children: ReactNode }) => children }));
vi.mock("../components/SessionDrawer", () => ({ SessionDrawer: ({ children }: { children: ReactNode }) => children }));
vi.mock("../components/Inspector", () => ({ Inspector: () => null }));
vi.mock("../components/StatusBar", () => ({ StatusBar: () => null }));
vi.mock("../components/SettingsDialog", () => ({ SettingsDialog: () => null }));
vi.mock("../components/RelayConnection", () => ({ RelayConnection: () => null }));
vi.mock("../components/Conversation", () => ({ Conversation: ({ session }: { session: SessionSummary | null }) =>
  <main aria-label="Saved conversation">{session?.title ?? "Choose a session"}</main> }));
vi.mock("../components/Sidebar", () => ({ Sidebar: ({ drafts = [], on_new_session, on_draft_select, on_session_select }: {
  drafts?: SessionDraft[]; on_new_session: () => void; on_draft_select: (draft_id: string) => void; on_session_select: (key: string) => void;
}) => <nav aria-label="Sessions">
  <button onClick={on_new_session}>New session</button><button onClick={() => on_session_select("pi:existing")}>Existing session</button>
  {drafts.map((draft) => <button key={draft.draft_id} data-testid="draft-row" onClick={() => on_draft_select(draft.draft_id)}>{draft.prompt || "Empty draft"}</button>)}
</nav> }));
vi.mock("../components/ProjectWorkspace", () => ({
  RegisterProjectDialog: () => null,
  ManagedSessionControls: () => null,
  ManagedPiDialog: ({ snapshot, on_respond }: { snapshot: ManagedSessionSnapshot; on_respond: (response: ManagedSessionDialogResponse) => void }) =>
    <div role="dialog" aria-label={snapshot.pending_dialog!.title}>
      <button onClick={() => on_respond({ request_id: snapshot.request_id, dialog_id: snapshot.pending_dialog!.dialog_id, confirmed: true, cancelled: false })}>Continue setup</button>
    </div>,
}));

const project: RegisteredProject = { project_id: "project", group_key: "/repo", folder_path: "/repo", name: "Repo", created_at_ms: 1, available: true };
const existing: SessionSummary = {
  session_key: "pi:existing", session_id: "existing", parent_session_id: null, is_subagent: false, provider: "pi", title: "Existing task", preview: "Hello",
  project: "Repo", cwd: "/repo", updated_at_ms: 1, timestamp: null, agent_path: null, agent_nickname: null, agent_role: null,
  child_count: 0, message_count: null, event_count: null, history_status: null, has_unread: false,
};
const created: SessionSummary = { ...existing, session_key: "pi:created", session_id: "created", title: "Created task" };
const workspace = {
  workspace: { available: true, unavailable_reason: null, projects: [project] }, managed: null, pending_dialog_session: null,
  controlling: false, control_error: null, error: null, input_refresh_key: 0, controlErrorFor: () => null, control: vi.fn(), respond: calls.respond,
  loadCreationOptions: async (project_id: string) => ({ project_id, providers: [
    { provider: "pi" as const, available: true, reason: null, supports_images: true, default_profile_id: null, runtimes: [] },
    { provider: "qoder" as const, available: true, reason: null, supports_images: true, default_profile_id: null, runtimes: [] },
  ] }),
};
vi.mock("../lib/useProjectWorkspace", () => ({ useProjectWorkspace: () => workspace }));
vi.mock("../lib/useViewerState", () => ({ useViewerState: () => {
  const [session, setSession] = useState<SessionSummary | null>(existing);
  const clearSessionSelection = useCallback(() => { calls.clear(); setSession(null); }, []);
  const selectSession = useCallback((session_key: string) => { calls.select(session_key); setSession(existing); }, []);
  const openCreatedSession = useCallback((next: SessionSummary) => { calls.open_created(next); setSession(next); }, []);
  return { selectedSessionKey: session?.session_key ?? null, selectedSession: session,
    clearSessionSelection, selectSession, selectQuestionSession: selectSession, openCreatedSession };
} }));

let emit: (event: { payload: ManagedSessionSnapshot }) => void;
const receipts = new Map<string, ManagedSessionSnapshot>();
function pending(request_id: string): ManagedSessionSnapshot {
  return { revision: 1, request_id, project_id: "project", provider: "pi", status: "starting", session_id: null,
    session_key: null, session: null, message: null, pending_dialog: null, creation_input_state: "not_attempted" };
}
function publish(snapshot: ManagedSessionSnapshot) {
  receipts.set(snapshot.request_id, snapshot);
  emit({ payload: snapshot });
}
function firstRequest(): CreateProjectSessionRequest {
  return (vi.mocked(invoke).mock.calls.find(([command]) => command === "create_project_session")![1] as { request: CreateProjectSessionRequest }).request;
}
async function startDraft(prompt: string, provider?: "qoder") {
  fireEvent.click(screen.getByRole("button", { name: "New session" }));
  if (provider) fireEvent.change(screen.getByRole("combobox", { name: "Agent" }), { target: { value: provider } });
  fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: prompt } });
  await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeEnabled());
  fireEvent.click(screen.getByRole("button", { name: "Send" }));
  await waitFor(() => expect(screen.getByRole("button", { name: "Check session start" })).toBeEnabled());
  return firstRequest().request_id;
}
beforeEach(() => {
  vi.stubGlobal("__TAURI_INTERNALS__", {});
  vi.mocked(listen).mockImplementation(async (name, handler) => {
    if (name === "managed-session-changed") emit = handler as typeof emit;
    return () => {};
  });
  vi.mocked(invoke).mockImplementation(async (command, payload) => {
    const request = (payload as { request: { request_id: string } }).request;
    if (command === "create_project_session") {
      const snapshot = pending(request.request_id);
      receipts.set(request.request_id, snapshot);
      return snapshot;
    }
    if (command === "get_managed_session") return receipts.get(request.request_id) ?? null;
    throw new Error(`Unexpected draft command: ${command}`);
  });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); receipts.clear(); vi.clearAllMocks(); });

describe("draft selection in the viewer", () => {
  it("clears native selection and keeps multiple draft texts while navigating saved sessions", async () => {
    render(<SessionDraftsProvider enabled><ViewerPage /></SessionDraftsProvider>);
    fireEvent.click(screen.getByRole("button", { name: "New session" }));
    expect(calls.clear).toHaveBeenCalledOnce();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("");
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "First draft" } });
    fireEvent.click(screen.getByRole("button", { name: "Existing session" }));
    expect(screen.getByRole("main", { name: "Saved conversation" })).toHaveTextContent("Existing task");
    fireEvent.click(screen.getByRole("button", { name: "New session" }));
    fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Second draft" } });
    fireEvent.click(screen.getByRole("button", { name: "First draft" }));
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("First draft");
    fireEvent.click(screen.getByRole("button", { name: "Second draft" }));
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Second draft");
    expect(screen.getAllByTestId("draft-row")).toHaveLength(2);
    expect(calls.select).toHaveBeenCalledExactlyOnceWith("pi:existing");
    expect(calls.open_created).not.toHaveBeenCalled();
    expect(invoke).not.toHaveBeenCalled();
    await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeEnabled());
  });

  it("opens the verified native conversation only after its selected draft's first input is accepted", async () => {
    render(<SessionDraftsProvider enabled><ViewerPage /></SessionDraftsProvider>);
    const request_id = await startDraft("First message");
    act(() => publish({ ...pending(request_id), revision: 2, session_key: created.session_key, session: created, creation_input_state: "uncertain" }));
    expect(calls.open_created).not.toHaveBeenCalled();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("First message");
    act(() => publish({ ...pending(request_id), revision: 3, session_key: created.session_key, session: created, creation_input_state: "accepted" }));
    expect(calls.open_created).toHaveBeenCalledExactlyOnceWith(created);
    expect(screen.getByRole("main", { name: "Saved conversation" })).toHaveTextContent("Created task");
    expect(screen.queryByTestId("draft-row")).not.toBeInTheDocument();
    expect(calls.select).not.toHaveBeenCalled();
  });

  it("answers a first Qoder permission and shows streamed work before initial admission completes while retaining its draft", async () => {
    const original = vi.mocked(invoke).getMockImplementation()!;
    const native = { ...created, provider: "qoder" as const, session_key: "qoder:created" };
    let response_sent = false;
    vi.mocked(invoke).mockImplementation(async (command, payload) => {
      if (command === "get_session_control") return { session_key: native.session_key, provider: "qoder", transport: "rpc",
        can_select: false, can_cancel: true, available: true, is_running: true, message: "", runtime_id: "runtime", instance_id: "instance",
        live_output: { prompt_id: "managed-window", truncated: false, parts: [{ kind: "text", text: "Checking the fixture" }] } };
      if (command === "list_session_interactions") return { session_key: native.session_key, runtime_id: "runtime", instance_id: "instance", interactions: [{
        interaction_id: "native-permission", kind: "permission", title: "Read fixture?", details: "cat fixture.txt", questions: [],
        permission_decisions: ["allow_once", "deny"], response_pending: response_sent,
      }] };
      if (command === "respond_session_interaction") {
        response_sent = true;
        return { request_id: (payload as { request: { request_id: string } }).request.request_id, status: "accepted", message: "Response sent" };
      }
      return original(command, payload);
    });
    render(<SessionDraftsProvider enabled><ViewerPage /></SessionDraftsProvider>);
    const request_id = await startDraft("Inspect this fixture", "qoder");
    expect(screen.queryByRole("region", { name: "Session activity" })).not.toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalledWith("list_session_interactions", expect.anything());
    act(() => publish({ ...pending(request_id), provider: "qoder", revision: 2, session_key: "qoder:another-session",
      session: native, creation_phase: "history_pending", creation_input_state: "not_attempted" }));
    expect(screen.queryByRole("region", { name: "Session activity" })).not.toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalledWith("list_session_interactions", expect.anything());
    act(() => publish({ ...pending(request_id), provider: "qoder", revision: 3, session_key: native.session_key,
      session: native, creation_phase: "history_pending", creation_input_state: "not_attempted" }));
    expect(await screen.findByRole("button", { name: "Allow once" })).toBeEnabled();
    expect(vi.mocked(invoke).mock.calls.map(([command]) => command)).toContain("get_session_control");
    expect(await screen.findByRole("button", { name: "Stop" })).toBeEnabled();
    expect(await screen.findByText("Checking the fixture")).toBeVisible();
    expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Inspect this fixture");
    expect(screen.getByRole("textbox", { name: "First message" })).toBeDisabled();
    expect(calls.open_created).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Allow once" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("respond_session_interaction", { request: {
      session_key: native.session_key, request_id: expect.any(String), runtime_id: "runtime", instance_id: "instance",
      interaction_id: "native-permission", response: { type: "permission", decision: "allow_once" },
    } }));
    expect(screen.getByTestId("draft-row")).toHaveTextContent("Inspect this fixture");
    expect(calls.open_created).not.toHaveBeenCalled();
    act(() => publish({ ...pending(request_id), provider: "qoder", revision: 4, session_key: native.session_key,
      session: native, creation_phase: "ready", creation_input_state: "accepted" }));
    expect(calls.open_created).toHaveBeenCalledExactlyOnceWith(native);
    expect(screen.queryByTestId("draft-row")).not.toBeInTheDocument();
    expect(vi.mocked(invoke).mock.calls.filter(([command]) => command === "create_project_session")).toHaveLength(1);
    expect(vi.mocked(invoke).mock.calls.filter(([command]) => command === "submit_session_input")).toHaveLength(0);
  });

  it.each(["native", "draft"] as const)("does not steal the selected %s conversation when another draft completes", async (destination) => {
    render(<SessionDraftsProvider enabled><ViewerPage /></SessionDraftsProvider>);
    const request_id = await startDraft("Background task");
    if (destination === "native") fireEvent.click(screen.getByRole("button", { name: "Existing session" }));
    else {
      fireEvent.click(screen.getByRole("button", { name: "New session" }));
      fireEvent.change(screen.getByRole("textbox", { name: "First message" }), { target: { value: "Keep writing here" } });
    }
    act(() => publish({ ...pending(request_id), revision: 2, session_key: created.session_key, session: created, creation_input_state: "accepted" }));
    expect(calls.open_created).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: "Background task" })).not.toBeInTheDocument();
    if (destination === "native") expect(screen.getByRole("main", { name: "Saved conversation" })).toHaveTextContent("Existing task");
    else {
      expect(screen.getByRole("textbox", { name: "First message" })).toHaveValue("Keep writing here");
      await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeEnabled());
    }
  });

  it("recovers a retained Pi setup dialog after leaving and remounting the local viewer", async () => {
    const view = render(<SessionDraftsProvider enabled><ViewerPage /></SessionDraftsProvider>);
    const request_id = await startDraft("Needs setup");
    act(() => publish({ ...pending(request_id), revision: 2, pending_dialog: {
      dialog_id: "setup-question", kind: "confirm", title: "Approve setup", message: null, options: [], initial_value: null,
    } }));
    expect(screen.getByRole("dialog", { name: "Approve setup" })).toBeInTheDocument();
    view.rerender(<SessionDraftsProvider enabled><div>Machines</div></SessionDraftsProvider>);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    view.rerender(<SessionDraftsProvider enabled><ViewerPage /></SessionDraftsProvider>);
    expect(screen.getByRole("dialog", { name: "Approve setup" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Continue setup" }));
    expect(calls.respond).toHaveBeenCalledExactlyOnceWith({ request_id, dialog_id: "setup-question", confirmed: true, cancelled: false });
    expect(calls.open_created).not.toHaveBeenCalled();
  });
});
