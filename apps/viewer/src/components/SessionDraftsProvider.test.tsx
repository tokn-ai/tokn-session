import { useEffect } from "react";
import { act, cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { SessionDraftsProvider, useLocalSessionDrafts } from "./SessionDraftsProvider";
import { selectMachine, type ViewerClient } from "../lib/transport";
import type { ManagedSessionSnapshot, SessionSummary } from "../lib/types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

let drafts: NonNullable<ReturnType<typeof useLocalSessionDrafts>>;
let emit: (event: { payload: ManagedSessionSnapshot }) => void;
const unlisten = vi.fn();
const summary: SessionSummary = {
  session_key: "pi:real", session_id: "real", parent_session_id: null, is_subagent: false, provider: "pi", title: "New task", preview: "Hello",
  project: "Repo", cwd: "/repo", updated_at_ms: 1, timestamp: null, agent_path: null, agent_nickname: null, agent_role: null,
  child_count: 0, message_count: null, event_count: null, history_status: null, has_unread: false,
};
function snapshot(request_id: string, complete = false): ManagedSessionSnapshot {
  return { request_id, revision: complete ? 2 : 1, project_id: "project", provider: "pi", status: complete ? "running" : "starting",
    session_id: complete ? "real" : null, session_key: complete ? "pi:real" : null, session: complete ? summary : null,
    creation_input_state: complete ? "accepted" : "not_attempted", message: null, pending_dialog: null };
}
function Consumer({ on_created }: { on_created?: (draft_id: string, session: SessionSummary) => void }) {
  const controller = useLocalSessionDrafts();
  if (controller) drafts = controller;
  useEffect(() => on_created && controller?.subscribeCreated(on_created), [controller?.subscribeCreated, on_created]);
  return null;
}
beforeEach(() => {
  vi.mocked(listen).mockImplementation(async (_name, handler) => {
    emit = handler as typeof emit;
    return unlisten;
  });
  vi.mocked(invoke).mockImplementation(async (command, payload) => {
    if (command === "create_project_session") return snapshot((payload as { request: { request_id: string } }).request.request_id);
    if (command === "get_managed_session") return null;
    throw new Error(`Unexpected command: ${command}`);
  });
});
afterEach(() => { cleanup(); selectMachine(); vi.clearAllMocks(); });

it("waits for local initialization and keeps creation on native transport while a remote machine is selected", async () => {
  const remote = { invoke: vi.fn(), listen: vi.fn(), close: vi.fn() } as unknown as ViewerClient;
  selectMachine(remote);
  const view = render(<SessionDraftsProvider enabled={false}><Consumer /></SessionDraftsProvider>);
  expect(listen).not.toHaveBeenCalled();
  expect(invoke).not.toHaveBeenCalled();
  view.rerender(<SessionDraftsProvider enabled><Consumer /></SessionDraftsProvider>);
  await waitFor(() => expect(listen).toHaveBeenCalledWith("managed-session-changed", expect.any(Function)));
  let draft_id = "";
  act(() => { draft_id = drafts.createDraft("project"); drafts.updateDraft(draft_id, { prompt: "First message" }); });
  expect(invoke).not.toHaveBeenCalled();
  await act(async () => { await drafts.sendDraft(draft_id); });
  expect(invoke).toHaveBeenCalledWith("create_project_session", { request: expect.objectContaining({ project_id: "project", prompt: "First message" }) });
  expect(remote.invoke).not.toHaveBeenCalled();
  expect(remote.listen).not.toHaveBeenCalled();
});

it("settles a local creation while its viewer is away without notifying an unmounted viewer", async () => {
  const on_created = vi.fn();
  const view = render(<SessionDraftsProvider enabled><Consumer on_created={on_created} /></SessionDraftsProvider>);
  await waitFor(() => expect(listen).toHaveBeenCalledTimes(1));
  let draft_id = "";
  act(() => { draft_id = drafts.createDraft("project"); drafts.updateDraft(draft_id, { prompt: "Continue in background" }); });
  await act(async () => { await drafts.sendDraft(draft_id); });
  const request_id = drafts.drafts[0].snapshot!.request_id;
  view.rerender(<SessionDraftsProvider enabled><Consumer /></SessionDraftsProvider>);
  act(() => emit({ payload: snapshot(request_id, true) }));
  expect(drafts.drafts).toHaveLength(0);
  expect(on_created).not.toHaveBeenCalled();
  expect(unlisten).not.toHaveBeenCalled();
  view.unmount();
  expect(unlisten).toHaveBeenCalledOnce();
});
