import type { RegisteredProject } from "./types";
import type { SessionDraft } from "./useSessionDrafts";

export const DRAFT_AGENT_NAMES = { pi: "Pi", codex: "Codex", open_code: "OpenCode", dsh: "DSH" };

export interface SidebarDraftEntry {
  draft: SessionDraft;
  project: RegisteredProject | undefined;
}

/** Resolve exact launch folders before grouping worktrees by their project key. */
export function sidebarDraftEntries(drafts: readonly SessionDraft[], projects: RegisteredProject[], selected_draft_id: string | null, search: string): SidebarDraftEntry[] {
  const by_id = new Map(projects.map((project) => [project.project_id, project]));
  const query = search.trim().toLocaleLowerCase();
  return drafts.map((draft) => ({ draft, project: by_id.get(draft.project_id) }))
    .filter(({ draft, project }) => draft.draft_id === selected_draft_id || !query
      || `New session ${draft.prompt} ${project?.name ?? ""} ${project?.folder_path ?? ""} ${DRAFT_AGENT_NAMES[draft.selection.provider]}`.toLocaleLowerCase().includes(query));
}
