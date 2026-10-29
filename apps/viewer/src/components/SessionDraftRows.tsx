import type { SessionDraft } from "../lib/useSessionDrafts";
import type { RegisteredProject } from "../lib/types";

const AGENT_NAMES = { pi: "Pi", codex: "Codex", open_code: "OpenCode", dsh: "DSH" };

/** Draft identities never enter the historical session tree or its load APIs. */
export function SessionDraftRows({ drafts, projects, selected_draft_id, search, on_select }: {
  drafts: readonly SessionDraft[];
  projects: RegisteredProject[];
  selected_draft_id: string | null;
  search: string;
  on_select: (draft_id: string) => void;
}) {
  const query = search.trim().toLocaleLowerCase();
  const rows = drafts.map((draft) => ({ draft, project: projects.find((project) => project.project_id === draft.project_id) }))
    .filter(({ draft, project }) => draft.draft_id === selected_draft_id || !query
      || `New session ${draft.prompt} ${project?.name ?? ""} ${project?.folder_path ?? ""} ${AGENT_NAMES[draft.selection.provider]}`.toLocaleLowerCase().includes(query));
  if (!rows.length) return null;
  return <section className="session-group" aria-label="Draft sessions">
    <h2>Drafts <span className="session-group__count">{rows.length}</span></h2>
    <div className="session-group__items">
      {rows.map(({ draft, project }) => {
        const title = draft.prompt.trim().split(/\r?\n/, 1)[0] || "New session";
        const state = draft.sending ? "Starting…" : draft.locked ? "Check start" : "Draft";
        return <button key={draft.draft_id} type="button" className="session-row"
          data-selected={selected_draft_id === draft.draft_id}
          aria-current={selected_draft_id === draft.draft_id ? "page" : undefined}
          aria-label={`${title}, ${AGENT_NAMES[draft.selection.provider]} draft${project ? ` in ${project.name}` : ""}`}
          onClick={() => on_select(draft.draft_id)}>
          <span className="session-row__body">
            <span className="session-row__headline">
              <span className="session-row__title">{title}</span>
              <span className="session-row__time">{state}</span>
            </span>
            <span className="session-row__project" title={project?.folder_path}>{project?.name ?? "Choose a project"}</span>
          </span>
        </button>;
      })}
    </div>
  </section>;
}
