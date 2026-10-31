import { useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { DEFAULT_IMAGE_LIMITS } from "../lib/imageDrafts";
import { errorMessage } from "../lib/state";
import type { ListSessionCreationModelsRequest, RegisteredProject, RpcProvider, SessionCreationModels, SessionCreationOptions } from "../lib/types";
import type { SessionDraft, SessionDraftPatch } from "../lib/useSessionDrafts";
import { creationModelScope, useSessionCreationModels } from "../lib/useSessionCreationModels";
import { ImageAttachmentPicker, ImageAttachmentPreviews, imageDrop, imagePaste } from "./ImageAttachments";
import { PanelIcon, SendIcon } from "./Icons";
import { SessionModelPicker } from "./SessionModelPicker";
import "./DraftConversation.css";

const AGENT_NAMES: Record<RpcProvider, string> = { pi: "Pi", codex: "Codex", open_code: "OpenCode", dsh: "DSH" };
const MAX_MESSAGE_LENGTH = 16_384;

export interface DraftConversationProps {
  draft: SessionDraft;
  projects: RegisteredProject[];
  workspace_available: boolean;
  workspace_notice?: string | null;
  managed_controls?: ReactNode;
  on_change: (patch: SessionDraftPatch) => void;
  on_add_images: (files: File[]) => void;
  on_remove_image: (image_id: string) => void;
  on_send: () => void;
  on_check: () => void;
  on_discard: () => void;
  on_add_project: () => void;
  on_configure_runtime?: (trigger: HTMLButtonElement) => void;
  on_sidebar_open: () => void;
  on_load_options: (project_id: string) => Promise<SessionCreationOptions>;
  on_load_models: (request: ListSessionCreationModelsRequest) => Promise<SessionCreationModels>;
  on_open_session?: () => void;
}

/** An editable conversation without a native identity or session subscriptions. */
export function DraftConversation({ draft, projects, workspace_available, workspace_notice, managed_controls,
  on_change, on_add_images, on_remove_image, on_send, on_check, on_discard, on_add_project, on_configure_runtime,
  on_sidebar_open, on_load_options, on_load_models, on_open_session }: DraftConversationProps) {
  const input_id = useId();
  const textarea = useRef<HTMLTextAreaElement>(null);
  const [options, setOptions] = useState<SessionCreationOptions | null>(null);
  const [options_loading, setOptionsLoading] = useState(false);
  const [options_error, setOptionsError] = useState<string | null>(null);
  const [options_retry, setOptionsRetry] = useState(0);
  const project = projects.find((entry) => entry.project_id === draft.project_id);
  const provider = draft.selection.provider;
  const agent_name = AGENT_NAMES[provider];
  const runtime_id = draft.selection.runtime_profile_id ?? "";
  const agent = options?.project_id === draft.project_id ? options.providers.find((entry) => entry.provider === provider) : undefined;
  const runtime = agent?.runtimes.find((entry) => entry.profile_id === runtime_id);
  const runtime_current = !runtime_id || !!runtime && runtime.revision === draft.selection.runtime_revision;
  const agent_available = !!agent && (runtime_id ? runtime?.available === true && runtime_current : agent.available);
  const runtime_plan_id = runtime_id ? runtime?.runtime_plan_id : agent?.runtime_plan_id;
  const plan_current = (draft.selection.runtime_plan_id ?? null) === (runtime_plan_id ?? null);
  const model_request = { project_id: draft.project_id, ...draft.selection };
  const model_scope = creationModelScope(model_request, agent?.default_profile_id ?? null,
    agent?.runtimes.find((entry) => entry.profile_id === agent.default_profile_id)?.revision ?? null);
  const model = useSessionCreationModels(model_request, model_scope,
    workspace_available && !!project?.available && agent_available && plan_current && !options_loading && !options_error, on_load_models);
  const model_current = model.selectionValid(draft.model);
  const images_allowed = agent?.supports_images === true && (!draft.model || model_current && draft.model.choice.supports_images);
  const length = Array.from(draft.prompt).length;
  const over_limit = length > MAX_MESSAGE_LENGTH;
  const can_send = workspace_available && !!project?.available && agent_available && !options_loading && !options_error
    && !draft.locked && !draft.sending && !over_limit && (!!draft.prompt.trim() || !!draft.images.length)
    && plan_current && model_current && (!draft.images.length || images_allowed);
  const checking_existing = draft.locked || draft.uncertain;
  const unavailable_projects = projects.filter((entry) => !entry.available);

  useEffect(() => {
    setOptions(null); setOptionsError(null);
    if (!workspace_available || !project?.available) { setOptionsLoading(false); return; }
    let cancelled = false;
    setOptionsLoading(true);
    void Promise.resolve().then(() => on_load_options(draft.project_id)).then((result) => {
      if (result.project_id !== draft.project_id) throw new Error("The agent configuration belongs to a different project. Retry to check this folder.");
      if (!cancelled) setOptions(result);
    }).catch((failure) => { if (!cancelled) setOptionsError(errorMessage(failure)); })
      .finally(() => { if (!cancelled) setOptionsLoading(false); });
    return () => { cancelled = true; };
  }, [draft.project_id, project?.available, workspace_available, on_load_options, options_retry]);

  useEffect(() => {
    if (agent && !options_loading && !draft.locked && runtime_current && !plan_current) {
      on_change({ selection: { ...draft.selection, runtime_plan_id: runtime_plan_id ?? null } });
    }
  }, [agent, draft.locked, draft.selection, on_change, options_loading, plan_current, runtime_current, runtime_plan_id]);

  useEffect(() => {
    if (agent && !options_loading && !draft.locked && draft.model && draft.model.scope_key !== model_scope) on_change({ model: null });
  }, [agent, draft.locked, draft.model, model_scope, on_change, options_loading]);

  useLayoutEffect(() => {
    const field = textarea.current;
    if (!field) return;
    const resize = () => { field.style.height = "auto"; field.style.height = `${field.scrollHeight}px`; };
    resize();
    window.addEventListener("resize", resize);
    return () => window.removeEventListener("resize", resize);
  }, [draft.draft_id, draft.prompt]);

  useEffect(() => {
    textarea.current?.focus({ preventScroll: true });
  }, [draft.draft_id]);

  function submit() {
    if (draft.sending) return;
    if (checking_existing) on_check();
    else if (can_send) on_send();
  }

  const addImages = (files: File[]) => {
    if (!draft.locked && !draft.sending && images_allowed) on_add_images(files);
  };

  return <main className="conversation draft-conversation">
    <header className="conversation__header" data-tauri-drag-region>
      <button aria-label="Open sessions" className="icon-button mobile-sidebar-button" onClick={on_sidebar_open} type="button"><PanelIcon /></button>
      <div className="conversation__identity">
        <div className="conversation__title-row"><h2>New session</h2><span className="draft-conversation__badge">Draft</span></div>
        <p>{project?.name ?? "Choose a project"}</p>
      </div>
      <button className="draft-conversation__discard" type="button" disabled={!draft.can_discard} onClick={on_discard}>Discard draft</button>
    </header>

    <div className="draft-conversation__body">
      <div className="draft-conversation__setup">
        <div className="draft-conversation__welcome">
          <h1>Start a conversation</h1>
          <p>Your conversation starts when you send your first message.</p>
        </div>
        <div className="draft-conversation__fields">
          <label className="draft-conversation__project">Project folder
            <select value={draft.project_id} disabled={draft.locked || draft.sending} onChange={(event) => on_change({ project_id: event.target.value, selection: { provider } })}>
              <option value="" disabled>Choose a folder</option>
              {draft.project_id && !project ? <option value={draft.project_id} disabled>Unavailable folder</option> : null}
              {projects.map((entry) => <option key={entry.project_id} value={entry.project_id} disabled={!entry.available}>{entry.name} · {entry.folder_path}{entry.available ? "" : " (unavailable)"}</option>)}
            </select>
          </label>
          <label>Agent
            <select value={provider} disabled={draft.locked || draft.sending} onChange={(event) => on_change({ selection: { provider: event.target.value as RpcProvider } })}>
              {(Object.keys(AGENT_NAMES) as RpcProvider[]).map((value) => <option key={value} value={value}>{AGENT_NAMES[value]}</option>)}
            </select>
          </label>
          {!!agent?.runtimes.length || runtime_id ? <label>Runtime
            <select value={runtime_id} disabled={draft.locked || draft.sending || options_loading || !agent} onChange={(event) => {
              const selected = agent?.runtimes.find((entry) => entry.profile_id === event.target.value);
              on_change({ selection: { provider, ...(selected ? { runtime_profile_id: selected.profile_id, runtime_revision: selected.revision } : {}) } });
            }}>
              <option value="">Automatic{agent?.default_profile_id ? ` · ${agent.runtimes.find((entry) => entry.profile_id === agent.default_profile_id)?.name ?? "Default"}` : ""}</option>
              {runtime_id && !runtime ? <option value={runtime_id} disabled>Unavailable runtime</option> : null}
              {agent?.runtimes.map((entry) => <option key={entry.profile_id} value={entry.profile_id} disabled={!entry.available}>{entry.name}{entry.available ? "" : " (unavailable)"}</option>)}
            </select>
          </label> : null}
        </div>
        <div className="draft-conversation__folder-row">
          <span title={project?.folder_path}>{project?.folder_path ?? "Choose an existing folder for this session."}</span>
          <button className="text-button" type="button" disabled={draft.locked || draft.sending || !workspace_available} onClick={on_add_project}>Add project</button>
        </div>
        {!workspace_available ? <p className="draft-conversation__notice" role="status">{workspace_notice ?? "Local session creation is currently unavailable."}</p> : null}
        {workspace_available && !projects.some((entry) => entry.available) ? <p className="draft-conversation__notice">No folders are available. Add an existing folder to start a session.</p> : null}
        {draft.project_id && !project ? <p className="draft-conversation__error" role="alert">This project is no longer registered. Choose another folder.</p> : null}
        {project && !project.available ? <p className="draft-conversation__error" role="alert">{project.unavailable_reason ?? "This folder is unavailable."}</p> : null}
        {unavailable_projects.length > 0 ? <details className="draft-conversation__unavailable">
          <summary>{unavailable_projects.length} unavailable {unavailable_projects.length === 1 ? "folder" : "folders"}</summary>
          <ul>{unavailable_projects.map((entry) => <li key={entry.project_id}><strong>{entry.name}</strong><span>{entry.folder_path}</span><span>{entry.unavailable_reason ?? "This folder is unavailable."}</span></li>)}</ul>
        </details> : null}
        {options_loading && !draft.locked ? <p className="draft-conversation__notice" role="status">Checking agent configuration…</p> : null}
        {options_error && !draft.locked ? <p className="draft-conversation__error" role="alert">{options_error} <button className="text-button" type="button" onClick={() => setOptionsRetry((value) => value + 1)}>Retry configuration</button></p> : null}
        {!options_loading && !options_error && options && !agent ? <p className="draft-conversation__error" role="alert">{agent_name} is unavailable on this machine.</p> : null}
        {runtime_id && agent && !runtime_current ? <p className="draft-conversation__error" role="alert">The selected runtime changed. Choose a runtime again before sending.</p> : null}
        {runtime?.reason || !runtime_id && agent?.reason ? <p className={agent_available ? "draft-conversation__notice" : "draft-conversation__error"} role={agent_available ? "status" : "alert"}>{runtime?.reason ?? agent?.reason}</p> : null}
        {provider !== "pi" && workspace_available && project?.available && options && !options_loading && !options_error && !agent_available && !draft.locked && on_configure_runtime
          ? <button className="text-button" type="button" disabled={draft.sending} onClick={(event) => on_configure_runtime(event.currentTarget)}>Configure agent runtime</button> : null}
      </div>
    </div>

    {managed_controls}
    <form className="session-composer" data-expanded="true" aria-label="Send the first message" onSubmit={(event) => { event.preventDefault(); submit(); }}
      onPaste={(event) => { if (!draft.locked && images_allowed) imagePaste(event, addImages); }}
      onDragOver={(event) => { if (!draft.locked && images_allowed && event.dataTransfer.types.includes("Files")) event.preventDefault(); }}
      onDrop={(event) => { if (!draft.locked && images_allowed) imageDrop(event, addImages); }}>
      <div className="session-composer__content">
        <label className="sr-only" htmlFor={input_id}>First message</label>
        <div className="session-composer__box">
          <ImageAttachmentPreviews images={draft.images} disabled={draft.locked || draft.sending} on_remove={on_remove_image} />
          <textarea ref={textarea} id={input_id} value={draft.prompt} rows={3} disabled={draft.locked || draft.sending}
            aria-describedby={`${input_id}-status ${input_id}-hint`} aria-invalid={over_limit || undefined}
            placeholder={`What would you like ${agent_name} to do?`} onChange={(event) => on_change({ prompt: event.target.value })}
            onKeyDown={(event) => {
              if (event.key === "Enter" && (event.metaKey || event.ctrlKey) && !event.nativeEvent.isComposing && event.keyCode !== 229) {
                event.preventDefault(); submit();
              }
            }} />
          <ImageAttachmentPicker image_count={draft.images.length} limits={images_allowed ? DEFAULT_IMAGE_LIMITS : null} disabled={draft.locked || draft.sending} on_add={addImages} />
          <div className="session-composer__footer">
            <SessionModelPicker key={model_scope} model={model} provider={agent_name} mode="creation" input_busy={draft.locked || draft.sending}
              selected_model={draft.model?.choice} selection_current={model_current} on_clear={() => on_change({ model: null })}
              on_select={(choice) => { const selected = model.selectionFor(choice); if (selected) on_change({ model: selected }); }} />
            {checking_existing ? <button className="draft-conversation__check" type="submit" disabled={draft.sending}>{draft.sending ? "Checking…" : "Check session start"}</button>
              : <button className="session-composer__send" type="submit" disabled={!can_send} aria-label={draft.sending ? "Starting session…" : "Send"} title="Send message (⌘ / Ctrl + Enter)">
                {draft.sending ? <span className="inline-spinner" aria-hidden="true" /> : <SendIcon />}
              </button>}
          </div>
        </div>
        <div className="session-composer__actions" id={`${input_id}-hint`}>
          {over_limit ? <span className="session-composer__limit">{length.toLocaleString()} / {MAX_MESSAGE_LENGTH.toLocaleString()} characters</span>
            : <span>⌘ / Ctrl + Enter to send · Draft kept until this window closes</span>}
        </div>
        <div className="draft-conversation__status" id={`${input_id}-status`} role="status">
          {draft.sending ? <p><span className="inline-spinner" />Starting {agent_name} and checking its conversation…</p>
            : draft.uncertain ? <p>The first message has not been confirmed. Check the session start before sending again.</p>
            : draft.locked && !draft.snapshot?.session ? <p>Waiting for the conversation to be saved. You can return to this draft later.</p> : null}
          {draft.snapshot?.creation_input_state === "rejected" ? <p className="draft-conversation__error">The session was created, but the first message was not accepted. Your draft is retained.</p> : null}
          {draft.snapshot?.message ? <p>{draft.snapshot.message}</p> : null}
          {draft.snapshot?.last_turn_error ? <p className="draft-conversation__error">{draft.snapshot.last_turn_error}</p> : null}
          {draft.error && draft.error !== draft.snapshot?.message ? <p className="draft-conversation__error" role="alert">{draft.error}</p> : null}
          {draft.image_error ? <p className="draft-conversation__error" role="alert">{draft.image_error}</p> : null}
          {draft.model && !model_current && !model.loading && !draft.locked ? <p className="draft-conversation__error" role="alert">The model catalog changed. Choose a model again or use runtime default before sending.</p> : null}
          {!!draft.images.length && agent && !images_allowed ? <p className="draft-conversation__error" role="alert">{draft.model ? "This model cannot accept these images. Remove them or choose a model with image support." : `${agent_name} cannot accept these images. Remove them or choose an agent with image support.`}</p> : null}
          {draft.snapshot?.session && on_open_session ? <button className="text-button" type="button" onClick={on_open_session}>Open saved conversation</button> : null}
        </div>
      </div>
    </form>
  </main>;
}
