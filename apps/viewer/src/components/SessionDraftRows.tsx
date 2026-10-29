import { DRAFT_AGENT_NAMES, type SidebarDraftEntry } from "../lib/sidebarDrafts";

/** Draft identities never enter the historical session tree or its load APIs. */
export function SessionDraftRows({ entries, selected_draft_id, show_project = true, on_select }: {
  entries: readonly SidebarDraftEntry[];
  selected_draft_id: string | null;
  show_project?: boolean;
  on_select: (draft_id: string) => void;
}) {
  return <>{entries.map(({ draft, project }) => {
    const title = draft.prompt.trim().split(/\r?\n/, 1)[0] || "New session";
    const state = draft.sending ? "Starting…" : draft.locked ? "Check start" : "Draft";
    return <button key={draft.draft_id} type="button" className="session-row"
      data-selected={selected_draft_id === draft.draft_id}
      aria-current={selected_draft_id === draft.draft_id ? "page" : undefined}
      aria-label={`${title}, ${DRAFT_AGENT_NAMES[draft.selection.provider]} draft${project ? ` in ${project.name}` : ""}`}
      title={project?.folder_path}
      onClick={() => on_select(draft.draft_id)}>
      <span className="session-row__body">
        <span className="session-row__headline">
          <span className="session-row__title">{title}</span>
          <span className="session-row__time">{state}</span>
        </span>
        {show_project ? <span className="session-row__project" title={project?.folder_path}>{project?.name ?? "Choose a project"}</span> : null}
      </span>
    </button>;
  })}</>;
}
