import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { SessionDraft } from "../lib/useSessionDrafts";
import type { RegisteredProject } from "../lib/types";
import { SessionDraftRows } from "./SessionDraftRows";

afterEach(cleanup);
const project: RegisteredProject = {
  project_id: "project", name: "Viewer", group_key: "/repo", folder_path: "/repo", available: true, created_at_ms: 1,
};
function draft(draft_id: string, overrides: Partial<SessionDraft> = {}): SessionDraft {
  return { draft_id, project_id: "project", selection: { provider: "pi" }, prompt: "", images: [], snapshot: null,
    sending: false, uncertain: false, locked: false, can_discard: true, error: null, image_error: null, ...overrides };
}

describe("draft sidebar identities", () => {
  it("keeps identical blank drafts independently selectable", () => {
    const select = vi.fn();
    render(<SessionDraftRows drafts={[draft("first-draft"), draft("second-draft")]} projects={[project]}
      selected_draft_id="second-draft" search="" on_select={select} />);
    const rows = screen.getAllByRole("button", { name: "New session, Pi draft in Viewer" });
    expect(rows).toHaveLength(2);
    fireEvent.click(rows[0]); expect(select).toHaveBeenLastCalledWith("first-draft");
    fireEvent.click(rows[1]); expect(select).toHaveBeenLastCalledWith("second-draft");
    expect(rows[0]).not.toHaveAttribute("aria-current"); expect(rows[1]).toHaveAttribute("aria-current", "page");
  });

  it("retains the selected draft while filtering other drafts by text, project, or agent", () => {
    const drafts = [draft("selected", { prompt: "Unrelated selected draft" }), draft("match", { prompt: "Investigate cache\nDo not show line two", selection: { provider: "codex" } }), draft("hidden")];
    const props = { drafts, projects: [project], selected_draft_id: "selected", on_select: vi.fn() };
    const { rerender } = render(<SessionDraftRows {...props} search="cache" />);
    expect(screen.getAllByRole("button")).toHaveLength(2);
    expect(screen.getByRole("button", { name: "Unrelated selected draft, Pi draft in Viewer" })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("button", { name: "Investigate cache, Codex draft in Viewer" })).toBeInTheDocument();
    expect(screen.queryByText("Do not show line two")).not.toBeInTheDocument();
    rerender(<SessionDraftRows {...props} search="codex" />);
    expect(screen.getAllByRole("button")).toHaveLength(2);
    rerender(<SessionDraftRows {...props} search="/repo" />);
    expect(screen.getAllByRole("button")).toHaveLength(3);
  });

  it("selects the draft identity even when its pending receipt knows a native key", () => {
    const select_draft = vi.fn(); const select_native = vi.fn();
    const pending = draft("draft-id", { locked: true, uncertain: true, can_discard: false, snapshot: {
      revision: 1, request_id: "creation-request", project_id: "project", provider: "pi", status: "starting",
      session_id: "native-id", session_key: "pi:native-key", session: null, pending_dialog: null, message: null,
      creation_input_state: "uncertain",
    } });
    render(<><SessionDraftRows drafts={[pending]} projects={[project]} selected_draft_id={null} search="" on_select={select_draft} />
      <button onClick={() => select_native("pi:native-key")}>Saved conversation</button></>);
    fireEvent.click(screen.getByRole("button", { name: "New session, Pi draft in Viewer" }));
    expect(select_draft).toHaveBeenCalledExactlyOnceWith("draft-id");
    expect(select_native).not.toHaveBeenCalled(); expect(screen.getByText("Check start")).toBeInTheDocument();
  });
});
